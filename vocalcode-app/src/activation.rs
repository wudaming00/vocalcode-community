//! Server-backed activation with a cryptographically verifiable offline receipt.
//!
//! The reusable licence key is sent to the delivery Worker over TLS. The Worker
//! validates it with Keygen, activates this machine, and returns an Ed25519-signed
//! receipt containing only an opaque licence id, the device fingerprint, product,
//! entitlement, and a finite expiry. Later launches verify that receipt locally.
//! No unsigned field read from disk can grant a licence.
//!
//! Release builds must embed the matching public key through
//! `VOCALCODE_LICENSE_PUBLIC_KEY_B64` at compile time. The Worker's corresponding
//! PKCS#8 private key is the `LICENSE_SIGNING_PRIVATE_KEY` secret; it never enters
//! this repository or the client environment.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use vocalcode_core::license::{
    verify_token, verify_trial_token, verifying_key_from_base64, LicenseClaims, TrialClaims,
};

const LICENSE_FILE: &str = "vocalcode-license.json";
const LEGACY_LICENSE_FILE: &str = "vocalcode-license.legacy.json";
const TRIAL_FILE: &str = "vocalcode-trial.dat";
pub(crate) const TRUSTED_TIME_FILE: &str = "vocalcode-time-anchor.bin";
const RECORD_VERSION: u32 = 2;
const TRIAL_RECORD_VERSION: u32 = 2;
const TRUSTED_TIME_VERSION: u32 = 1;
const CLOCK_ROLLBACK_TOLERANCE_SECONDS: u64 = 300;
const REFRESH_WINDOW_SECONDS: u64 = 7 * 24 * 60 * 60;
const DEFAULT_ACTIVATION_URL: &str = "https://vocalcode-deliver.wudaming00.workers.dev/activate";
const LICENSE_LOCK_FILE: &str = ".vocalcode-license.lock";
const LICENSE_OPERATION_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LICENSE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);
const JSON_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const JSON_RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
const JSON_BODY_TIMEOUT: Duration = Duration::from_secs(10);
const JSON_GLOBAL_TIMEOUT: Duration = Duration::from_secs(45);
const JSON_RESPONSE_MAX_BYTES: u64 = 64 * 1024;
const LICENSE_STATE_MAX_BYTES: u64 = 64 * 1024;
#[cfg(any(test, not(target_os = "macos")))]
const TRUSTED_TIME_MAX_BYTES: u64 = 4 * 1024;
const LICENSE_KEY_MAX_BYTES: usize = 512;
const RECEIPT_TOKEN_MAX_BYTES: usize = 8 * 1024;
const DEVICE_ID_MAX_BYTES: usize = 512;

static LICENSE_OPERATION: Mutex<()> = Mutex::new(());

const NETWORK_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Result of a blocking network operation whose joinable owner remains
/// responsive to application shutdown. The request thread is intentionally
/// detached: it owns request data only and can neither publish runtime state
/// nor write authority-bearing files after its receiver is dropped.
pub(crate) enum CancellableRequest<T> {
    Completed(T),
    Cancelled,
}

pub(crate) fn cancellable_network_request<T, F>(
    shutdown: &AtomicBool,
    thread_name: &str,
    request: F,
) -> anyhow::Result<CancellableRequest<T>>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    if shutdown.load(Ordering::Acquire) {
        return Ok(CancellableRequest::Cancelled);
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(thread_name.to_string())
        .spawn(move || {
            // `try_send` can never make this detached request wait for an owner
            // that already cancelled and dropped its receiver.
            let _ = sender.try_send(request());
        })
        .map_err(|error| anyhow::anyhow!("could not start network request: {error}"))?;

    loop {
        if shutdown.load(Ordering::Acquire) {
            return Ok(CancellableRequest::Cancelled);
        }
        match receiver.recv_timeout(NETWORK_CANCEL_POLL_INTERVAL) {
            Ok(result) => {
                // Cancellation wins a race with completion. This is the last
                // check before the joinable owner may validate and commit.
                if shutdown.load(Ordering::Acquire) {
                    return Ok(CancellableRequest::Cancelled);
                }
                return result.map(CancellableRequest::Completed);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("network request ended without a result")
            }
        }
    }
}

/// Read one local authority-adjacent state file without trusting its metadata.
///
/// The extra byte distinguishes an exact-limit file from a truncated oversized
/// one while `take` keeps a sparse, raced, or attacker-written file from being
/// accumulated into memory without bound.
fn read_small_file(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let probe_bytes = max_bytes.checked_add(1).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid file limit")
    })?;
    let mut bytes = Vec::new();
    File::open(path)?
        .take(probe_bytes)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("state file exceeds the {max_bytes}-byte safety limit"),
        ));
    }
    Ok(bytes)
}

fn read_small_utf8_file(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    String::from_utf8(read_small_file(path, max_bytes)?).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "state file is not valid UTF-8",
        )
    })
}

struct LicenseOperationGuard<'a> {
    _local: MutexGuard<'a, ()>,
    file: File,
}

impl Drop for LicenseOperationGuard<'_> {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// Serialize read→network→replace as one transaction, both between GUI
/// threads and against a concurrently invoked `vocalcode-app activate` CLI.
/// Unique temp names prevent corruption; this lock also prevents an older
/// refresh response from overwriting a newer manual activation receipt.
fn license_operation(base: &Path) -> anyhow::Result<LicenseOperationGuard<'static>> {
    license_operation_with_timeout(base, &LICENSE_OPERATION, LICENSE_OPERATION_LOCK_TIMEOUT)
}

fn license_operation_cancellable(
    base: &Path,
    shutdown: &AtomicBool,
) -> anyhow::Result<Option<LicenseOperationGuard<'static>>> {
    license_operation_with_timeout_and_cancel(
        base,
        &LICENSE_OPERATION,
        LICENSE_OPERATION_LOCK_TIMEOUT,
        || shutdown.load(Ordering::Acquire),
    )
}

fn license_file_lock_is_contended(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    if error.raw_os_error() == Some(33) {
        // LockFileEx reports ERROR_LOCK_VIOLATION as ErrorKind::Other.
        return true;
    }
    #[cfg(target_os = "linux")]
    if error.raw_os_error() == Some(11) {
        return true;
    }
    #[cfg(target_os = "macos")]
    if error.raw_os_error() == Some(35) {
        return true;
    }
    false
}

fn licence_lock_timeout(stage: &str, timeout: Duration) -> anyhow::Error {
    anyhow::anyhow!("licence operation is busy: timed out waiting for {stage} after {timeout:?}")
}

/// Acquire both serialization layers under one monotonic deadline. A suspended
/// GUI/CLI process can therefore delay another operation, but can never make a
/// caller (or the shutdown join waiting for it) block forever.
fn license_operation_with_timeout<'a>(
    base: &Path,
    local_mutex: &'a Mutex<()>,
    timeout: Duration,
) -> anyhow::Result<LicenseOperationGuard<'a>> {
    license_operation_with_timeout_and_cancel(base, local_mutex, timeout, || false)?.ok_or_else(
        || anyhow::anyhow!("licence operation cancelled without a cancellation request"),
    )
}

fn license_operation_with_timeout_and_cancel<'a, C>(
    base: &Path,
    local_mutex: &'a Mutex<()>,
    timeout: Duration,
    should_cancel: C,
) -> anyhow::Result<Option<LicenseOperationGuard<'a>>>
where
    C: Fn() -> bool,
{
    license_operation_with_clock(
        base,
        local_mutex,
        timeout,
        should_cancel,
        Instant::now,
        std::thread::sleep,
    )
}

// Production always uses the monotonic clock above. Injecting clock/wait here
// lets tests verify a shared deadline without depending on runner scheduling.
fn license_operation_with_clock<'a, C, N, W>(
    base: &Path,
    local_mutex: &'a Mutex<()>,
    timeout: Duration,
    should_cancel: C,
    now: N,
    wait: W,
) -> anyhow::Result<Option<LicenseOperationGuard<'a>>>
where
    C: Fn() -> bool,
    N: Fn() -> Instant,
    W: Fn(Duration),
{
    if timeout.is_zero() {
        anyhow::bail!("licence operation lock timeout must be non-zero");
    }
    let deadline = now()
        .checked_add(timeout)
        .ok_or_else(|| anyhow::anyhow!("licence operation lock deadline overflow"))?;

    let local = loop {
        if should_cancel() {
            return Ok(None);
        }
        match local_mutex.try_lock() {
            Ok(guard) => break guard,
            Err(TryLockError::Poisoned(_)) => {
                anyhow::bail!("licence operation lock was poisoned")
            }
            Err(TryLockError::WouldBlock) => {
                let remaining = deadline.saturating_duration_since(now());
                if remaining.is_zero() {
                    return Err(licence_lock_timeout("in-process lock", timeout));
                }
                wait(LICENSE_LOCK_POLL_INTERVAL.min(remaining));
            }
        }
    };
    if should_cancel() {
        return Ok(None);
    }
    if now() >= deadline {
        return Err(licence_lock_timeout("in-process lock", timeout));
    }

    std::fs::create_dir_all(base)?;
    if now() >= deadline {
        return Err(licence_lock_timeout("cross-process file lock", timeout));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(base.join(LICENSE_LOCK_FILE))?;
    loop {
        if should_cancel() {
            return Ok(None);
        }
        if now() >= deadline {
            return Err(licence_lock_timeout("cross-process file lock", timeout));
        }
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => break,
            Err(error) if license_file_lock_is_contended(&error) => {
                let remaining = deadline.saturating_duration_since(now());
                if remaining.is_zero() {
                    return Err(licence_lock_timeout("cross-process file lock", timeout));
                }
                wait(LICENSE_LOCK_POLL_INTERVAL.min(remaining));
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "could not acquire licence operation file lock: {error}"
                ))
            }
        }
    }
    if should_cancel() {
        let _ = fs2::FileExt::unlock(&file);
        return Ok(None);
    }
    if now() >= deadline {
        let _ = fs2::FileExt::unlock(&file);
        return Err(licence_lock_timeout("cross-process file lock", timeout));
    }
    Ok(Some(LicenseOperationGuard {
        _local: local,
        file,
    }))
}

// A paid build that silently omits this value would compile successfully but
// reject every genuine receipt at runtime. Make that a release-build failure.
#[cfg(all(not(debug_assertions), not(feature = "community")))]
const _: () = {
    if option_env!("VOCALCODE_LICENSE_PUBLIC_KEY_B64").is_none() {
        panic!("release builds require VOCALCODE_LICENSE_PUBLIC_KEY_B64");
    }
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LicenseRecord {
    version: u32,
    receipt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrialRecord {
    version: u32,
    receipt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptInstallSource {
    ExplicitActivation,
    LegacyMigration,
    CachedRefresh,
    Checkout,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TrustedTimeRecord {
    version: u32,
    /// Greatest authority-bearing Unix time observed or derived from elapsed
    /// system uptime. This value is monotonic across ordinary app restarts.
    trusted_unix: u64,
    /// OS uptime at `trusted_unix`. Unlike `Instant`, this survives process
    /// restarts; a decrease identifies a machine reboot.
    uptime_millis: Option<u64>,
}

/// Shape written by releases before signed receipts. It is parsed only so it can
/// be preserved for manual recovery; it is never accepted as proof of purchase.
#[derive(Debug, Clone, Deserialize)]
struct LegacyLicenseRecord {
    key: String,
    #[serde(rename = "device")]
    _device: String,
}

#[derive(Debug, Deserialize)]
struct ActivationResponse {
    token: Option<String>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(windows)]
fn system_uptime_millis() -> Option<u64> {
    Some(unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() })
}

#[cfg(windows)]
fn protect_trusted_time(plaintext: &[u8], protect: bool) -> anyhow::Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    const ENTROPY: &[u8] = b"VocalCode trusted-time anchor v1";
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(plaintext.len())?,
        pbData: plaintext.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(ENTROPY.len())?,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        if protect {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let result = unsafe {
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(output.pbData.cast());
        bytes
    };
    Ok(result)
}

#[cfg(windows)]
fn read_trusted_time_bytes(base: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match read_small_file(&base.join(TRUSTED_TIME_FILE), TRUSTED_TIME_MAX_BYTES) {
        Ok(ciphertext) => protect_trusted_time(&ciphertext, false)
            .map(Some)
            .map_err(|error| anyhow::anyhow!("trusted-time anchor authentication failed: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn write_trusted_time_bytes(base: &Path, plaintext: &[u8]) -> anyhow::Result<()> {
    let ciphertext = protect_trusted_time(plaintext, true)?;
    crate::storage::atomic_write(&base.join(TRUSTED_TIME_FILE), ciphertext).map_err(Into::into)
}

#[cfg(all(target_os = "macos", not(test)))]
fn read_trusted_time_bytes(_base: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    const ITEM_NOT_FOUND: i32 = -25_300;
    match security_framework::passwords::get_generic_password(
        "app.vocalcode.trusted-time",
        "high-water-v1",
    ) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
        Err(error) => Err(anyhow::anyhow!("read trusted time from Keychain: {error}")),
    }
}

#[cfg(all(target_os = "macos", not(test)))]
fn write_trusted_time_bytes(_base: &Path, plaintext: &[u8]) -> anyhow::Result<()> {
    security_framework::passwords::set_generic_password(
        "app.vocalcode.trusted-time",
        "high-water-v1",
        plaintext,
    )
    .map_err(|error| anyhow::anyhow!("write trusted time to Keychain: {error}"))
}

// Unit tests must never touch the developer's real login Keychain. Linux is
// not shipped; its file backend exists solely to exercise portable logic.
#[cfg(any(all(target_os = "macos", test), not(any(target_os = "macos", windows))))]
fn read_trusted_time_bytes(base: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match read_small_file(&base.join(TRUSTED_TIME_FILE), TRUSTED_TIME_MAX_BYTES) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(any(all(target_os = "macos", test), not(any(target_os = "macos", windows))))]
fn write_trusted_time_bytes(base: &Path, plaintext: &[u8]) -> anyhow::Result<()> {
    crate::storage::atomic_write(&base.join(TRUSTED_TIME_FILE), plaintext).map_err(Into::into)
}

#[cfg(target_os = "macos")]
fn system_uptime_millis() -> Option<u64> {
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }
    extern "C" {
        fn mach_continuous_time() -> u64;
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }
    let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
    let ticks = unsafe { mach_continuous_time() };
    if unsafe { mach_timebase_info(&mut info) } != 0 || info.denom == 0 {
        return None;
    }
    let nanos = (ticks as u128)
        .checked_mul(info.numer as u128)?
        .checked_div(info.denom as u128)?;
    u64::try_from(nanos / 1_000_000).ok()
}

#[cfg(not(any(target_os = "macos", windows)))]
fn system_uptime_millis() -> Option<u64> {
    // Linux is not a shipped target. Refusing to invent a process-local value
    // keeps the persisted format honest for future platform work.
    None
}

/// Advance and persist the local trusted-time high-water mark. The wall clock
/// alone cannot enforce a finite receipt: moving it backwards and freezing it
/// would otherwise make both paid and trial tokens live forever. Windows boot
/// tick count and macOS continuous time keep elapsed time moving across process
/// restarts on the same boot; after a reboot, a wall clock that did not advance
/// is rejected until corrected.
///
/// `signed_floor` is an authenticated issuance epoch (currently the trial
/// receipt's `iat`). It can advance the high-water mark but can never make a
/// materially older wall clock trustworthy.
fn trusted_now_with(
    base: &Path,
    wall_now: u64,
    uptime_millis: Option<u64>,
    signed_floor: u64,
    require_existing: bool,
) -> anyhow::Result<u64> {
    if wall_now == 0 {
        anyhow::bail!("system clock is unavailable");
    }
    let previous = match read_trusted_time_bytes(base)? {
        Some(source) => {
            let record: TrustedTimeRecord = serde_json::from_slice(&source)
                .map_err(|error| anyhow::anyhow!("trusted-time anchor is malformed: {error}"))?;
            if record.version != TRUSTED_TIME_VERSION || record.trusted_unix == 0 {
                anyhow::bail!("trusted-time anchor has an unsupported version or value");
            }
            Some(record)
        }
        None if require_existing => {
            anyhow::bail!("trusted-time anchor is missing; connect once to repair it")
        }
        None => None,
    };

    let mut rollback = signed_floor.saturating_sub(wall_now) > CLOCK_ROLLBACK_TOLERANCE_SECONDS;
    let (trusted_unix, recorded_uptime) = match previous {
        None => (wall_now.max(signed_floor), uptime_millis),
        Some(record) => match (record.uptime_millis, uptime_millis) {
            (Some(old_uptime), Some(current_uptime)) if current_uptime >= old_uptime => {
                let elapsed = (current_uptime - old_uptime) / 1_000;
                let monotonic = record.trusted_unix.saturating_add(elapsed);
                if monotonic.saturating_sub(wall_now) > CLOCK_ROLLBACK_TOLERANCE_SECONDS {
                    rollback = true;
                }
                let trusted = monotonic.max(wall_now).max(signed_floor);
                // Do not discard a sub-second remainder merely because another
                // thread asked for status; retain the old reference until the
                // trusted whole-second value actually advances.
                let reference = if trusted > record.trusted_unix {
                    Some(current_uptime)
                } else {
                    Some(old_uptime)
                };
                (trusted, reference)
            }
            (Some(_), Some(current_uptime)) => {
                // Uptime moved backwards, so the machine rebooted. A frozen or
                // rolled-back wall clock must not let each reboot reset elapsed
                // time. Requiring actual forward wall progress is fail-closed.
                if wall_now <= record.trusted_unix {
                    rollback = true;
                }
                (
                    record.trusted_unix.max(wall_now).max(signed_floor),
                    Some(current_uptime),
                )
            }
            _ => {
                if record.trusted_unix.saturating_sub(wall_now) > CLOCK_ROLLBACK_TOLERANCE_SECONDS {
                    rollback = true;
                }
                (
                    record.trusted_unix.max(wall_now).max(signed_floor),
                    uptime_millis,
                )
            }
        },
    };

    let record = TrustedTimeRecord {
        version: TRUSTED_TIME_VERSION,
        trusted_unix,
        uptime_millis: recorded_uptime,
    };
    std::fs::create_dir_all(base)?;
    write_trusted_time_bytes(base, &serde_json::to_vec(&record)?)?;
    if rollback {
        anyhow::bail!("system clock moved backwards or stopped relative to trusted elapsed time");
    }
    Ok(trusted_unix)
}

fn trusted_now(base: &Path, signed_floor: u64, require_existing: bool) -> anyhow::Result<u64> {
    trusted_now_with(
        base,
        now_unix(),
        system_uptime_millis(),
        signed_floor,
        require_existing,
    )
}

/// Establish or repair the protected high-water mark only after a freshly
/// received receipt has authenticated `server_time`. Missing, malformed, or
/// edited app-data bytes can therefore cause a safe online repair but can never
/// authorize a cached receipt using an attacker-chosen wall clock.
fn install_online_time_anchor_with(
    base: &Path,
    server_time: u64,
    wall_now: u64,
    uptime_millis: Option<u64>,
) -> anyhow::Result<u64> {
    if server_time == 0 || server_time.abs_diff(wall_now) > CLOCK_ROLLBACK_TOLERANCE_SECONDS {
        anyhow::bail!("system clock materially differs from authenticated server time");
    }
    let previous = read_trusted_time_bytes(base)
        .ok()
        .flatten()
        .and_then(|source| serde_json::from_slice::<TrustedTimeRecord>(&source).ok())
        .filter(|record| record.version == TRUSTED_TIME_VERSION && record.trusted_unix > 0);
    let previous_unix = previous.map(|record| record.trusted_unix).unwrap_or(0);
    let trusted_unix = if previous_unix.saturating_sub(server_time)
        > CLOCK_ROLLBACK_TOLERANCE_SECONDS
    {
        // A accidentally-future wall clock may have advanced the local anchor
        // years ahead. A fresh signed server time is the only authority allowed
        // to repair that poison; cached receipts never enter this function.
        log::warn!(
            "resetting future trusted-time anchor {previous_unix} to authenticated server time {server_time}"
        );
        server_time.max(wall_now)
    } else {
        previous_unix.max(server_time).max(wall_now)
    };
    let record = TrustedTimeRecord {
        version: TRUSTED_TIME_VERSION,
        trusted_unix,
        uptime_millis,
    };
    std::fs::create_dir_all(base)?;
    write_trusted_time_bytes(base, &serde_json::to_vec(&record)?)?;
    Ok(trusted_unix)
}

fn install_online_time_anchor(base: &Path, server_time: u64) -> anyhow::Result<u64> {
    install_online_time_anchor_with(base, server_time, now_unix(), system_uptime_millis())
}

fn embedded_verifying_key() -> anyhow::Result<[u8; 32]> {
    let encoded = option_env!("VOCALCODE_LICENSE_PUBLIC_KEY_B64").ok_or_else(|| {
        anyhow::anyhow!(
            "this build has no licence receipt public key; rebuild with \
             VOCALCODE_LICENSE_PUBLIC_KEY_B64 set"
        )
    })?;
    verifying_key_from_base64(encoded).map_err(|e| anyhow::anyhow!(e.to_string()))
}

fn usable_device(device: &str) -> bool {
    let device = device.trim();
    !device.is_empty()
        && device.len() <= DEVICE_ID_MAX_BYTES
        && device != "vocalcode-unknown-device"
        && !device.chars().any(char::is_control)
}

fn endpoint_url_from_override(
    operation: &str,
    configured_override: Option<&str>,
    allow_override: bool,
) -> anyhow::Result<String> {
    let configured = if allow_override {
        configured_override.unwrap_or(DEFAULT_ACTIVATION_URL)
    } else {
        DEFAULT_ACTIVATION_URL
    };
    let configured = configured.trim_end_matches('/');
    if !configured.starts_with("https://") {
        anyhow::bail!("activation endpoint must use https");
    }
    if configured.contains('?') || configured.contains('#') || !configured.ends_with("/activate") {
        anyhow::bail!("activation endpoint must be an https URL ending in /activate");
    }
    if operation == "activate" {
        return Ok(configured.to_string());
    }
    if operation != "refresh" && operation != "trial" {
        anyhow::bail!("unsupported activation operation");
    }
    let base = configured.strip_suffix("/activate").ok_or_else(|| {
        anyhow::anyhow!("VOCALCODE_ACTIVATION_URL must end in /activate to use receipt refresh")
    })?;
    Ok(format!("{base}/{operation}"))
}

fn endpoint_url(operation: &str) -> anyhow::Result<String> {
    // An environment variable is useful for local integration tests, but a
    // production process inherits environment state from launchers, shells and
    // service managers that are outside the app's trust boundary. Never let
    // such state redirect a reusable paid key away from the official Worker.
    #[cfg(debug_assertions)]
    {
        let configured = std::env::var("VOCALCODE_ACTIVATION_URL").ok();
        endpoint_url_from_override(operation, configured.as_deref(), true)
    }
    #[cfg(not(debug_assertions))]
    {
        if std::env::var_os("VOCALCODE_ACTIVATION_URL").is_some() {
            log::warn!("VOCALCODE_ACTIVATION_URL is ignored in production builds");
        }
        endpoint_url_from_override(operation, None, false)
    }
}

fn usable_license_key(key: &str) -> bool {
    let key = key.trim();
    !key.is_empty() && key.len() <= LICENSE_KEY_MAX_BYTES && !key.chars().any(char::is_control)
}

fn usable_receipt_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= RECEIPT_TOKEN_MAX_BYTES
}

#[derive(Debug)]
pub enum TrialReceiptStatus {
    Active(TrialClaims),
    Expired,
    SetupRequired,
    Invalid(String),
}

/// Verify the cached server-owned trial epoch locally. Missing, legacy numeric,
/// malformed and wrong-device data require one online repair; an authentic
/// expired receipt remains distinctly expired and can never restart.
pub fn trial_status(base: &Path, device: &str, now: u64) -> TrialReceiptStatus {
    let _guard = match license_operation(base) {
        Ok(guard) => guard,
        Err(error) => return TrialReceiptStatus::Invalid(error.to_string()),
    };
    if !usable_device(device) {
        return TrialReceiptStatus::Invalid(
            "a stable machine fingerprint is unavailable on this device".into(),
        );
    }
    let key = match embedded_verifying_key() {
        Ok(key) => key,
        Err(error) => return TrialReceiptStatus::Invalid(error.to_string()),
    };
    trial_status_with_key(base, device, now, &key)
}

fn trial_status_with_key(
    base: &Path,
    device: &str,
    now: u64,
    key: &[u8; 32],
) -> TrialReceiptStatus {
    let source = match read_small_utf8_file(&base.join(TRIAL_FILE), LICENSE_STATE_MAX_BYTES) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return TrialReceiptStatus::SetupRequired;
        }
        Err(error) => return TrialReceiptStatus::Invalid(error.to_string()),
    };
    let record: TrialRecord = match serde_json::from_str(&source) {
        Ok(record) => record,
        Err(_) => return TrialReceiptStatus::SetupRequired,
    };
    if record.version != TRIAL_RECORD_VERSION || !usable_receipt_token(&record.receipt) {
        return TrialReceiptStatus::SetupRequired;
    }
    let claims = match verify_trial_token(&record.receipt, key, device, 0) {
        Ok(claims) => claims,
        Err(_) => return TrialReceiptStatus::SetupRequired,
    };
    let now = match trusted_now_with(base, now, system_uptime_millis(), claims.iat, true) {
        Ok(now) => now,
        Err(error) => return TrialReceiptStatus::Invalid(error.to_string()),
    };
    if now >= claims.exp {
        TrialReceiptStatus::Expired
    } else {
        TrialReceiptStatus::Active(claims)
    }
}

/// Provision an authoritative trial epoch once per stable device. An authentic
/// cached receipt, including an expired one, never causes a new issuance call;
/// deletion/tampering asks the Worker, whose Durable Object returns the same
/// original epoch.
/// Cancellation-aware GUI/maintenance variant. Network I/O happens in a
/// request-only detached thread; receipt verification, trusted-time repair,
/// and disk writes remain in this joinable caller and are skipped once
/// shutdown is observed.
pub(crate) fn refresh_trial_receipt_cancellable(
    base: &Path,
    device: &str,
    shutdown: &AtomicBool,
) -> anyhow::Result<Option<bool>> {
    if shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    let Some(_guard) = license_operation_cancellable(base, shutdown)? else {
        return Ok(None);
    };
    if shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    if !usable_device(device) {
        anyhow::bail!("a stable machine fingerprint is unavailable on this device");
    }
    let key = embedded_verifying_key()?;
    let mut cancelled = false;
    let result = refresh_trial_receipt_with_commit_check(
        base,
        device,
        &key,
        now_unix(),
        system_uptime_millis(),
        || {
            let url = endpoint_url("trial")?;
            let body = serde_json::json!({ "device": device }).to_string();
            match request_receipt_cancellable(url, body, shutdown)? {
                CancellableRequest::Completed(receipt) => Ok(receipt),
                CancellableRequest::Cancelled => {
                    cancelled = true;
                    anyhow::bail!("activation request cancelled")
                }
            }
        },
        || !shutdown.load(Ordering::Acquire),
    );
    if cancelled || shutdown.load(Ordering::Acquire) {
        Ok(None)
    } else {
        result.map(Some)
    }
}

#[cfg(test)]
fn refresh_trial_receipt_with<F>(
    base: &Path,
    device: &str,
    verifying_key: &[u8; 32],
    wall_now: u64,
    uptime_millis: Option<u64>,
    request: F,
) -> anyhow::Result<bool>
where
    F: FnOnce() -> anyhow::Result<String>,
{
    refresh_trial_receipt_with_commit_check(
        base,
        device,
        verifying_key,
        wall_now,
        uptime_millis,
        request,
        || true,
    )
}

fn refresh_trial_receipt_with_commit_check<F, C>(
    base: &Path,
    device: &str,
    verifying_key: &[u8; 32],
    wall_now: u64,
    uptime_millis: Option<u64>,
    request: F,
    may_commit: C,
) -> anyhow::Result<bool>
where
    F: FnOnce() -> anyhow::Result<String>,
    C: Fn() -> bool,
{
    let mut existing_claims = None;
    if let Ok(source) = read_small_utf8_file(&base.join(TRIAL_FILE), LICENSE_STATE_MAX_BYTES) {
        if let Ok(record) = serde_json::from_str::<TrialRecord>(&source) {
            if record.version == TRIAL_RECORD_VERSION && usable_receipt_token(&record.receipt) {
                if let Ok(claims) = verify_trial_token(&record.receipt, verifying_key, device, 0) {
                    existing_claims = Some(claims.clone());
                    if trusted_now_with(base, wall_now, uptime_millis, claims.checked_at, true)
                        .is_ok()
                    {
                        return Ok(false);
                    }
                }
            }
        }
    }
    let receipt = request()?;
    let incoming = verify_trial_token(&receipt, verifying_key, device, 0)
        .map_err(|error| anyhow::anyhow!("trial receipt rejected: {error}"))?;
    if let Some(existing) = existing_claims {
        if incoming.iat != existing.iat || incoming.exp != existing.exp {
            anyhow::bail!("trial repair changed the authenticated first-seen epoch");
        }
    }
    if !may_commit() {
        anyhow::bail!("activation request cancelled");
    }
    install_online_time_anchor_with(base, incoming.checked_at, wall_now, uptime_millis)?;
    save_trial_receipt(base, &receipt)?;
    Ok(true)
}

/// Offline check: verify the cached server receipt against the embedded public
/// key, current machine, product id, entitlement, and expiry.
pub fn license_entitlement(base: &Path, device: &str) -> Option<u32> {
    let _guard = license_operation(base).ok()?;
    license_entitlement_unlocked(base, device)
}

fn license_entitlement_unlocked(base: &Path, device: &str) -> Option<u32> {
    if !usable_device(device) {
        log::warn!("licence receipt refused: stable device id unavailable");
        return None;
    }
    let path = base.join(LICENSE_FILE);
    let Ok(s) = read_small_utf8_file(&path, LICENSE_STATE_MAX_BYTES) else {
        return None;
    };

    if let Ok(rec) = serde_json::from_str::<LicenseRecord>(&s) {
        if rec.version != RECORD_VERSION || !usable_receipt_token(&rec.receipt) {
            return None;
        }
        let Ok(key) = embedded_verifying_key() else {
            log::error!("licence receipt cannot be verified: release public key is missing");
            return None;
        };
        return paid_entitlement_from_record(
            base,
            &rec,
            &key,
            device,
            now_unix(),
            system_uptime_millis(),
        );
    }

    // A copied two-field JSON file used to grant a permanent licence. Preserve a
    // genuine old key so its owner can reactivate, but remove it from the active
    // filename and never trust it offline.
    if let Ok(legacy) = serde_json::from_str::<LegacyLicenseRecord>(&s) {
        let backup = base.join(LEGACY_LICENSE_FILE);
        log::warn!("legacy unsigned licence cache is not trusted; reactivation is required");
        if !legacy.key.is_empty() && !backup.exists() {
            if let Err(e) = std::fs::rename(&path, &backup) {
                log::warn!("could not preserve legacy licence cache: {e}");
            }
        }
    }
    None
}

fn paid_entitlement_from_record(
    base: &Path,
    record: &LicenseRecord,
    key: &[u8; 32],
    device: &str,
    wall_now: u64,
    uptime_millis: Option<u64>,
) -> Option<u32> {
    let authenticated = match verify_token(&record.receipt, key, device, 0) {
        Ok(claims) => claims,
        Err(e) => {
            log::warn!("cached licence receipt rejected: {e}");
            return None;
        }
    };
    let now = match trusted_now_with(base, wall_now, uptime_millis, authenticated.iat, true) {
        Ok(now) => now,
        Err(error) => {
            log::warn!("cached licence receipt rejected: {error}");
            return None;
        }
    };
    match verify_token(&record.receipt, key, device, now) {
        Ok(claims) => Some(claims.max_version),
        Err(e) => {
            log::warn!("cached licence receipt rejected: {e}");
            None
        }
    }
}

/// Upgrade a pre-receipt cache without ever trusting it locally.
///
/// The old reusable key is first preserved under the legacy filename, then sent
/// through the normal online activation flow. Only the Worker's signed receipt
/// can make the device licensed. A network/server/signature failure leaves the
/// legacy backup in place and returns an error, so the caller must continue as
/// unlicensed and can retry on a later launch.
pub(crate) fn refresh_legacy_receipt_cancellable(
    base: &Path,
    shutdown: &AtomicBool,
) -> anyhow::Result<Option<bool>> {
    if shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    let Some(_guard) = license_operation_cancellable(base, shutdown)? else {
        return Ok(None);
    };
    if shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    let device = vocalcode_platform::device_id();
    if !usable_device(&device) {
        anyhow::bail!("a stable machine fingerprint is unavailable on this device");
    }
    let verifying_key = embedded_verifying_key()?;
    let mut cancelled = false;
    let result = refresh_legacy_receipt_with_authenticated(base, &verifying_key, &device, |key| {
        match activate_unlocked_for_cancellable(
            key,
            base,
            ReceiptInstallSource::LegacyMigration,
            shutdown,
        )? {
            Some(()) => Ok(()),
            None => {
                cancelled = true;
                anyhow::bail!("activation request cancelled")
            }
        }
    });
    if cancelled || shutdown.load(Ordering::Acquire) {
        Ok(None)
    } else {
        result.map(Some)
    }
}

/// Refresh a signed receipt before it expires, without retaining or asking for
/// the reusable license key. A currently valid receipt with more than seven
/// days remaining does no network work. Expired receipts may identify the
/// license to the Worker, but only a new server signature after live Keygen
/// validation can restore authorization.
pub(crate) fn refresh_cached_receipt_cancellable(
    base: &Path,
    device: &str,
    shutdown: &AtomicBool,
) -> anyhow::Result<Option<bool>> {
    if shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    let Some(_guard) = license_operation_cancellable(base, shutdown)? else {
        return Ok(None);
    };
    if shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    refresh_cached_receipt_unlocked_cancellable(base, device, shutdown)
}

fn refresh_cached_receipt_unlocked_cancellable(
    base: &Path,
    device: &str,
    shutdown: &AtomicBool,
) -> anyhow::Result<Option<bool>> {
    if !usable_device(device) {
        anyhow::bail!("a stable machine fingerprint is unavailable on this device");
    }
    let source = match read_small_utf8_file(&base.join(LICENSE_FILE), LICENSE_STATE_MAX_BYTES) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Some(false)),
        Err(error) => return Err(error.into()),
    };
    let record: LicenseRecord = match serde_json::from_str::<LicenseRecord>(&source) {
        Ok(record) if record.version == RECORD_VERSION && usable_receipt_token(&record.receipt) => {
            record
        }
        _ => return Ok(Some(false)),
    };
    let verifying_key = embedded_verifying_key()?;
    let claims = verify_token(&record.receipt, &verifying_key, device, 0)
        .map_err(|error| anyhow::anyhow!("cached receipt cannot be refreshed: {error}"))?;
    if let Ok(now) = trusted_now(base, 0, true) {
        if claims.exp.saturating_sub(now) > REFRESH_WINDOW_SECONDS {
            return Ok(Some(false));
        }
    }

    let url = endpoint_url("refresh")?;
    let body = serde_json::json!({ "token": record.receipt, "device": device }).to_string();
    let receipt = match request_receipt_cancellable(url, body, shutdown)? {
        CancellableRequest::Completed(receipt) => receipt,
        CancellableRequest::Cancelled => return Ok(None),
    };
    if shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    install_online_receipt(
        base,
        &receipt,
        &verifying_key,
        device,
        ReceiptInstallSource::CachedRefresh,
    )?;
    Ok(Some(true))
}

fn validate_refresh_replacement(
    existing: &LicenseClaims,
    refreshed: &LicenseClaims,
) -> anyhow::Result<()> {
    if refreshed.key != existing.key {
        anyhow::bail!("refreshed receipt identifies a different licence");
    }
    if refreshed.max_version < existing.max_version {
        anyhow::bail!("refreshed receipt has a lower product entitlement");
    }
    if refreshed.exp <= existing.exp {
        anyhow::bail!("refreshed receipt does not extend the authenticated receipt");
    }
    Ok(())
}

fn refresh_legacy_receipt_with<F>(base: &Path, refresh: F) -> anyhow::Result<bool>
where
    F: FnOnce(&str) -> anyhow::Result<()>,
{
    let active = base.join(LICENSE_FILE);
    let backup = base.join(LEGACY_LICENSE_FILE);

    let active_legacy = read_legacy_record(&active);
    let legacy = active_legacy
        .clone()
        .or_else(|| read_legacy_record(&backup));
    let Some(legacy) = legacy else {
        return Ok(false);
    };
    let key = legacy.key.trim();
    if key.is_empty() {
        return Ok(false);
    }

    // When the old record is still at the active path, move it out of the way
    // before doing any network work. If activation fails, the reusable key is
    // still recoverable but it can never be mistaken for an active receipt.
    if active_legacy.is_some() && !backup.exists() {
        std::fs::rename(&active, &backup)
            .map_err(|e| anyhow::anyhow!("preserve legacy licence before refresh: {e}"))?;
    }

    refresh(key)?;
    if backup.exists() {
        if let Err(e) = std::fs::remove_file(&backup) {
            log::warn!("signed receipt installed, but legacy key backup could not be removed: {e}");
        }
    }
    Ok(true)
}

fn refresh_legacy_receipt_with_authenticated<F>(
    base: &Path,
    verifying_key: &[u8; 32],
    device: &str,
    refresh: F,
) -> anyhow::Result<bool>
where
    F: FnOnce(&str) -> anyhow::Result<()>,
{
    // An authenticated receipt at the active path always wins over an unsigned
    // migration backup, regardless of local expiry. Maintenance will refresh
    // that exact licence online; it must never silently activate backup key A
    // over signed licence B.
    if read_authenticated_receipt(base, verifying_key, device)?.is_some() {
        return Ok(false);
    }
    refresh_legacy_receipt_with(base, refresh)
}

fn read_legacy_record(path: &Path) -> Option<LegacyLicenseRecord> {
    let source = read_small_utf8_file(path, LICENSE_STATE_MAX_BYTES).ok()?;
    serde_json::from_str(&source).ok()
}

/// Validate a licence key, activate this machine, require a signed receipt, and
/// cache only that receipt. A server response without a token fails closed.
pub fn activate(key: &str, base: &Path) -> anyhow::Result<()> {
    let _guard = license_operation(base)?;
    activate_unlocked_for(key, base, ReceiptInstallSource::ExplicitActivation)
}

pub(crate) fn activate_cancellable(
    key: &str,
    base: &Path,
    shutdown: &AtomicBool,
) -> anyhow::Result<Option<()>> {
    if shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    let Some(_guard) = license_operation_cancellable(base, shutdown)? else {
        return Ok(None);
    };
    if shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    activate_unlocked_for_cancellable(
        key,
        base,
        ReceiptInstallSource::ExplicitActivation,
        shutdown,
    )
}

fn activate_unlocked_for(
    key: &str,
    base: &Path,
    source: ReceiptInstallSource,
) -> anyhow::Result<()> {
    let key = key.trim();
    if !usable_license_key(key) {
        anyhow::bail!("license key is empty, too long, or malformed");
    }
    let device = vocalcode_platform::device_id();
    if !usable_device(&device) {
        anyhow::bail!("a stable machine fingerprint is unavailable on this device");
    }
    let verifying_key = embedded_verifying_key()?;
    let url = endpoint_url("activate")?;

    let body = serde_json::json!({ "key": key, "device": device }).to_string();
    let receipt = request_receipt(&url, body)?;
    install_online_receipt(base, &receipt, &verifying_key, &device, source)?;
    Ok(())
}

fn activate_unlocked_for_cancellable(
    key: &str,
    base: &Path,
    source: ReceiptInstallSource,
    shutdown: &AtomicBool,
) -> anyhow::Result<Option<()>> {
    let key = key.trim();
    if !usable_license_key(key) {
        anyhow::bail!("license key is empty, too long, or malformed");
    }
    let device = vocalcode_platform::device_id();
    if !usable_device(&device) {
        anyhow::bail!("a stable machine fingerprint is unavailable on this device");
    }
    let verifying_key = embedded_verifying_key()?;
    let url = endpoint_url("activate")?;
    let body = serde_json::json!({ "key": key, "device": device }).to_string();
    let receipt = match request_receipt_cancellable(url, body, shutdown)? {
        CancellableRequest::Completed(receipt) => receipt,
        CancellableRequest::Cancelled => return Ok(None),
    };
    if shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    install_online_receipt(base, &receipt, &verifying_key, &device, source)?;
    Ok(Some(()))
}

/// Install a receipt returned by the native checkout poll. The poll endpoint
/// never exposes the reusable licence key; this client still verifies the
/// signature, product, expiry, and exact device binding before writing it.
pub(crate) fn install_receipt_cancellable(
    receipt: &str,
    base: &Path,
    shutdown: &AtomicBool,
) -> anyhow::Result<Option<()>> {
    if shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    let Some(_guard) = license_operation_cancellable(base, shutdown)? else {
        return Ok(None);
    };
    if shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    let receipt = receipt.trim();
    if !usable_receipt_token(receipt) {
        anyhow::bail!("activation server returned no signed receipt");
    }
    let device = vocalcode_platform::device_id();
    if !usable_device(&device) {
        anyhow::bail!("a stable machine fingerprint is unavailable on this device");
    }
    let verifying_key = embedded_verifying_key()?;
    if shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    install_online_receipt(
        base,
        receipt,
        &verifying_key,
        &device,
        ReceiptInstallSource::Checkout,
    )?;
    Ok(Some(()))
}

fn read_authenticated_receipt(
    base: &Path,
    verifying_key: &[u8; 32],
    device: &str,
) -> anyhow::Result<Option<LicenseClaims>> {
    let source = match read_small_utf8_file(&base.join(LICENSE_FILE), LICENSE_STATE_MAX_BYTES) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let record = match serde_json::from_str::<LicenseRecord>(&source) {
        Ok(record) if record.version == RECORD_VERSION && usable_receipt_token(&record.receipt) => {
            record
        }
        _ => return Ok(None),
    };

    // Time zero accepts an expired but otherwise authentic receipt. Expiry is
    // part of the replacement policy below, while signature, product, device,
    // licence id and entitlement must all be authenticated here.
    match verify_token(&record.receipt, verifying_key, device, 0) {
        Ok(claims) => Ok(Some(claims)),
        Err(error) => {
            log::warn!("existing licence receipt is invalid and will be replaced: {error}");
            Ok(None)
        }
    }
}

/// Decide whether a checkout-delivered receipt may replace an authenticated
/// disk receipt. This contains no I/O or cryptography so every downgrade and
/// cross-licence case can be covered directly by unit tests.
fn validate_checkout_replacement(
    existing: Option<&LicenseClaims>,
    incoming: &LicenseClaims,
    now: u64,
) -> anyhow::Result<()> {
    let Some(existing) = existing else {
        return Ok(());
    };
    if existing.exp <= now {
        return Ok(());
    }
    if existing.key != incoming.key {
        anyhow::bail!(
            "checkout receipt belongs to a different licence; the current active licence was kept"
        );
    }
    if incoming.exp < existing.exp {
        anyhow::bail!(
            "checkout receipt expires earlier than the current receipt; the current receipt was kept"
        );
    }
    if incoming.max_version < existing.max_version {
        anyhow::bail!(
            "checkout receipt has a lower product entitlement; the current receipt was kept"
        );
    }
    Ok(())
}

/// Single authority-bearing write gate for every paid receipt source. The
/// incoming token is authenticated before its signed server time can repair
/// the protected clock anchor; the current disk receipt is then authenticated
/// before source-specific replacement rules are applied.
fn install_online_receipt(
    base: &Path,
    receipt: &str,
    verifying_key: &[u8; 32],
    device: &str,
    source: ReceiptInstallSource,
) -> anyhow::Result<LicenseClaims> {
    if !usable_receipt_token(receipt) {
        anyhow::bail!("activation server returned no signed receipt");
    }
    let incoming = verify_token(receipt, verifying_key, device, 0)
        .map_err(|error| anyhow::anyhow!("activation receipt rejected: {error}"))?;
    let now = install_online_time_anchor(base, incoming.iat)?;
    verify_token(receipt, verifying_key, device, now)
        .map_err(|error| anyhow::anyhow!("activation receipt rejected: {error}"))?;

    let existing = read_authenticated_receipt(base, verifying_key, device)?;
    match source {
        ReceiptInstallSource::ExplicitActivation => {}
        ReceiptInstallSource::LegacyMigration => {
            if existing.is_some() {
                anyhow::bail!(
                    "an authenticated licence receipt already exists; legacy backup was kept"
                );
            }
        }
        ReceiptInstallSource::CachedRefresh => {
            let existing = existing
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no authenticated receipt exists to refresh"))?;
            validate_refresh_replacement(existing, &incoming)?;
        }
        ReceiptInstallSource::Checkout => {
            validate_checkout_replacement(existing.as_ref(), &incoming, now)?;
        }
    }
    save_receipt(base, receipt)?;
    Ok(incoming)
}

#[derive(Clone, Copy)]
struct JsonRequestPolicy {
    https_only: bool,
    connect_timeout: Duration,
    response_timeout: Duration,
    body_timeout: Duration,
    global_timeout: Duration,
}

impl JsonRequestPolicy {
    fn production() -> Self {
        Self {
            https_only: true,
            connect_timeout: JSON_CONNECT_TIMEOUT,
            response_timeout: JSON_RESPONSE_TIMEOUT,
            body_timeout: JSON_BODY_TIMEOUT,
            global_timeout: JSON_GLOBAL_TIMEOUT,
        }
    }
}

fn request_receipt(url: &str, body: String) -> anyhow::Result<String> {
    request_receipt_with_policy(url, body, JsonRequestPolicy::production())
}

fn request_receipt_cancellable(
    url: String,
    body: String,
    shutdown: &AtomicBool,
) -> anyhow::Result<CancellableRequest<String>> {
    cancellable_network_request(shutdown, "vocalcode-activation-request", move || {
        request_receipt(&url, body)
    })
}

fn request_receipt_with_policy(
    url: &str,
    body: String,
    policy: JsonRequestPolicy,
) -> anyhow::Result<String> {
    if policy.connect_timeout.is_zero()
        || policy.response_timeout.is_zero()
        || policy.body_timeout.is_zero()
        || policy.global_timeout.is_zero()
    {
        anyhow::bail!("activation request timeouts must be non-zero");
    }
    let mut resp = ureq::post(url)
        .config()
        .http_status_as_error(false)
        .https_only(policy.https_only)
        .max_redirects(0)
        .timeout_connect(Some(policy.connect_timeout))
        .timeout_recv_response(Some(policy.response_timeout))
        // ureq's global timer is carried into Body::read_json and is documented
        // as DNS-through-body. Keep the tiny JSON body on a stricter independent
        // bound as defence in depth if that internal propagation ever regresses.
        .timeout_recv_body(Some(policy.body_timeout))
        .timeout_global(Some(policy.global_timeout))
        .build()
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(body)
        .map_err(|e| anyhow::anyhow!("activation request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        // A rejection body belongs to the remote server and may deliberately
        // reflect the reusable key/receipt from our POST or contain forged log
        // lines. Never parse or surface it. The numeric status is sufficient
        // for diagnosis and contains no request material.
        anyhow::bail!(
            "activation server rejected the licence (HTTP {})",
            status.as_u16()
        );
    }
    let reply: ActivationResponse = resp
        .body_mut()
        .with_config()
        .limit(JSON_RESPONSE_MAX_BYTES)
        .read_json()
        .map_err(|e| anyhow::anyhow!("read activation response: {e}"))?;
    let receipt = reply
        .token
        .filter(|token| usable_receipt_token(token))
        .ok_or_else(|| anyhow::anyhow!("activation server returned no signed receipt"))?;
    Ok(receipt)
}

fn save_receipt(base: &Path, receipt: &str) -> anyhow::Result<()> {
    if !usable_receipt_token(receipt) {
        anyhow::bail!("signed receipt is empty or too large");
    }
    std::fs::create_dir_all(base)?;
    let rec = LicenseRecord {
        version: RECORD_VERSION,
        receipt: receipt.to_string(),
    };
    let bytes = serde_json::to_vec_pretty(&rec)?;
    crate::storage::atomic_write(&base.join(LICENSE_FILE), bytes).map_err(Into::into)
}

fn save_trial_receipt(base: &Path, receipt: &str) -> anyhow::Result<()> {
    if !usable_receipt_token(receipt) {
        anyhow::bail!("signed trial receipt is empty or too large");
    }
    std::fs::create_dir_all(base)?;
    let record = TrialRecord {
        version: TRIAL_RECORD_VERSION,
        receipt: receipt.to_string(),
    };
    let bytes = serde_json::to_vec_pretty(&record)?;
    crate::storage::atomic_write(&base.join(TRIAL_FILE), bytes).map_err(Into::into)
}

/// Complete CLI activation after a trusted input path has obtained the key.
fn activate_cli(key: &str, base: &Path) -> anyhow::Result<()> {
    println!("Activating on this device…");
    activate(key, base)?;
    println!("Activated. Signed receipt saved; no restart is required.");
    Ok(())
}

/// Read one license key from a non-argument input and activate it.
///
/// The caller should connect this to a no-echo terminal prompt or redirected
/// standard input. Keeping the reusable key out of `argv` prevents it from
/// appearing in shell history and process inspection. Input is bounded before
/// conversion and only one line is accepted.
pub fn activate_cli_from_reader<R: std::io::BufRead>(
    reader: &mut R,
    base: &Path,
) -> anyhow::Result<()> {
    let key = read_cli_key(reader)?;
    activate_cli(&key, base)
}

fn read_cli_key<R: std::io::BufRead>(reader: &mut R) -> anyhow::Result<String> {
    let mut bounded = std::io::Read::take(reader, (LICENSE_KEY_MAX_BYTES + 3) as u64);
    let mut raw = Vec::with_capacity(LICENSE_KEY_MAX_BYTES + 2);
    std::io::BufRead::read_until(&mut bounded, b'\n', &mut raw)?;
    if raw.len() > LICENSE_KEY_MAX_BYTES + 2 {
        anyhow::bail!("license key is too long");
    }
    while matches!(raw.last(), Some(b'\r' | b'\n')) {
        raw.pop();
    }
    let key = String::from_utf8(raw).map_err(|_| anyhow::anyhow!("license key is not UTF-8"))?;
    if !usable_license_key(&key) {
        anyhow::bail!("license key is empty, too long, or malformed");
    }
    Ok(key.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct ReceiptContractFixture {
        public_key_b64: String,
        token: String,
        claims: LicenseClaims,
        trial_token: String,
        trial_claims: TrialClaims,
    }

    fn claims(id: &str, exp: u64, max_version: u32) -> LicenseClaims {
        LicenseClaims {
            key: id.to_string(),
            device: "dev-abc".to_string(),
            product: vocalcode_core::license::PRODUCT.to_string(),
            max_version,
            iat: 1,
            exp,
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vocalcode-activation-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn stalled_detached_request_cancels_without_waiting_for_its_network_deadline() {
        let shutdown = std::sync::Arc::new(AtomicBool::new(false));
        let cancel = shutdown.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let canceller = std::thread::spawn(move || {
            started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            cancel.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let result = cancellable_network_request(&shutdown, "vocalcode-stalled-test", move || {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(30));
            Ok(7_u8)
        })
        .unwrap();

        assert!(matches!(result, CancellableRequest::Cancelled));
        // 100x the 50 ms cancel poll; the request itself would hold for 30 s.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancellation took {:?}",
            started.elapsed()
        );
        let _ = release_tx.send(());
        canceller.join().unwrap();
    }

    #[test]
    fn cancelled_trial_request_never_commits_receipt_or_time_anchor() {
        let fixture: ReceiptContractFixture = serde_json::from_str(include_str!(
            "../../packaging/deliver/receipt-contract-fixture.json"
        ))
        .unwrap();
        let key = verifying_key_from_base64(&fixture.public_key_b64).unwrap();
        let dir = scratch("cancelled-trial-commit");

        let error = refresh_trial_receipt_with_commit_check(
            &dir,
            &fixture.trial_claims.device,
            &key,
            fixture.trial_claims.checked_at,
            Some(1_000),
            || Ok(fixture.trial_token.clone()),
            || false,
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "activation request cancelled");
        assert!(!dir.join(TRIAL_FILE).exists());
        assert!(!dir.join(TRUSTED_TIME_FILE).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn small_state_files_accept_the_limit_and_reject_limit_plus_one() {
        let dir = scratch("small-file-limits");
        for (name, limit) in [
            ("licence.json", LICENSE_STATE_MAX_BYTES),
            ("trusted-time.bin", TRUSTED_TIME_MAX_BYTES),
        ] {
            let path = dir.join(name);
            let exact = vec![b'x'; usize::try_from(limit).unwrap()];
            std::fs::write(&path, &exact).unwrap();
            assert_eq!(read_small_file(&path, limit).unwrap(), exact);
            assert_eq!(
                read_small_utf8_file(&path, limit).unwrap().len(),
                usize::try_from(limit).unwrap()
            );

            std::fs::write(&path, vec![b'x'; usize::try_from(limit + 1).unwrap()]).unwrap();
            let error = read_small_file(&path, limit).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(error.to_string().contains(&limit.to_string()), "{error}");
            assert!(read_small_utf8_file(&path, limit).is_err());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn licence_operation_rejects_a_zero_lock_timeout() {
        let dir = scratch("zero-lock-timeout");
        let local = Mutex::new(());
        let error = match license_operation_with_timeout(&dir, &local, Duration::ZERO) {
            Ok(_) => panic!("a zero lock timeout was accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("must be non-zero"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn licence_lock_wait_observes_shutdown_before_its_deadline() {
        let dir = scratch("cancelled-lock-wait");
        let local = Mutex::new(());
        let _held = local.lock().unwrap();
        let shutdown = AtomicBool::new(false);
        let started = Instant::now();
        let result = license_operation_with_timeout_and_cancel(
            &dir,
            &local,
            Duration::from_secs(30),
            || {
                if started.elapsed() >= Duration::from_millis(75) {
                    shutdown.store(true, Ordering::Release);
                }
                shutdown.load(Ordering::Acquire)
            },
        )
        .unwrap();

        assert!(result.is_none());
        // Far past the 75 ms trigger, far short of the 30 s lock deadline.
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn licence_operation_local_lock_wait_has_one_hard_deadline() {
        let dir = scratch("local-lock-deadline");
        let local = Mutex::new(());
        let held = local.lock().unwrap();
        let timeout = Duration::from_millis(125);
        let started = Instant::now();
        let error = match license_operation_with_timeout(&dir, &local, timeout) {
            Ok(_) => panic!("a concurrently held local lock was acquired"),
            Err(error) => error,
        };
        let elapsed = started.elapsed();
        assert!(
            error.to_string().contains("in-process lock")
                && error.to_string().contains("timed out"),
            "{error}"
        );
        assert!(
            elapsed >= Duration::from_millis(100) && elapsed < Duration::from_secs(2),
            "local lock deadline was not bounded: {elapsed:?}"
        );
        drop(held);

        // Timing out must not poison the mutex or retain hidden state.
        drop(
            license_operation_with_timeout(&dir, &local, Duration::from_secs(1))
                .expect("local lock should be reusable after timeout"),
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn licence_operation_file_lock_wait_has_one_hard_deadline() {
        let dir = scratch("file-lock-deadline");
        let lock_path = dir.join(LICENSE_LOCK_FILE);
        let held_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        fs2::FileExt::lock_exclusive(&held_file).unwrap();
        let local = Mutex::new(());
        let timeout = Duration::from_millis(125);
        let started = Instant::now();
        let error = match license_operation_with_timeout(&dir, &local, timeout) {
            Ok(_) => panic!("a concurrently held file lock was acquired"),
            Err(error) => error,
        };
        let elapsed = started.elapsed();
        assert!(
            error.to_string().contains("cross-process file lock")
                && error.to_string().contains("timed out"),
            "{error}"
        );
        assert!(
            elapsed >= Duration::from_millis(100) && elapsed < Duration::from_secs(2),
            "file lock deadline was not bounded: {elapsed:?}"
        );
        assert!(
            local.try_lock().is_ok(),
            "a file-lock timeout retained the local mutex"
        );

        fs2::FileExt::unlock(&held_file).unwrap();
        drop(held_file);
        drop(
            license_operation_with_timeout(&dir, &local, Duration::from_secs(1))
                .expect("file lock should be reusable after its holder exits"),
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn licence_operation_local_and_file_wait_share_one_deadline() {
        use std::cell::{Cell, RefCell};

        let dir = scratch("shared-lock-deadline");
        let held_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(LICENSE_LOCK_FILE))
            .unwrap();
        fs2::FileExt::lock_exclusive(&held_file).unwrap();

        let local = Mutex::new(());
        let held_local = RefCell::new(Some(local.lock().unwrap()));
        let started = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let result = license_operation_with_clock(
            &dir,
            &local,
            Duration::from_millis(300),
            || false,
            || started + elapsed.get(),
            |duration| {
                elapsed.set(elapsed.get() + duration);
                // Consume half the shared budget before making the local mutex
                // available. The actual file lock stays held throughout.
                if elapsed.get() >= Duration::from_millis(150) {
                    held_local.borrow_mut().take();
                }
            },
        );
        let error = match result {
            Ok(_) => panic!("both held locks were acquired"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("cross-process file lock"),
            "{error}"
        );
        assert_eq!(
            elapsed.get(),
            Duration::from_millis(300),
            "the file phase must inherit the deadline, not start another budget"
        );
        assert!(local.try_lock().is_ok(), "timeout retained the local mutex");

        fs2::FileExt::unlock(&held_file).unwrap();
        drop(held_file);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    #[cfg(windows)]
    fn windows_lock_violation_is_classified_as_contention() {
        assert!(license_file_lock_is_contended(
            &std::io::Error::from_raw_os_error(33)
        ));
    }

    #[test]
    fn activation_json_body_is_bounded_by_the_global_deadline() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                let count = stream.read(&mut byte).unwrap();
                assert_ne!(count, 0, "client closed before sending request headers");
                request.extend_from_slice(&byte[..count]);
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\nConnection: close\r\n\r\n{",
                )
                .unwrap();
            stream.flush().unwrap();
            // The declared JSON body remains incomplete well past the client's
            // deadline. read_json must return on the global timer, not EOF.
            let _ = release_rx.recv_timeout(Duration::from_secs(30));
        });

        let policy = JsonRequestPolicy {
            https_only: false,
            connect_timeout: Duration::from_secs(1),
            response_timeout: Duration::from_secs(1),
            // Keep the phase-specific bound later than the global one so the
            // observed error proves the end-to-end timer reached Body::read_json.
            body_timeout: Duration::from_secs(30),
            // Windows' socket timeout has coarse sub-second behavior on some
            // runners, so use one full second while keeping the peer open for
            // thirty. Production uses this exact path with a 45-second bound.
            global_timeout: Duration::from_secs(1),
        };
        let started = Instant::now();
        let error = request_receipt_with_policy(
            &format!("http://{address}/activate"),
            "{}".to_string(),
            policy,
        )
        .unwrap_err();
        let elapsed = started.elapsed();

        let message = error.to_string().to_ascii_lowercase();
        assert!(
            message.contains("timeout") && message.contains("global"),
            "unexpected stalled-body error: {error}"
        );
        assert!(
            elapsed >= Duration::from_millis(750) && elapsed < Duration::from_secs(10),
            "activation body ignored its global deadline: {elapsed:?}"
        );
        let _ = release_tx.send(());
        server.join().unwrap();
    }

    #[test]
    fn activation_rejection_never_reflects_remote_secrets_or_log_lines() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let secret = "VC-REUSABLE-SECRET";
        let forged = "\n2026-08-07 ERROR forged log entry";
        let response_body = serde_json::json!({
            "error": format!("rejected {secret}{forged}")
        })
        .to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                let count = stream.read(&mut byte).unwrap();
                assert_ne!(count, 0, "client closed before sending request headers");
                request.extend_from_slice(&byte[..count]);
            }
            // Consume the POST body before closing the connection. On Windows,
            // closing a socket with unread inbound bytes can turn the intended
            // HTTP 403 into WSAECONNRESET at the client and make this security
            // assertion spuriously exercise transport failure instead.
            let headers = String::from_utf8(request).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.eq_ignore_ascii_case("content-length") {
                        value.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let mut request_body = vec![0; content_length];
            stream.read_exact(&mut request_body).unwrap();
            write!(
                stream,
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .unwrap();
            stream.flush().unwrap();
        });

        let policy = JsonRequestPolicy {
            https_only: false,
            connect_timeout: Duration::from_secs(1),
            response_timeout: Duration::from_secs(1),
            body_timeout: Duration::from_secs(1),
            global_timeout: Duration::from_secs(2),
        };
        let error = request_receipt_with_policy(
            &format!("http://{address}/activate"),
            serde_json::json!({ "key": secret }).to_string(),
            policy,
        )
        .unwrap_err();
        let message = error.to_string();

        assert_eq!(message, "activation server rejected the licence (HTTP 403)");
        assert!(!message.contains(secret));
        assert!(!message.contains(forged));
        assert!(!message.contains('\n'));
        server.join().unwrap();
    }

    #[test]
    fn production_activation_timeouts_keep_the_body_inside_the_global_budget() {
        let policy = JsonRequestPolicy::production();
        assert_eq!(policy.global_timeout, Duration::from_secs(45));
        assert_eq!(policy.body_timeout, Duration::from_secs(10));
        assert!(policy.connect_timeout <= policy.global_timeout);
        assert!(policy.response_timeout <= policy.global_timeout);
        assert!(policy.body_timeout <= policy.global_timeout);
    }

    #[test]
    fn legacy_two_field_cache_never_activates() {
        let dir = scratch("legacy");
        std::fs::write(
            dir.join(LICENSE_FILE),
            r#"{"key":"anything","device":"dev-abc"}"#,
        )
        .unwrap();
        assert!(license_entitlement(&dir, "dev-abc").is_none());
        assert!(!dir.join(LICENSE_FILE).exists());
        assert!(dir.join(LEGACY_LICENSE_FILE).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unsigned_receipt_json_never_activates() {
        let dir = scratch("unsigned");
        let record = LicenseRecord {
            version: RECORD_VERSION,
            receipt: "eyJwcm9kdWN0Ijoidm9jYWxjb2RlIn0=.not-a-signature".into(),
        };
        std::fs::write(dir.join(LICENSE_FILE), serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(license_entitlement(&dir, "dev-abc").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_shared_device_is_refused_before_disk_is_read() {
        let dir = scratch("unknown-device");
        std::fs::write(dir.join(LICENSE_FILE), b"not even json").unwrap();
        assert!(license_entitlement(&dir, "vocalcode-unknown-device").is_none());
        assert!(!usable_device("device\0injected"));
        assert!(!usable_device(&"d".repeat(DEVICE_ID_MAX_BYTES + 1)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_receipt_is_never_written() {
        let dir = scratch("oversized-receipt");
        let receipt = "r".repeat(RECEIPT_TOKEN_MAX_BYTES + 1);
        assert!(save_receipt(&dir, &receipt).is_err());
        assert!(!dir.join(LICENSE_FILE).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_legacy_refresh_never_authorizes_and_keeps_the_key_backup() {
        let dir = scratch("legacy-refresh-failure");
        std::fs::write(
            dir.join(LICENSE_FILE),
            r#"{"key":"paid-key","device":"old-device"}"#,
        )
        .unwrap();

        let error = refresh_legacy_receipt_with(&dir, |key| {
            assert_eq!(key, "paid-key");
            anyhow::bail!("offline")
        })
        .expect_err("an online refresh failure must be returned");

        assert!(error.to_string().contains("offline"));
        assert!(license_entitlement(&dir, "new-device").is_none());
        assert!(!dir.join(LICENSE_FILE).exists());
        assert!(dir.join(LEGACY_LICENSE_FILE).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_legacy_cache_needs_no_network_refresh() {
        let dir = scratch("no-legacy-refresh");
        let refreshed = refresh_legacy_receipt_with(&dir, |_| {
            panic!("refresh callback must not run without a legacy key")
        })
        .unwrap();
        assert!(!refreshed);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn authenticated_active_receipt_wins_over_a_different_legacy_backup_byte_for_byte() {
        let fixture: ReceiptContractFixture = serde_json::from_str(include_str!(
            "../../packaging/deliver/receipt-contract-fixture.json"
        ))
        .unwrap();
        let key = verifying_key_from_base64(&fixture.public_key_b64).unwrap();
        let dir = scratch("signed-b-wins-over-legacy-a");
        save_receipt(&dir, &fixture.token).unwrap();
        let backup = dir.join(LEGACY_LICENSE_FILE);
        std::fs::write(&backup, br#"{"key":"legacy-key-a","device":"old-device"}"#).unwrap();
        install_online_time_anchor_with(
            &dir,
            fixture.claims.iat,
            fixture.claims.iat + 1,
            Some(1_000),
        )
        .unwrap();
        let active_before = std::fs::read(dir.join(LICENSE_FILE)).unwrap();
        let backup_before = std::fs::read(&backup).unwrap();

        let changed =
            refresh_legacy_receipt_with_authenticated(&dir, &key, &fixture.claims.device, |_| {
                panic!("legacy key A must not be sent while signed receipt B exists")
            })
            .unwrap();
        assert!(!changed);
        assert_eq!(
            std::fs::read(dir.join(LICENSE_FILE)).unwrap(),
            active_before
        );
        assert_eq!(std::fs::read(&backup).unwrap(), backup_before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn paid_receipt_cannot_authorize_after_protected_anchor_deletion_and_rollback() {
        let fixture: ReceiptContractFixture = serde_json::from_str(include_str!(
            "../../packaging/deliver/receipt-contract-fixture.json"
        ))
        .unwrap();
        let key = verifying_key_from_base64(&fixture.public_key_b64).unwrap();
        let record = LicenseRecord {
            version: RECORD_VERSION,
            receipt: fixture.token.clone(),
        };
        let dir = scratch("paid-anchor-delete");
        assert_eq!(
            paid_entitlement_from_record(
                &dir,
                &record,
                &key,
                &fixture.claims.device,
                fixture.claims.iat + 1,
                Some(1_000),
            ),
            None,
            "a cached receipt must not bootstrap its own missing time evidence"
        );
        install_online_time_anchor_with(
            &dir,
            fixture.claims.iat,
            fixture.claims.iat + 1,
            Some(1_000),
        )
        .unwrap();
        assert_eq!(
            paid_entitlement_from_record(
                &dir,
                &record,
                &key,
                &fixture.claims.device,
                fixture.claims.iat + 2,
                Some(2_000),
            ),
            Some(fixture.claims.max_version)
        );
        std::fs::remove_file(dir.join(TRUSTED_TIME_FILE)).unwrap();
        assert_eq!(
            paid_entitlement_from_record(
                &dir,
                &record,
                &key,
                &fixture.claims.device,
                fixture.claims.iat + 1,
                Some(3_000),
            ),
            None
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn plaintext_valid_json_cannot_lower_the_dpapi_protected_anchor() {
        let fixture: ReceiptContractFixture = serde_json::from_str(include_str!(
            "../../packaging/deliver/receipt-contract-fixture.json"
        ))
        .unwrap();
        let key = verifying_key_from_base64(&fixture.public_key_b64).unwrap();
        let record = LicenseRecord {
            version: RECORD_VERSION,
            receipt: fixture.token.clone(),
        };
        let dir = scratch("paid-anchor-plaintext-edit");
        let lowered = TrustedTimeRecord {
            version: TRUSTED_TIME_VERSION,
            trusted_unix: fixture.claims.iat,
            uptime_millis: Some(1_000),
        };
        std::fs::write(
            dir.join(TRUSTED_TIME_FILE),
            serde_json::to_vec(&lowered).unwrap(),
        )
        .unwrap();
        assert_eq!(
            paid_entitlement_from_record(
                &dir,
                &record,
                &key,
                &fixture.claims.device,
                fixture.claims.iat + 1,
                Some(2_000),
            ),
            None
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn trusted_time_advances_on_uptime_when_the_wall_clock_is_frozen() {
        let dir = scratch("trusted-time-freeze");
        assert_eq!(
            trusted_now_with(&dir, 10_000, Some(50_000), 0, false).unwrap(),
            10_000
        );
        assert_eq!(
            trusted_now_with(&dir, 10_000, Some(349_000), 0, false).unwrap(),
            10_299
        );
        let error = trusted_now_with(&dir, 10_000, Some(351_000), 0, false).unwrap_err();
        assert!(error.to_string().contains("moved backwards or stopped"));

        let record: TrustedTimeRecord =
            serde_json::from_slice(&read_trusted_time_bytes(&dir).unwrap().unwrap()).unwrap();
        assert_eq!(record.trusted_unix, 10_301);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn trusted_time_never_rewinds_and_small_clock_corrections_do_not_revive_time() {
        let dir = scratch("trusted-time-small-rollback");
        trusted_now_with(&dir, 20_000, Some(1_000), 0, false).unwrap();
        let effective = trusted_now_with(&dir, 19_950, Some(51_000), 0, false).unwrap();
        assert_eq!(effective, 20_050);
        let later = trusted_now_with(&dir, 20_020, Some(61_000), 0, false).unwrap();
        assert_eq!(later, 20_060);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn frozen_clock_across_a_machine_reboot_fails_closed() {
        let dir = scratch("trusted-time-reboot");
        trusted_now_with(&dir, 30_000, Some(9_000_000), 0, false).unwrap();
        let error = trusted_now_with(&dir, 30_000, Some(2_000), 0, false).unwrap_err();
        assert!(error.to_string().contains("moved backwards or stopped"));
        // Correcting the wall clock restores service without deleting evidence.
        assert_eq!(
            trusted_now_with(&dir, 30_001, Some(3_000), 0, false).unwrap(),
            30_001
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_trusted_time_anchor_is_never_silently_reset() {
        let dir = scratch("trusted-time-malformed");
        let path = dir.join(TRUSTED_TIME_FILE);
        std::fs::write(&path, b"not-valid-protected-data").unwrap();
        assert!(trusted_now_with(&dir, 40_000, Some(1_000), 0, false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not-valid-protected-data");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fresh_authenticated_server_time_repairs_a_future_poisoned_anchor() {
        let dir = scratch("trusted-time-future-repair");
        let poisoned = TrustedTimeRecord {
            version: TRUSTED_TIME_VERSION,
            trusted_unix: 2_100_000_000,
            uptime_millis: Some(1_000),
        };
        write_trusted_time_bytes(&dir, &serde_json::to_vec(&poisoned).unwrap()).unwrap();
        assert_eq!(
            install_online_time_anchor_with(&dir, 1_800_000_000, 1_800_000_001, Some(2_000),)
                .unwrap(),
            1_800_000_001
        );
        let repaired: TrustedTimeRecord =
            serde_json::from_slice(&read_trusted_time_bytes(&dir).unwrap().unwrap()).unwrap();
        assert_eq!(repaired.trusted_unix, 1_800_000_001);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sibling_endpoint_requires_the_configured_activation_path() {
        let default_refresh = DEFAULT_ACTIVATION_URL
            .strip_suffix("/activate")
            .map(|base| format!("{base}/refresh"))
            .unwrap();
        assert_eq!(
            default_refresh,
            "https://vocalcode-deliver.wudaming00.workers.dev/refresh"
        );
        assert_eq!(
            endpoint_url_from_override("trial", None, false).unwrap(),
            "https://vocalcode-deliver.wudaming00.workers.dev/trial"
        );
    }

    #[test]
    fn production_endpoint_cannot_be_overridden_by_process_environment() {
        assert_eq!(
            endpoint_url_from_override(
                "activate",
                Some("https://attacker.example/activate"),
                false,
            )
            .unwrap(),
            DEFAULT_ACTIVATION_URL
        );
        assert_eq!(
            endpoint_url_from_override(
                "refresh",
                Some("https://attacker.example/activate"),
                false,
            )
            .unwrap(),
            "https://vocalcode-deliver.wudaming00.workers.dev/refresh"
        );
        assert_eq!(
            endpoint_url_from_override("trial", Some("https://attacker.example/activate"), false,)
                .unwrap(),
            "https://vocalcode-deliver.wudaming00.workers.dev/trial"
        );
    }

    #[test]
    fn debug_endpoint_override_is_strictly_shaped() {
        assert_eq!(
            endpoint_url_from_override(
                "refresh",
                Some("https://localhost.example/activate/"),
                true,
            )
            .unwrap(),
            "https://localhost.example/refresh"
        );
        assert_eq!(
            endpoint_url_from_override("trial", Some("https://localhost.example/activate/"), true,)
                .unwrap(),
            "https://localhost.example/trial"
        );
        assert!(endpoint_url_from_override(
            "activate",
            Some("https://localhost.example/activate?forward=attacker"),
            true,
        )
        .is_err());
        assert!(
            endpoint_url_from_override("activate", Some("http://localhost/activate"), true,)
                .is_err()
        );
    }

    #[test]
    fn cli_reader_accepts_one_bounded_key_without_using_argv() {
        let mut input = std::io::Cursor::new(b"  KEY-PAID-123  \r\nignored".to_vec());
        assert_eq!(read_cli_key(&mut input).unwrap(), "KEY-PAID-123");

        let mut oversized = std::io::Cursor::new(vec![b'A'; LICENSE_KEY_MAX_BYTES + 3]);
        assert!(read_cli_key(&mut oversized).is_err());

        let mut control = std::io::Cursor::new(b"KEY\0BAD\n".to_vec());
        assert!(read_cli_key(&mut control).is_err());
    }

    #[test]
    fn javascript_receipt_fixture_verifies_with_rust_contract() {
        let fixture: ReceiptContractFixture = serde_json::from_str(include_str!(
            "../../packaging/deliver/receipt-contract-fixture.json"
        ))
        .unwrap();
        let key = verifying_key_from_base64(&fixture.public_key_b64).unwrap();
        let verified = verify_token(
            &fixture.token,
            &key,
            &fixture.claims.device,
            fixture.claims.exp - 1,
        )
        .unwrap();
        assert_eq!(verified.key, fixture.claims.key);
        assert_eq!(verified.product, fixture.claims.product);
        assert_eq!(verified.max_version, fixture.claims.max_version);
        assert_eq!(verified.exp, fixture.claims.exp);
        let trial = verify_trial_token(
            &fixture.trial_token,
            &key,
            &fixture.trial_claims.device,
            fixture.trial_claims.iat + 1,
        )
        .unwrap();
        assert_eq!(trial, fixture.trial_claims);
    }

    #[test]
    fn legacy_or_deleted_trial_cache_is_replaced_only_by_server_signature() {
        let fixture: ReceiptContractFixture = serde_json::from_str(include_str!(
            "../../packaging/deliver/receipt-contract-fixture.json"
        ))
        .unwrap();
        let key = verifying_key_from_base64(&fixture.public_key_b64).unwrap();
        let dir = scratch("signed-trial-migration");
        let path = dir.join(TRIAL_FILE);

        // The retired editable timestamp is migration input, never authority.
        std::fs::write(&path, fixture.trial_claims.iat.to_string()).unwrap();
        assert!(refresh_trial_receipt_with(
            &dir,
            &fixture.trial_claims.device,
            &key,
            fixture.trial_claims.checked_at,
            Some(1_000),
            || Ok(fixture.trial_token.clone()),
        )
        .unwrap());
        let record: TrialRecord = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(record.version, TRIAL_RECORD_VERSION);
        assert_eq!(record.receipt, fixture.trial_token);
        let claims = verify_trial_token(
            &record.receipt,
            &key,
            &fixture.trial_claims.device,
            fixture.trial_claims.iat + 1,
        )
        .unwrap();
        assert_eq!(claims.iat, fixture.trial_claims.iat);

        // Deleting the local cache cannot choose a new epoch. The simulated
        // Worker reissues the same authoritative receipt for this device.
        std::fs::remove_file(&path).unwrap();
        assert!(refresh_trial_receipt_with(
            &dir,
            &fixture.trial_claims.device,
            &key,
            fixture.trial_claims.checked_at,
            Some(2_000),
            || Ok(fixture.trial_token.clone()),
        )
        .unwrap());
        let reissued: TrialRecord = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(reissued.receipt, record.receipt);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn authentic_expired_trial_never_requests_a_fresh_epoch() {
        let fixture: ReceiptContractFixture = serde_json::from_str(include_str!(
            "../../packaging/deliver/receipt-contract-fixture.json"
        ))
        .unwrap();
        let key = verifying_key_from_base64(&fixture.public_key_b64).unwrap();
        let dir = scratch("expired-trial-cache");
        install_online_time_anchor_with(
            &dir,
            fixture.trial_claims.checked_at,
            fixture.trial_claims.checked_at,
            Some(1_000),
        )
        .unwrap();
        let expired_uptime =
            1_000 + (fixture.trial_claims.exp - fixture.trial_claims.checked_at) * 1_000;
        trusted_now_with(
            &dir,
            fixture.trial_claims.exp,
            Some(expired_uptime),
            fixture.trial_claims.checked_at,
            true,
        )
        .unwrap();
        save_trial_receipt(&dir, &fixture.trial_token).unwrap();
        let refreshed = refresh_trial_receipt_with(
            &dir,
            &fixture.trial_claims.device,
            &key,
            fixture.trial_claims.exp,
            Some(expired_uptime + 1_000),
            || panic!("an authentic expired receipt must remain authoritative"),
        )
        .unwrap();
        assert!(!refreshed);
        assert!(verify_trial_token(
            &fixture.trial_token,
            &key,
            &fixture.trial_claims.device,
            fixture.trial_claims.exp,
        )
        .is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn authentic_trial_cannot_authorize_after_anchor_deletion_and_clock_rollback() {
        let fixture: ReceiptContractFixture = serde_json::from_str(include_str!(
            "../../packaging/deliver/receipt-contract-fixture.json"
        ))
        .unwrap();
        let key = verifying_key_from_base64(&fixture.public_key_b64).unwrap();
        let dir = scratch("trial-anchor-delete");
        install_online_time_anchor_with(
            &dir,
            fixture.trial_claims.checked_at,
            fixture.trial_claims.iat + 10,
            system_uptime_millis(),
        )
        .unwrap();
        save_trial_receipt(&dir, &fixture.trial_token).unwrap();
        assert!(matches!(
            trial_status_with_key(
                &dir,
                &fixture.trial_claims.device,
                fixture.trial_claims.iat + 11,
                &key,
            ),
            TrialReceiptStatus::Active(_)
        ));
        std::fs::remove_file(dir.join(TRUSTED_TIME_FILE)).unwrap();
        assert!(matches!(
            trial_status_with_key(
                &dir,
                &fixture.trial_claims.device,
                fixture.trial_claims.iat + 1,
                &key,
            ),
            TrialReceiptStatus::Invalid(_)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn trial_cache_states_distinguish_setup_expiry_and_clock_failure() {
        let fixture: ReceiptContractFixture = serde_json::from_str(include_str!(
            "../../packaging/deliver/receipt-contract-fixture.json"
        ))
        .unwrap();
        let key = verifying_key_from_base64(&fixture.public_key_b64).unwrap();
        let dir = scratch("trial-cache-states");
        assert!(matches!(
            trial_status_with_key(
                &dir,
                &fixture.trial_claims.device,
                fixture.trial_claims.iat,
                &key
            ),
            TrialReceiptStatus::SetupRequired
        ));
        std::fs::write(dir.join(TRIAL_FILE), fixture.trial_claims.iat.to_string()).unwrap();
        assert!(matches!(
            trial_status_with_key(
                &dir,
                &fixture.trial_claims.device,
                fixture.trial_claims.iat,
                &key
            ),
            TrialReceiptStatus::SetupRequired
        ));
        install_online_time_anchor_with(
            &dir,
            fixture.trial_claims.checked_at,
            fixture.trial_claims.iat + 1,
            system_uptime_millis(),
        )
        .unwrap();
        save_trial_receipt(&dir, &fixture.trial_token).unwrap();
        assert!(matches!(
            trial_status_with_key(
                &dir,
                &fixture.trial_claims.device,
                fixture.trial_claims.iat + 1,
                &key,
            ),
            TrialReceiptStatus::Active(_)
        ));
        assert!(matches!(
            trial_status_with_key(
                &dir,
                &fixture.trial_claims.device,
                fixture.trial_claims.exp,
                &key,
            ),
            TrialReceiptStatus::Expired
        ));
        assert!(matches!(
            trial_status_with_key(
                &dir,
                &fixture.trial_claims.device,
                fixture.trial_claims.iat - 301,
                &key,
            ),
            TrialReceiptStatus::Invalid(_)
        ));
        assert!(matches!(
            trial_status_with_key(&dir, "other-device", fixture.trial_claims.iat + 1, &key),
            TrialReceiptStatus::SetupRequired
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wrong_device_trial_response_is_rejected_without_overwriting_evidence() {
        let fixture: ReceiptContractFixture = serde_json::from_str(include_str!(
            "../../packaging/deliver/receipt-contract-fixture.json"
        ))
        .unwrap();
        let key = verifying_key_from_base64(&fixture.public_key_b64).unwrap();
        let dir = scratch("wrong-device-trial");
        let path = dir.join(TRIAL_FILE);
        std::fs::write(&path, "legacy-evidence").unwrap();
        let error = refresh_trial_receipt_with(
            &dir,
            "different-device",
            &key,
            fixture.trial_claims.checked_at,
            Some(1_000),
            || Ok(fixture.trial_token.clone()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("different device"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "legacy-evidence");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Invoked by `smoke-production.mjs` only after its explicit live-write
    /// opt-in has received a real receipt from the deployed Worker.
    #[test]
    #[ignore = "requires an explicitly authorized production activation smoke run"]
    fn production_worker_receipt_verifies_in_rust() {
        let token = std::env::var("VC_E2E_RECEIPT_TOKEN").expect("missing smoke receipt");
        let trial_token = std::env::var("VC_E2E_TRIAL_TOKEN").expect("missing smoke trial receipt");
        let device = std::env::var("VC_E2E_DEVICE").expect("missing smoke device");
        let expected_public = std::env::var("VOCALCODE_LICENSE_PUBLIC_KEY_B64")
            .expect("missing production public key");
        let runtime_public = verifying_key_from_base64(&expected_public).unwrap();
        let embedded_public = embedded_verifying_key().expect("public key was not embedded");
        assert_eq!(
            embedded_public, runtime_public,
            "embedded public key drifted"
        );
        let claims = verify_token(&token, &embedded_public, &device, now_unix())
            .expect("production Worker receipt failed Rust verification");
        assert_eq!(claims.device, device);
        assert_eq!(claims.product, vocalcode_core::license::PRODUCT);
        assert!(claims.max_version > 0);
        let trial = verify_trial_token(&trial_token, &embedded_public, &device, 0)
            .expect("production Worker trial receipt failed Rust verification");
        assert_eq!(trial.device, device);
        assert_eq!(trial.product, vocalcode_core::license::PRODUCT);
    }

    #[test]
    fn checkout_cannot_replace_an_active_different_licence() {
        let existing = claims("licence-a", 5_000, 1);
        let incoming = claims("licence-b", 6_000, 2);
        let error = validate_checkout_replacement(Some(&existing), &incoming, 1_000)
            .expect_err("a stale checkout must not switch licences");
        assert!(error.to_string().contains("different licence"));
    }

    #[test]
    fn checkout_cannot_downgrade_expiry_or_entitlement() {
        let existing = claims("licence-a", 5_000, 3);

        let earlier = claims("licence-a", 4_999, 3);
        let error = validate_checkout_replacement(Some(&existing), &earlier, 1_000)
            .expect_err("an earlier expiry is a downgrade");
        assert!(error.to_string().contains("expires earlier"));

        let lower_entitlement = claims("licence-a", 6_000, 2);
        let error = validate_checkout_replacement(Some(&existing), &lower_entitlement, 1_000)
            .expect_err("a lower max_version is a downgrade");
        assert!(error.to_string().contains("lower product entitlement"));
    }

    #[test]
    fn checkout_may_refresh_same_licence_or_replace_an_expired_one() {
        let existing = claims("licence-a", 5_000, 2);
        let refreshed = claims("licence-a", 6_000, 2);
        validate_checkout_replacement(Some(&existing), &refreshed, 1_000).unwrap();
        validate_checkout_replacement(None, &refreshed, 1_000).unwrap();

        let expired = claims("licence-a", 900, 9);
        let replacement = claims("licence-b", 6_000, 1);
        validate_checkout_replacement(Some(&expired), &replacement, 1_000).unwrap();
    }

    #[test]
    fn refresh_must_keep_the_licence_and_never_reduce_entitlement() {
        let existing = claims("licence-a", 5_000, 2);
        validate_refresh_replacement(&existing, &claims("licence-a", 6_000, 2)).unwrap();
        validate_refresh_replacement(&existing, &claims("licence-a", 6_000, 3)).unwrap();

        let other =
            validate_refresh_replacement(&existing, &claims("licence-b", 6_000, 2)).unwrap_err();
        assert!(other.to_string().contains("different licence"));
        let downgraded =
            validate_refresh_replacement(&existing, &claims("licence-a", 6_000, 1)).unwrap_err();
        assert!(downgraded.to_string().contains("lower product entitlement"));
        let stale =
            validate_refresh_replacement(&existing, &claims("licence-a", 5_000, 2)).unwrap_err();
        assert!(stale.to_string().contains("does not extend"));
    }
}
