//! Where VocalCode keeps its writable state (config, models, trial, licence).
//!
//! Writable data must never share a directory with installed program files. On
//! Windows the installer lives under Program Files; treating that directory as
//! app data made the Settings "Remove app data" action delete the uninstaller
//! and every unlocked program file. Windows state now lives in
//! `%LOCALAPPDATA%\VocalCode`, with a one-time allow-listed migration from the
//! legacy executable directory. macOS uses its standard Application Support
//! location for the same code-signing and permissions reasons as before.

use std::path::{Component, Path, PathBuf};
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{fs::File, fs::OpenOptions};

#[cfg(windows)]
static MIGRATION_NONCE: AtomicU64 = AtomicU64::new(0);
#[cfg(windows)]
const MIGRATION_STAGE_PREFIX: &str = ".vocalcode-migration-";
#[cfg(windows)]
const LEGACY_MIGRATION_STAGE_PREFIX: &str = ".vocalcode-migrating-";
#[cfg(windows)]
const MIGRATION_MANIFEST: &str = ".entry-name";
#[cfg(windows)]
const MIGRATION_MANIFEST_MAX_BYTES: u64 = 256;

/// Directory containing the running executable.
///
/// Read-only resources may be resolved from here. Never write user state here;
/// use [`data_dir`].
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn ensure_dir(dir: PathBuf) -> PathBuf {
    std::fs::create_dir_all(&dir).unwrap_or_else(|error| {
        panic!(
            "could not create trusted data directory {}: {error}",
            dir.display()
        )
    });
    let metadata = std::fs::symlink_metadata(&dir).unwrap_or_else(|error| {
        panic!(
            "could not inspect trusted data directory {}: {error}",
            dir.display()
        )
    });
    if !metadata.is_dir() || data_directory_is_redirect(&metadata) {
        panic!(
            "refusing redirected or non-directory data root {}",
            dir.display()
        );
    }
    dir
}

fn invalid_trusted_directory(path: &Path, reason: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "refusing untrusted data directory {}: {reason}",
            path.display()
        ),
    )
}

/// Open a directory without following its final component and retain the
/// handle while a child path is being constructed. On Windows, omitting
/// `FILE_SHARE_DELETE` also prevents an inspected component from being swapped
/// for a junction before the next component is opened.
#[cfg(windows)]
fn hold_trusted_directory(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || data_directory_is_redirect(&metadata) {
        return Err(invalid_trusted_directory(
            path,
            "the opened handle is not a real directory",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn hold_trusted_directory(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    #[cfg(target_os = "macos")]
    const NOFOLLOW_DIRECTORY: i32 = 0x0010_0100; // O_DIRECTORY | O_NOFOLLOW
    #[cfg(target_os = "linux")]
    const NOFOLLOW_DIRECTORY: i32 = 0x0003_0000; // O_DIRECTORY | O_NOFOLLOW
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    const NOFOLLOW_DIRECTORY: i32 = 0;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW_DIRECTORY)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || data_directory_is_redirect(&metadata) {
        return Err(invalid_trusted_directory(
            path,
            "the opened handle is not a real directory",
        ));
    }
    Ok(file)
}

#[cfg(not(any(windows, unix)))]
fn hold_trusted_directory(path: &Path) -> std::io::Result<File> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || data_directory_is_redirect(&metadata) {
        return Err(invalid_trusted_directory(
            path,
            "the opened handle is not a real directory",
        ));
    }
    Ok(file)
}

fn validate_trusted_directory_path(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || data_directory_is_redirect(&metadata) {
        return Err(invalid_trusted_directory(
            path,
            "the path is a redirect or is not a directory",
        ));
    }
    Ok(())
}

/// Create a relative directory below an already trusted data root.
///
/// Only ordinary relative components are accepted. Every existing or newly
/// created component is checked with `symlink_metadata` and an opened,
/// no-follow directory handle; handles for all parents remain live until the
/// full path has been verified. This is the required entry point for writable
/// WebView2 and overlay subdirectories.
pub(crate) fn ensure_trusted_data_subdir(base: &Path, relative: &Path) -> std::io::Result<PathBuf> {
    if !base.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("trusted data root must be absolute: {}", base.display()),
        ));
    }

    let components = relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name.to_owned()),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "trusted data subdirectory must contain only normal relative components: {}",
                    relative.display()
                ),
            )),
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    if components.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "trusted data subdirectory cannot be empty",
        ));
    }

    validate_trusted_directory_path(base)?;
    let mut held_directories = vec![hold_trusted_directory(base)?];
    let mut current = base.to_path_buf();

    for component in components {
        current.push(component);
        match std::fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        validate_trusted_directory_path(&current)?;
        held_directories.push(hold_trusted_directory(&current)?);
    }

    // Keep every no-delete/no-follow handle alive through the final check.
    validate_trusted_directory_path(&current)?;
    drop(held_directories);
    Ok(current)
}

#[cfg(windows)]
fn data_directory_is_redirect(metadata: &std::fs::Metadata) -> bool {
    is_link_or_reparse(metadata)
}

#[cfg(not(windows))]
fn data_directory_is_redirect(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// Process-lifetime guard for VocalCode's writable data tree.
///
/// Every normal GUI/CLI process holds a shared lock from before its first data
/// access until all of its workers have stopped. Destructive cleanup takes the
/// exclusive form. The lock file deliberately lives beside, rather than
/// inside, `VocalCode`: purging the protected tree must not unlink the inode
/// that another process is blocked on.
pub struct DataLifecycleGuard {
    file: File,
}

struct LifecycleTransitionGuard {
    file: File,
}

#[derive(Clone, Copy)]
enum LifecycleLockWait {
    #[cfg(all(test, windows))]
    Blocking,
    Until(Instant),
}

#[derive(Clone, Copy)]
enum LifecycleLockMode {
    Shared,
    Exclusive,
}

const LIFECYCLE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);
const LIFECYCLE_STARTUP_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

fn lifecycle_lock_is_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock || {
        #[cfg(windows)]
        {
            // fs2 0.4 forwards ERROR_LOCK_VIOLATION instead of mapping it
            // to ErrorKind::WouldBlock on Windows.
            error.raw_os_error() == Some(33)
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

fn lifecycle_lock_timeout(description: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("timed out waiting for {description}"),
    )
}

impl Drop for LifecycleTransitionGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

impl Drop for DataLifecycleGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

#[cfg(target_os = "macos")]
fn raw_data_dir() -> PathBuf {
    use objc2_foundation::{
        NSSearchPathDirectory, NSSearchPathDomainMask, NSSearchPathForDirectoriesInDomains,
    };

    let paths = NSSearchPathForDirectoriesInDomains(
        NSSearchPathDirectory::ApplicationSupportDirectory,
        NSSearchPathDomainMask::UserDomainMask,
        true,
    );
    let support = paths
        .firstObject()
        .map(|value| PathBuf::from(value.to_string()))
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            panic!("macOS did not return an absolute user Application Support directory")
        });
    support.join(crate::community::DATA_DIR_NAME)
}

#[cfg(windows)]
fn raw_data_dir() -> PathBuf {
    windows_data_dir_from_known_folder()
}

#[cfg(not(any(target_os = "macos", windows)))]
fn raw_data_dir() -> PathBuf {
    exe_dir()
}

fn lifecycle_lock_parent() -> std::io::Result<PathBuf> {
    let data = raw_data_dir();
    let parent = data.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("app-data path {} has no stable parent", data.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || data_directory_is_redirect(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "refusing redirected or non-directory app-data parent {}",
                parent.display()
            ),
        ));
    }
    Ok(parent.to_path_buf())
}

fn lifecycle_lock_path() -> std::io::Result<PathBuf> {
    Ok(lifecycle_lock_parent()?.join(crate::community::DATA_LOCK_NAME))
}

fn lifecycle_transition_path() -> std::io::Result<PathBuf> {
    Ok(lifecycle_lock_parent()?.join(crate::community::TRANSITION_LOCK_NAME))
}

fn open_lifecycle_lock_at(path: &std::path::Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn acquire_lifecycle_file_lock(
    file: &File,
    mode: LifecycleLockMode,
    wait: LifecycleLockWait,
    description: &str,
) -> std::io::Result<()> {
    match wait {
        #[cfg(all(test, windows))]
        LifecycleLockWait::Blocking => match mode {
            LifecycleLockMode::Shared => fs2::FileExt::lock_shared(file),
            LifecycleLockMode::Exclusive => fs2::FileExt::lock_exclusive(file),
        },
        LifecycleLockWait::Until(deadline) => loop {
            if Instant::now() >= deadline {
                return Err(lifecycle_lock_timeout(description));
            }
            let result = match mode {
                LifecycleLockMode::Shared => fs2::FileExt::try_lock_shared(file),
                LifecycleLockMode::Exclusive => fs2::FileExt::try_lock_exclusive(file),
            };
            match result {
                Ok(()) => {
                    if Instant::now() >= deadline {
                        let _ = fs2::FileExt::unlock(file);
                        return Err(lifecycle_lock_timeout(description));
                    }
                    return Ok(());
                }
                Err(error) if lifecycle_lock_is_contended(&error) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(lifecycle_lock_timeout(description));
                    }
                    std::thread::sleep(
                        LIFECYCLE_LOCK_POLL_INTERVAL.min(deadline.saturating_duration_since(now)),
                    );
                }
                Err(error) => return Err(error),
            }
        },
    }
}

fn lock_lifecycle_transition_with(
    wait: LifecycleLockWait,
) -> std::io::Result<LifecycleTransitionGuard> {
    let file = open_lifecycle_lock_at(&lifecycle_transition_path()?)?;
    acquire_lifecycle_file_lock(
        &file,
        LifecycleLockMode::Exclusive,
        wait,
        "the app-data lifecycle transition lock",
    )?;
    Ok(LifecycleTransitionGuard { file })
}

fn lock_data_lifecycle_shared_raw_with(
    wait: LifecycleLockWait,
) -> std::io::Result<DataLifecycleGuard> {
    lock_data_lifecycle_shared_at_with(&lifecycle_lock_path()?, wait)
}

#[cfg(all(test, windows))]
fn lock_data_lifecycle_shared_at(path: &std::path::Path) -> std::io::Result<DataLifecycleGuard> {
    lock_data_lifecycle_shared_at_with(path, LifecycleLockWait::Blocking)
}

fn lock_data_lifecycle_shared_at_with(
    path: &std::path::Path,
    wait: LifecycleLockWait,
) -> std::io::Result<DataLifecycleGuard> {
    let file = open_lifecycle_lock_at(path)?;
    acquire_lifecycle_file_lock(
        &file,
        LifecycleLockMode::Shared,
        wait,
        "the shared app-data lifecycle lock",
    )?;
    Ok(DataLifecycleGuard { file })
}

fn lock_data_lifecycle_exclusive_raw_with(
    wait: LifecycleLockWait,
) -> std::io::Result<DataLifecycleGuard> {
    lock_data_lifecycle_exclusive_at_with(&lifecycle_lock_path()?, wait)
}

#[cfg(all(test, windows))]
fn lock_data_lifecycle_exclusive_at(path: &std::path::Path) -> std::io::Result<DataLifecycleGuard> {
    lock_data_lifecycle_exclusive_at_with(path, LifecycleLockWait::Blocking)
}

fn lock_data_lifecycle_exclusive_at_with(
    path: &std::path::Path,
    wait: LifecycleLockWait,
) -> std::io::Result<DataLifecycleGuard> {
    let file = open_lifecycle_lock_at(path)?;
    acquire_lifecycle_file_lock(
        &file,
        LifecycleLockMode::Exclusive,
        wait,
        "the exclusive app-data lifecycle lock",
    )?;
    Ok(DataLifecycleGuard { file })
}

/// Deadline-bounded exclusive form for cleanup/uninstall. A diagnostic process
/// may legitimately retain a shared guard without owning the installer's GUI
/// observation mutex, so destructive callers must fail instead of waiting
/// forever when such a process remains alive.
pub(crate) fn lock_data_lifecycle_exclusive_until(
    deadline: Instant,
) -> std::io::Result<DataLifecycleGuard> {
    let wait = LifecycleLockWait::Until(deadline);
    let _transition = lock_lifecycle_transition_with(wait)?;
    lock_data_lifecycle_exclusive_raw_with(wait)
}

/// Cooperative atomic downgrade. The caller must have held `transition` since
/// before acquiring `exclusive`, and must keep it until the returned shared
/// guard has been acquired. That ordering prevents another cooperative process
/// from waiting for this exclusive lock while holding the transition gate (the
/// circular wait that a downgrade which reacquired the gate would create).
#[cfg(all(test, windows))]
fn downgrade_data_lifecycle_at(
    exclusive: DataLifecycleGuard,
    _transition: &LifecycleTransitionGuard,
    lifecycle_path: &std::path::Path,
) -> std::io::Result<DataLifecycleGuard> {
    downgrade_data_lifecycle_at_with(
        exclusive,
        _transition,
        lifecycle_path,
        LifecycleLockWait::Blocking,
    )
}

#[cfg(windows)]
fn downgrade_data_lifecycle_at_with(
    exclusive: DataLifecycleGuard,
    _transition: &LifecycleTransitionGuard,
    lifecycle_path: &std::path::Path,
    wait: LifecycleLockWait,
) -> std::io::Result<DataLifecycleGuard> {
    drop(exclusive);
    lock_data_lifecycle_shared_at_with(lifecycle_path, wait)
}

/// The writable per-user data directory, created on first use.
#[cfg(target_os = "macos")]
pub fn data_dir() -> PathBuf {
    ensure_dir(raw_data_dir())
}

#[cfg(windows)]
fn windows_data_dir_from_known_folder() -> PathBuf {
    use std::ffi::{c_void, OsString};
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY,
    };

    let mut raw = std::ptr::null_mut();
    // SAFETY: `raw` is a valid out pointer. On success Shell allocates one
    // NUL-terminated UTF-16 string with the COM task allocator; every return
    // below frees it with CoTaskMemFree.
    let result = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    if result < 0 || raw.is_null() {
        if !raw.is_null() {
            // SAFETY: a non-null failure result is still documented as COM-task
            // allocated output and may be released by the caller.
            unsafe { CoTaskMemFree(raw.cast::<c_void>()) };
        }
        panic!("SHGetKnownFolderPath(LocalAppData) failed with HRESULT {result:#010x}");
    }
    let mut len = 0usize;
    // SAFETY: successful SHGetKnownFolderPath returns a NUL-terminated string.
    unsafe {
        while *raw.add(len) != 0 {
            len += 1;
        }
    }
    // SAFETY: the `len` initialized UTF-16 units precede the terminator above.
    let local = unsafe { OsString::from_wide(std::slice::from_raw_parts(raw, len)) };
    // SAFETY: `raw` came from SHGetKnownFolderPath and has not yet been freed.
    unsafe { CoTaskMemFree(raw.cast::<c_void>()) };
    let local = PathBuf::from(local);
    if !local.is_absolute() {
        panic!(
            "SHGetKnownFolderPath(LocalAppData) returned a non-absolute path {}",
            local.display()
        );
    }
    local.join(crate::community::DATA_DIR_NAME)
}

#[cfg(windows)]
fn is_legacy_data_entry(name: &str) -> bool {
    matches!(
        name,
        "models"
            | "vocalcode.toml"
            | "vocalcode-trial.dat"
            | "vocalcode-time-anchor.bin"
            | "vocalcode-time-anchor.json"
            | "vocalcode-license.json"
            | "vocalcode-license.legacy.json"
            | "replacements.txt"
            | "totals.json"
            | "vocalcode.log"
            | "vocalcode.log.1"
    ) || name.starts_with("vocalcode.toml.invalid-")
        || name.starts_with("replacements.txt.invalid-")
        || name.starts_with("totals.json.invalid-")
}

#[cfg(windows)]
fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// True for symbolic links, junctions and other Windows reparse points. Purge
/// and migration callers use this before any recursive filesystem operation.
#[cfg(windows)]
pub(crate) fn is_reparse_point(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| is_link_or_reparse(&metadata))
        // Failure to classify a path must never make recursion look safe.
        .unwrap_or(true)
}

#[cfg(windows)]
fn reparse_error(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "refusing to migrate symlink or reparse point {}",
            path.display()
        ),
    )
}

/// Walk a source tree without following it. This is deliberately run both
/// before and after the source is moved under a unique staging name: the first
/// pass avoids disturbing an obvious junction, while the second closes the
/// race where the legacy name was swapped between inspection and rename.
#[cfg(windows)]
fn validate_tree_without_reparse(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if is_link_or_reparse(&metadata) {
        return Err(reparse_error(path));
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            validate_tree_without_reparse(&entry?.path())?;
        }
    } else if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("refusing to migrate special file {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn hash_migration_tree(path: &Path) -> std::io::Result<[u8; 32]> {
    use sha2::Digest;

    fn update(path: &Path, digest: &mut sha2::Sha256) -> std::io::Result<()> {
        use std::io::Read;
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let metadata = std::fs::symlink_metadata(path)?;
        if is_link_or_reparse(&metadata) {
            return Err(reparse_error(path));
        }
        if metadata.is_file() {
            digest.update(b"F");
            digest.update(metadata.len().to_le_bytes());
            let mut options = std::fs::OpenOptions::new();
            options
                .read(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
            let mut file = options.open(path)?;
            let opened = file.metadata()?;
            if is_link_or_reparse(&opened) || !opened.is_file() || opened.len() != metadata.len() {
                return Err(reparse_error(path));
            }
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            return Ok(());
        }
        if !metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("refusing to hash special migration file {}", path.display()),
            ));
        }
        digest.update(b"D");
        let mut entries = std::fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name().encode_wide().collect::<Vec<_>>());
        for entry in entries {
            let name = entry.file_name().encode_wide().collect::<Vec<_>>();
            digest.update((name.len() as u64).to_le_bytes());
            for unit in name {
                digest.update(unit.to_le_bytes());
            }
            update(&entry.path(), digest)?;
        }
        Ok(())
    }

    let mut digest = sha2::Sha256::new();
    update(path, &mut digest)?;
    Ok(digest.finalize().into())
}

#[cfg(windows)]
fn migration_trees_match(left: &Path, right: &Path) -> std::io::Result<bool> {
    Ok(hash_migration_tree(left)? == hash_migration_tree(right)?)
}

#[cfg(windows)]
fn create_migration_stage(parent: &Path, role: &str) -> std::io::Result<PathBuf> {
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..128 {
        let nonce = MIGRATION_NONCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            "{MIGRATION_STAGE_PREFIX}{role}-{}-{tick:032x}-{nonce:016x}",
            std::process::id()
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not reserve a unique migration staging directory",
    ))
}

#[cfg(windows)]
fn write_migration_manifest(stage: &Path, entry_name: &str) -> std::io::Result<()> {
    if !is_legacy_data_entry(entry_name)
        || Path::new(entry_name).components().count() != 1
        || entry_name == "."
        || entry_name == ".."
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "migration manifest entry is not allow-listed",
        ));
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(stage.join(MIGRATION_MANIFEST))?;
    file.write_all(entry_name.as_bytes())?;
    file.sync_all()
}

#[cfg(windows)]
fn read_migration_manifest(stage: &Path) -> std::io::Result<String> {
    use std::io::Read;

    let path = stage.join(MIGRATION_MANIFEST);
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || is_link_or_reparse(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "migration manifest is not a regular file",
        ));
    }
    if metadata.len() > MIGRATION_MANIFEST_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "migration manifest exceeds its safety limit",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(&path)?
        .take(MIGRATION_MANIFEST_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MIGRATION_MANIFEST_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "migration manifest exceeds its safety limit",
        ));
    }
    let source = String::from_utf8(bytes).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, error.utf8_error())
    })?;
    if source.is_empty()
        || !is_legacy_data_entry(&source)
        || Path::new(&source).components().count() != 1
        || source == "."
        || source == ".."
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "migration stage has an invalid entry manifest",
        ));
    }
    Ok(source)
}

#[cfg(windows)]
fn remove_completed_stage(stage: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(stage.join(MIGRATION_MANIFEST)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::remove_dir(stage)
}

#[cfg(windows)]
fn remove_migration_path(path: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if is_link_or_reparse(&metadata) {
        return std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path));
    }
    if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Copy one regular file into a name that must not already exist and flush the
/// complete contents before the name can be published. `std::fs::copy` may
/// return while dirty pages are still only in the cache, which made a
/// cross-volume migration look complete after a power loss even though model
/// bytes had never reached disk.
#[cfg(windows)]
fn copy_file_synced(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut source_options = std::fs::OpenOptions::new();
    source_options
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let mut input = source_options.open(source)?;
    let metadata = input.metadata()?;
    if is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(reparse_error(source));
    }

    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let result = (|| {
        let copied = std::io::copy(&mut input, &mut output)?;
        if copied != metadata.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "source changed while copying {}: expected {} bytes, copied {copied}",
                    source.display(),
                    metadata.len()
                ),
            ));
        }
        output.set_permissions(metadata.permissions())?;
        output.sync_all()
    })();
    drop(output);
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}

#[cfg(windows)]
fn copy_directory_without_following_links(
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    std::fs::create_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        let target = destination.join(entry.file_name());
        if is_link_or_reparse(&metadata) {
            return Err(reparse_error(&entry.path()));
        }
        if metadata.is_dir() {
            copy_directory_without_following_links(&entry.path(), &target)?;
        } else if metadata.is_file() {
            copy_file_synced(&entry.path(), &target)?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "refusing to migrate special file {}",
                    entry.path().display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn restore_staged_source(payload: &Path, source: &Path, stage: &Path) -> std::io::Result<()> {
    crate::storage::atomic_move_into_reserved(payload, source)?;
    if let Err(error) = remove_completed_stage(stage) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!(
                "restored {}, but could not remove empty migration stage {}: {error}",
                source.display(),
                stage.display()
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
fn restore_after_migration_error(
    error: std::io::Error,
    payload: &Path,
    source: &Path,
    stage: &Path,
) -> std::io::Error {
    match restore_staged_source(payload, source, stage) {
        Ok(()) => error,
        Err(restore_error) => std::io::Error::new(
            error.kind(),
            format!(
                "{error}; could not restore the legacy name ({restore_error}); data is preserved at {}",
                payload.display()
            ),
        ),
    }
}

#[cfg(windows)]
fn finish_committed_source_stage(
    payload: &Path,
    destination: &Path,
    stage: &Path,
) -> std::io::Result<bool> {
    if !destination.exists() {
        return Ok(false);
    }
    validate_tree_without_reparse(destination)?;
    if !migration_trees_match(payload, destination)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "migration destination {} conflicts with staged source {}; preserving both",
                destination.display(),
                payload.display()
            ),
        ));
    }
    remove_migration_path(payload)?;
    remove_completed_stage(stage)?;
    Ok(true)
}

/// Move one legacy entry without ever replacing a destination created by a
/// newer build. Rename is atomic and cheap on the usual same-volume install;
/// the copy fallback handles custom installs on another drive.
#[cfg(windows)]
fn migrate_entry(source: &Path, destination: &Path) -> std::io::Result<()> {
    if destination.exists() || !source.exists() {
        return Ok(());
    }
    validate_tree_without_reparse(source)?;

    // First take ownership of the exact name we inspected by moving it into a
    // unique directory on the *source* volume. A racing replacement of the old
    // public name can no longer become the object we publish. The staged tree is
    // then inspected again before either the fast rename or copy fallback.
    let source_parent = source.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "legacy path has no parent",
        )
    })?;
    let source_stage = create_migration_stage(source_parent, "source")?;
    let entry_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "legacy migration entry name is not UTF-8",
            )
        })?;
    if let Err(error) = write_migration_manifest(&source_stage, entry_name) {
        let _ = std::fs::remove_dir(&source_stage);
        return Err(error);
    }
    let source_payload = source_stage.join("payload");
    if let Err(error) = crate::storage::atomic_move_into_reserved(source, &source_payload) {
        let _ = remove_completed_stage(&source_stage);
        if destination.exists() || !source.exists() {
            return Ok(());
        }
        return Err(error);
    }
    if let Err(error) = validate_tree_without_reparse(&source_payload) {
        return Err(restore_after_migration_error(
            error,
            &source_payload,
            source,
            &source_stage,
        ));
    }

    // Same-volume installs stay a pair of no-clobber atomic renames. Publishing
    // from the controlled name, rather than the public legacy name, is what
    // makes the reparse validation above meaningful.
    match crate::storage::atomic_move_into_reserved(&source_payload, destination) {
        Ok(()) => {
            if let Err(error) = remove_completed_stage(&source_stage) {
                log::warn!(
                    "migrated {}, but could not remove empty stage {}: {error}",
                    source.display(),
                    source_stage.display()
                );
            }
            return Ok(());
        }
        Err(_) if destination.exists() => {
            if finish_committed_source_stage(&source_payload, destination, &source_stage)? {
                log::info!(
                    "confirmed and removed committed migration residue for {}",
                    destination.display()
                );
                return Ok(());
            }
            restore_staged_source(&source_payload, source, &source_stage)?;
            return Ok(());
        }
        Err(_) => {}
    }

    // A direct rename fails across volumes. Build a fully flushed copy inside a
    // unique destination-side directory and only then publish its payload.
    let destination_parent = destination.parent().ok_or_else(|| {
        restore_after_migration_error(
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "migration destination has no parent",
            ),
            &source_payload,
            source,
            &source_stage,
        )
    })?;
    let destination_stage = match create_migration_stage(destination_parent, "copy") {
        Ok(stage) => stage,
        Err(error) => {
            return Err(restore_after_migration_error(
                error,
                &source_payload,
                source,
                &source_stage,
            ));
        }
    };
    if let Err(error) = write_migration_manifest(&destination_stage, entry_name) {
        let _ = std::fs::remove_dir(&destination_stage);
        return Err(restore_after_migration_error(
            error,
            &source_payload,
            source,
            &source_stage,
        ));
    }
    let destination_payload = destination_stage.join("payload");
    let source_metadata = match std::fs::symlink_metadata(&source_payload) {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = remove_migration_path(&destination_stage);
            return Err(restore_after_migration_error(
                error,
                &source_payload,
                source,
                &source_stage,
            ));
        }
    };
    let copied = if source_metadata.is_dir() {
        copy_directory_without_following_links(&source_payload, &destination_payload)
    } else {
        copy_file_synced(&source_payload, &destination_payload)
    };
    if let Err(error) = copied {
        let cleanup = remove_migration_path(&destination_stage);
        let error = if let Err(cleanup_error) = cleanup {
            std::io::Error::new(
                error.kind(),
                format!(
                    "{error}; could not remove partial copy {}: {cleanup_error}",
                    destination_stage.display()
                ),
            )
        } else {
            error
        };
        return Err(restore_after_migration_error(
            error,
            &source_payload,
            source,
            &source_stage,
        ));
    }

    match crate::storage::atomic_move_into_reserved(&destination_payload, destination) {
        Ok(()) => {
            let _ = remove_completed_stage(&destination_stage);
            if let Err(error) = remove_migration_path(&source_payload) {
                log::warn!(
                    "migrated {}, but its staged legacy copy remains at {}: {error}",
                    source.display(),
                    source_payload.display()
                );
            } else if let Err(error) = remove_completed_stage(&source_stage) {
                log::warn!(
                    "migrated {}, but could not remove empty stage {}: {error}",
                    source.display(),
                    source_stage.display()
                );
            }
            Ok(())
        }
        Err(_error) if destination.exists() => {
            let _ = remove_migration_path(&destination_stage);
            if finish_committed_source_stage(&source_payload, destination, &source_stage)? {
                log::info!(
                    "confirmed and removed committed cross-volume residue for {}",
                    destination.display()
                );
                Ok(())
            } else {
                restore_staged_source(&source_payload, source, &source_stage)
            }
        }
        Err(error) => {
            let _ = remove_migration_path(&destination_stage);
            Err(restore_after_migration_error(
                error,
                &source_payload,
                source,
                &source_stage,
            ))
        }
    }
}

#[cfg(windows)]
fn migration_stage_present(parent: &Path, role: &str) -> bool {
    let prefixes = [
        format!("{MIGRATION_STAGE_PREFIX}{role}-"),
        format!("{LEGACY_MIGRATION_STAGE_PREFIX}{role}-"),
    ];
    std::fs::read_dir(parent)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            prefixes.iter().any(|prefix| name.starts_with(prefix))
        })
}

/// Recover source ownership after a crash between moving a public legacy entry
/// into its private stage and publishing it at the new location. A valid stage
/// is restored to the original public name with a no-clobber move; if that name
/// has since reappeared, both copies are preserved for explicit recovery.
#[cfg(windows)]
fn recover_source_migration_stages(legacy: &Path, destination: &Path) {
    let prefixes = [
        format!("{MIGRATION_STAGE_PREFIX}source-"),
        format!("{LEGACY_MIGRATION_STAGE_PREFIX}source-"),
    ];
    let Some(entries) = std::fs::read_dir(legacy).ok() else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        if !prefixes
            .iter()
            .any(|prefix| entry.file_name().to_string_lossy().starts_with(prefix))
        {
            continue;
        }
        let stage = entry.path();
        let recovered = (|| -> std::io::Result<()> {
            let metadata = std::fs::symlink_metadata(&stage)?;
            if is_link_or_reparse(&metadata) || !metadata.is_dir() {
                return Err(reparse_error(&stage));
            }
            let name = read_migration_manifest(&stage)?;
            let payload = stage.join("payload");
            if !payload.exists() {
                return remove_completed_stage(&stage);
            }
            validate_tree_without_reparse(&payload)?;
            let original = legacy.join(name);
            let committed = destination.join(
                original
                    .file_name()
                    .expect("validated migration manifest always has one component"),
            );
            if committed.exists() {
                if original.exists() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!(
                            "both public legacy name {} and committed destination {} exist; preserving staged payload {}",
                            original.display(),
                            committed.display(),
                            payload.display()
                        ),
                    ));
                }
                if finish_committed_source_stage(&payload, &committed, &stage)? {
                    return Ok(());
                }
            }
            if original.exists() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "public legacy name {} already exists; staged payload is preserved at {}",
                        original.display(),
                        payload.display()
                    ),
                ));
            }
            crate::storage::atomic_move_into_reserved(&payload, &original)?;
            remove_completed_stage(&stage)
        })();
        match recovered {
            Ok(()) => log::info!("recovered interrupted legacy migration {}", stage.display()),
            Err(error) => log::warn!(
                "preserving interrupted legacy migration stage {}: {error}",
                stage.display()
            ),
        }
    }
}

/// Destination copy stages are never authoritative: the source stage retains
/// the complete synced payload until publication succeeds. After source stages
/// are restored, a validated orphan copy can therefore be removed safely.
#[cfg(windows)]
fn clean_copy_migration_stages(destination_parent: &Path) {
    let prefixes = [
        format!("{MIGRATION_STAGE_PREFIX}copy-"),
        format!("{LEGACY_MIGRATION_STAGE_PREFIX}copy-"),
    ];
    let Some(entries) = std::fs::read_dir(destination_parent).ok() else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        if !prefixes
            .iter()
            .any(|prefix| entry.file_name().to_string_lossy().starts_with(prefix))
        {
            continue;
        }
        let stage = entry.path();
        let cleaned = (|| -> std::io::Result<()> {
            let _ = read_migration_manifest(&stage)?;
            validate_tree_without_reparse(&stage)?;
            remove_migration_path(&stage)
        })();
        match cleaned {
            Ok(()) => log::info!("removed interrupted migration copy {}", stage.display()),
            Err(error) => log::warn!(
                "preserving unverified migration copy stage {}: {error}",
                stage.display()
            ),
        }
    }
}

#[cfg(windows)]
fn migrate_legacy_windows_data(legacy: &Path, destination: &Path) {
    if legacy == destination || !legacy.is_dir() {
        return;
    }
    let entries = match std::fs::read_dir(legacy) {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!(
                "could not inspect legacy data at {}: {error}",
                legacy.display()
            );
            return;
        }
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name_text) = name.to_str() else {
            continue;
        };
        if !is_legacy_data_entry(name_text) {
            continue;
        }
        let target = destination.join(&name);
        match migrate_entry(&entry.path(), &target) {
            Ok(()) if target.exists() => log::info!(
                "migrated legacy app data {} -> {}",
                entry.path().display(),
                target.display()
            ),
            Ok(()) => {}
            Err(error) => log::warn!(
                "could not migrate legacy app data {}: {error}; the source was preserved",
                entry.path().display()
            ),
        }
    }
}

#[cfg(windows)]
pub fn data_dir() -> PathBuf {
    use std::sync::OnceLock;
    static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
    DATA_DIR.get_or_init(|| ensure_dir(raw_data_dir())).clone()
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn data_dir() -> PathBuf {
    // Linux is not a shipped target yet; preserve the existing portable layout
    // there until a supported installer defines its standard data location.
    exe_dir()
}

#[cfg(windows)]
fn legacy_migration_needed(legacy: &Path, destination: &Path) -> bool {
    if legacy == destination || !legacy.is_dir() {
        return false;
    }
    std::fs::read_dir(legacy)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| is_legacy_data_entry(name) && !destination.join(name).exists())
                .unwrap_or(false)
        })
}

#[cfg(windows)]
fn windows_migration_work_needed(legacy: &Path, destination: &Path) -> bool {
    legacy_migration_needed(legacy, destination)
        || migration_stage_present(legacy, "source")
        || destination
            .parent()
            .map(|parent| migration_stage_present(parent, "copy"))
            .unwrap_or(false)
}

/// Enter the writable-data lifecycle before logging, migration, model access,
/// or any CLI/GUI state read. Normal processes retain the returned shared guard
/// for their complete lifetime. Windows legacy migration is an exclusive,
/// rechecked transaction; the common no-migration path remains shared so a GUI
/// and diagnostic CLI can coexist.
fn enter_data_lifecycle_with(wait: LifecycleLockWait) -> std::io::Result<DataLifecycleGuard> {
    // Hold the transition gate across the complete shared -> exclusive ->
    // shared transaction. Normal exclusive callers use the same lock order,
    // but release the gate as soon as they own the lifecycle lock.
    let transition = lock_lifecycle_transition_with(wait)?;
    let shared = lock_data_lifecycle_shared_raw_with(wait)?;
    #[cfg(windows)]
    let destination = ensure_dir(raw_data_dir());
    #[cfg(not(windows))]
    drop(ensure_dir(raw_data_dir()));

    // Unverified/conflicting crash evidence is deliberately preserved. Try the
    // complete recovery/migration transaction once per process startup, then
    // continue under the shared guard even if evidence remains; otherwise a
    // malformed stage name could turn the recovery loop into a permanent
    // startup denial of service.
    #[cfg(windows)]
    {
        let legacy = exe_dir();
        if !crate::community::ENABLED && windows_migration_work_needed(&legacy, &destination) {
            drop(shared);
            let exclusive = lock_data_lifecycle_exclusive_raw_with(wait)?;
            // A process ahead of us may have completed the move while this one
            // waited. The mover itself also rechecks every destination and
            // publishes with no-clobber atomic operations.
            if windows_migration_work_needed(&legacy, &destination) {
                recover_source_migration_stages(&legacy, &destination);
                if let Some(parent) = destination.parent() {
                    clean_copy_migration_stages(parent);
                }
                migrate_legacy_windows_data(&legacy, &destination);
            }
            let shared = downgrade_data_lifecycle_at_with(
                exclusive,
                &transition,
                &lifecycle_lock_path()?,
                wait,
            )?;
            drop(transition);
            return Ok(shared);
        }
    }

    drop(transition);
    Ok(shared)
}

pub fn enter_data_lifecycle() -> std::io::Result<DataLifecycleGuard> {
    let deadline = Instant::now()
        .checked_add(LIFECYCLE_STARTUP_LOCK_TIMEOUT)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "app-data lifecycle startup deadline overflow",
            )
        })?;
    enter_data_lifecycle_until(deadline)
}

/// Enter the migration/data lifecycle without permitting any caller to wait
/// indefinitely on another process. Every transition in the possible shared
/// -> exclusive -> shared migration uses the same absolute deadline.
pub(crate) fn enter_data_lifecycle_until(deadline: Instant) -> std::io::Result<DataLifecycleGuard> {
    enter_data_lifecycle_with(LifecycleLockWait::Until(deadline))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn migration_moves_only_allow_listed_data_and_never_program_files() {
        let scratch = TempDir::new("paths-allow-list");
        let legacy = scratch.join("install");
        let data = scratch.join("data");
        std::fs::create_dir_all(legacy.join("models/model-a")).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(legacy.join("models/model-a/model.onnx"), b"model").unwrap();
        std::fs::write(legacy.join("vocalcode.toml"), b"onboarded = true").unwrap();
        std::fs::write(legacy.join("vocalcode-time-anchor.bin"), b"dpapi").unwrap();
        std::fs::write(legacy.join("vocalcode-time-anchor.json"), b"legacy").unwrap();
        std::fs::write(legacy.join("VocalCode.exe"), b"program").unwrap();
        std::fs::write(legacy.join("unins000.exe"), b"uninstaller").unwrap();

        migrate_legacy_windows_data(&legacy, &data);

        assert_eq!(
            std::fs::read(data.join("vocalcode.toml")).unwrap(),
            b"onboarded = true"
        );
        assert_eq!(
            std::fs::read(data.join("models/model-a/model.onnx")).unwrap(),
            b"model"
        );
        assert_eq!(
            std::fs::read(data.join("vocalcode-time-anchor.bin")).unwrap(),
            b"dpapi"
        );
        assert_eq!(
            std::fs::read(data.join("vocalcode-time-anchor.json")).unwrap(),
            b"legacy"
        );
        assert!(legacy.join("VocalCode.exe").exists());
        assert!(legacy.join("unins000.exe").exists());
        assert!(!data.join("VocalCode.exe").exists());
        assert!(!data.join("unins000.exe").exists());
        assert!(
            std::fs::read_dir(&legacy).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".vocalcode-migration-")),
            "a successful move must not leave source staging behind"
        );
        assert!(
            std::fs::read_dir(&data).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".vocalcode-migration-")),
            "a successful move must not leave destination staging behind"
        );
    }

    #[test]
    fn migration_never_overwrites_newer_destination_state() {
        let scratch = TempDir::new("paths-no-clobber");
        let legacy = scratch.join("install");
        let data = scratch.join("data");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(legacy.join("vocalcode.toml"), b"old").unwrap();
        std::fs::write(data.join("vocalcode.toml"), b"new").unwrap();

        migrate_legacy_windows_data(&legacy, &data);

        assert_eq!(std::fs::read(data.join("vocalcode.toml")).unwrap(), b"new");
        assert_eq!(
            std::fs::read(legacy.join("vocalcode.toml")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn crash_after_source_staging_is_recovered_before_migration_retries() {
        let scratch = TempDir::new("paths-crash-source-stage");
        let legacy = scratch.join("legacy");
        let data = scratch.join("data");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let source = legacy.join("vocalcode.toml");
        std::fs::write(&source, b"language = 'en'").unwrap();

        let stage = create_migration_stage(&legacy, "source").unwrap();
        write_migration_manifest(&stage, "vocalcode.toml").unwrap();
        crate::storage::atomic_move_into_reserved(&source, &stage.join("payload")).unwrap();
        assert!(!source.exists(), "simulated crash hides the public name");

        recover_source_migration_stages(&legacy, &data);
        assert_eq!(std::fs::read(&source).unwrap(), b"language = 'en'");
        assert!(!stage.exists());
        migrate_legacy_windows_data(&legacy, &data);
        assert_eq!(
            std::fs::read(data.join("vocalcode.toml")).unwrap(),
            b"language = 'en'"
        );
    }

    #[test]
    fn crash_after_conflicting_destination_preserves_staged_authority() {
        let scratch = TempDir::new("paths-crash-after-publish");
        let legacy = scratch.join("legacy");
        let data = scratch.join("data");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let stage = create_migration_stage(&legacy, "source").unwrap();
        write_migration_manifest(&stage, "vocalcode.toml").unwrap();
        std::fs::write(stage.join("payload"), b"older recovered settings").unwrap();
        std::fs::write(data.join("vocalcode.toml"), b"new destination settings").unwrap();

        recover_source_migration_stages(&legacy, &data);
        assert!(!legacy.join("vocalcode.toml").exists());
        assert_eq!(
            std::fs::read(stage.join("payload")).unwrap(),
            b"older recovered settings"
        );
        assert_eq!(
            std::fs::read(data.join("vocalcode.toml")).unwrap(),
            b"new destination settings"
        );
    }

    #[test]
    fn crash_after_committed_publish_removes_identical_source_residue() {
        let scratch = TempDir::new("paths-crash-after-committed-publish");
        let legacy = scratch.join("legacy");
        let data = scratch.join("data");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let stage = create_migration_stage(&legacy, "source").unwrap();
        write_migration_manifest(&stage, "models").unwrap();
        std::fs::create_dir_all(stage.join("payload/model-a")).unwrap();
        std::fs::write(stage.join("payload/model-a/model.onnx"), b"same model").unwrap();
        std::fs::create_dir_all(data.join("models/model-a")).unwrap();
        std::fs::write(data.join("models/model-a/model.onnx"), b"same model").unwrap();

        recover_source_migration_stages(&legacy, &data);

        assert!(
            !stage.exists(),
            "verified committed residue should be removed"
        );
        assert!(
            !legacy.join("models").exists(),
            "a duplicate source must not be restored"
        );
        assert_eq!(
            std::fs::read(data.join("models/model-a/model.onnx")).unwrap(),
            b"same model"
        );
    }

    #[test]
    fn recovery_recognizes_the_installer_legacy_stage_spelling() {
        let scratch = TempDir::new("paths-legacy-stage-spelling");
        let legacy = scratch.join("legacy");
        let data = scratch.join("data");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let stage = create_migration_stage(&legacy, "source").unwrap();
        write_migration_manifest(&stage, "vocalcode.toml").unwrap();
        std::fs::write(stage.join("payload"), b"recover me").unwrap();
        let old_name = stage.file_name().unwrap().to_string_lossy().replacen(
            MIGRATION_STAGE_PREFIX,
            LEGACY_MIGRATION_STAGE_PREFIX,
            1,
        );
        let legacy_spelling = legacy.join(old_name);
        std::fs::rename(&stage, &legacy_spelling).unwrap();

        assert!(windows_migration_work_needed(&legacy, &data));
        recover_source_migration_stages(&legacy, &data);
        assert_eq!(
            std::fs::read(legacy.join("vocalcode.toml")).unwrap(),
            b"recover me"
        );
        assert!(!legacy_spelling.exists());
    }

    #[test]
    fn orphan_copy_cleanup_requires_a_valid_manifest_and_never_follows_links() {
        let scratch = TempDir::new("paths-crash-copy-stage");
        let valid = create_migration_stage(scratch.path(), "copy").unwrap();
        write_migration_manifest(&valid, "vocalcode.toml").unwrap();
        std::fs::write(valid.join("payload"), b"partial copy").unwrap();
        let unowned = create_migration_stage(scratch.path(), "copy").unwrap();
        std::fs::write(unowned.join("payload"), b"unowned evidence").unwrap();

        clean_copy_migration_stages(scratch.path());
        assert!(!valid.exists());
        assert!(unowned.exists(), "unverified stages must be preserved");
        let legacy = scratch.join("legacy");
        std::fs::create_dir_all(&legacy).unwrap();
        let destination = scratch.join("VocalCode");
        assert!(windows_migration_work_needed(&legacy, &destination));
        clean_copy_migration_stages(scratch.path());
        assert!(unowned.exists(), "one recovery pass preserves bad evidence");
    }

    #[test]
    fn migration_manifest_reader_rejects_limit_plus_one_without_allocating_it_all() {
        let scratch = TempDir::new("paths-oversized-migration-manifest");
        let stage = create_migration_stage(scratch.path(), "copy").unwrap();
        std::fs::write(
            stage.join(MIGRATION_MANIFEST),
            vec![b'x'; MIGRATION_MANIFEST_MAX_BYTES as usize + 1],
        )
        .unwrap();

        let error = read_migration_manifest(&stage).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("safety limit"), "{error}");
    }

    #[test]
    fn lifecycle_lock_is_outside_the_tree_it_protects() {
        let data = raw_data_dir();
        let lock = lifecycle_lock_path().unwrap();
        assert!(!lock.starts_with(&data));
        assert_eq!(lock.parent(), data.parent());
    }

    #[test]
    fn shared_lifecycle_guard_excludes_destructive_owner() {
        let scratch = TempDir::new("paths-lifecycle-lock");
        let path = scratch.join("lifecycle.lock");
        let shared = open_lifecycle_lock_at(&path).unwrap();
        fs2::FileExt::lock_shared(&shared).unwrap();

        let destructive = open_lifecycle_lock_at(&path).unwrap();
        assert!(
            fs2::FileExt::try_lock_exclusive(&destructive).is_err(),
            "purge must not overlap a process that may still write app data"
        );

        fs2::FileExt::unlock(&shared).unwrap();
        fs2::FileExt::try_lock_exclusive(&destructive).unwrap();
        fs2::FileExt::unlock(&destructive).unwrap();
    }

    #[test]
    fn deadline_bounded_lifecycle_lock_times_out_while_a_reader_is_alive() {
        use std::time::{Duration, Instant};

        let scratch = TempDir::new("paths-lifecycle-timeout");
        let path = scratch.join("lifecycle.lock");
        let shared = lock_data_lifecycle_shared_at(&path).unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_millis(75);

        let error = match lock_data_lifecycle_exclusive_at_with(
            &path,
            LifecycleLockWait::Until(deadline),
        ) {
            Ok(_) => panic!("a live shared owner must prevent destructive ownership"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the bounded lock attempt waited far beyond its deadline"
        );
        drop(shared);
    }

    #[test]
    fn deadline_bounded_startup_lock_times_out_while_destructive_owner_is_alive() {
        use std::time::{Duration, Instant};

        let scratch = TempDir::new("paths-startup-lifecycle-timeout");
        let path = scratch.join("lifecycle.lock");
        let destructive = lock_data_lifecycle_exclusive_at(&path).unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_millis(125);

        let error =
            match lock_data_lifecycle_shared_at_with(&path, LifecycleLockWait::Until(deadline)) {
                Ok(_) => panic!("a live destructive owner must prevent startup ownership"),
                Err(error) => error,
            };

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            error.to_string().contains("shared app-data lifecycle lock"),
            "{error}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(100)
                && started.elapsed() < Duration::from_secs(2),
            "startup lock attempt ignored its deadline: {:?}",
            started.elapsed()
        );
        drop(destructive);

        // The timeout path must release its file handle; the next ordinary
        // shared acquisition succeeds once the destructive owner is gone.
        drop(
            lock_data_lifecycle_shared_at_with(
                &path,
                LifecycleLockWait::Until(Instant::now() + Duration::from_secs(1)),
            )
            .expect("startup lock should recover after its owner exits"),
        );
    }

    #[test]
    fn expired_startup_deadline_never_acquires_an_available_lock() {
        let scratch = TempDir::new("paths-expired-startup-deadline");
        let path = scratch.join("lifecycle.lock");
        let error = match lock_data_lifecycle_shared_at_with(
            &path,
            LifecycleLockWait::Until(Instant::now()),
        ) {
            Ok(_) => panic!("an expired startup deadline acquired the lock"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);

        // No phantom ownership may be left behind by the rejected attempt.
        drop(
            lock_data_lifecycle_exclusive_at_with(
                &path,
                LifecycleLockWait::Until(Instant::now() + Duration::from_secs(1)),
            )
            .expect("expired startup attempt retained the file lock"),
        );
    }

    #[test]
    fn transition_gate_makes_exclusive_to_shared_downgrade_finite() {
        use std::sync::mpsc;
        use std::time::Duration;

        let scratch = TempDir::new("paths-lifecycle-downgrade");
        let transition_path = scratch.join("transition.lock");
        let lifecycle_path = scratch.join("lifecycle.lock");

        let transition_file = open_lifecycle_lock_at(&transition_path).unwrap();
        fs2::FileExt::lock_exclusive(&transition_file).unwrap();
        let transition = LifecycleTransitionGuard {
            file: transition_file,
        };
        let exclusive = lock_data_lifecycle_exclusive_at(&lifecycle_path).unwrap();

        let (ready_tx, ready_rx) = mpsc::channel();
        let (owned_tx, owned_rx) = mpsc::channel();
        let contender_transition = transition_path.clone();
        let contender_lifecycle = lifecycle_path.clone();
        let contender = std::thread::spawn(move || {
            let transition = open_lifecycle_lock_at(&contender_transition).unwrap();
            ready_tx.send(()).unwrap();
            fs2::FileExt::lock_exclusive(&transition).unwrap();
            let lifecycle = open_lifecycle_lock_at(&contender_lifecycle).unwrap();
            fs2::FileExt::lock_exclusive(&lifecycle).unwrap();
            owned_tx.send(()).unwrap();
            fs2::FileExt::unlock(&lifecycle).unwrap();
            fs2::FileExt::unlock(&transition).unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();

        // The old implementation deadlocked here: it tried to reacquire the
        // transition lock while still owning the exclusive lifecycle lock.
        let shared = downgrade_data_lifecycle_at(exclusive, &transition, &lifecycle_path).unwrap();
        drop(transition);
        assert!(
            owned_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "the queued destructive owner must wait for the replacement shared guard"
        );
        drop(shared);
        owned_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the queued owner should finish once the shared guard exits");
        contender.join().unwrap();
    }

    #[test]
    fn failed_migration_restores_the_public_source_and_cleans_staging() {
        let scratch = TempDir::new("paths-restore-on-error");
        let legacy = scratch.join("install");
        std::fs::create_dir_all(&legacy).unwrap();
        let source = legacy.join("vocalcode.toml");
        std::fs::write(&source, b"keep me").unwrap();

        // A regular file cannot be the parent of a destination. This fails only
        // after the source has entered its controlled stage, exercising the
        // restore path without relying on permissions or another drive.
        let impossible_parent = scratch.join("not-a-directory");
        std::fs::write(&impossible_parent, b"blocker").unwrap();
        let error = migrate_entry(&source, &impossible_parent.join("vocalcode.toml"))
            .expect_err("the destination parent is not a directory");
        assert!(!error.to_string().is_empty());
        assert_eq!(std::fs::read(&source).unwrap(), b"keep me");
        assert!(
            std::fs::read_dir(&legacy).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".vocalcode-migration-")),
            "a failed transaction must restore the source name and remove its stage"
        );
    }

    #[test]
    fn synced_copy_is_no_clobber() {
        let scratch = TempDir::new("paths-synced-copy");
        let source = scratch.join("source.bin");
        let destination = scratch.join("destination.bin");
        std::fs::write(&source, b"complete model bytes").unwrap();

        copy_file_synced(&source, &destination).unwrap();
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"complete model bytes"
        );
        let error = copy_file_synced(&source, &destination).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"complete model bytes",
            "a second writer must not truncate a published copy"
        );
    }

    #[test]
    fn trusted_data_root_rejects_a_regular_file_and_directory_redirect() {
        let scratch = TempDir::new("paths-trusted-root");
        let file = scratch.join("not-a-directory");
        std::fs::write(&file, b"block").unwrap();
        assert!(std::panic::catch_unwind(|| ensure_dir(file.clone())).is_err());

        let target = scratch.join("target");
        let redirected = scratch.join("redirected");
        std::fs::create_dir(&target).unwrap();
        if std::os::windows::fs::symlink_dir(&target, &redirected).is_ok() {
            assert!(std::panic::catch_unwind(|| ensure_dir(redirected.clone())).is_err());
        }
    }

    #[test]
    fn trusted_subdirectory_is_created_component_by_component() {
        let scratch = TempDir::new("paths-trusted-subdir");
        let created =
            ensure_trusted_data_subdir(scratch.path(), Path::new("webview2/settings")).unwrap();
        assert_eq!(created, scratch.join("webview2/settings"));
        assert!(created.is_dir());

        let repeated =
            ensure_trusted_data_subdir(scratch.path(), Path::new("webview2/settings")).unwrap();
        assert_eq!(repeated, created);
    }

    #[test]
    fn trusted_subdirectory_rejects_escape_and_non_directory_components() {
        let scratch = TempDir::new("paths-trusted-subdir-reject");
        assert!(ensure_trusted_data_subdir(scratch.path(), Path::new("../escape")).is_err());
        assert!(ensure_trusted_data_subdir(scratch.path(), &scratch.join("absolute")).is_err());
        assert!(ensure_trusted_data_subdir(scratch.path(), Path::new("")).is_err());

        std::fs::create_dir(scratch.join("webview2")).unwrap();
        std::fs::write(scratch.join("webview2/settings"), b"not a directory").unwrap();
        assert!(
            ensure_trusted_data_subdir(scratch.path(), Path::new("webview2/settings")).is_err()
        );
    }

    #[test]
    fn trusted_subdirectory_rejects_redirected_component() {
        let scratch = TempDir::new("paths-trusted-subdir-redirect");
        let outside = scratch.join("outside");
        let redirected = scratch.join("webview2");
        std::fs::create_dir(&outside).unwrap();
        if std::os::windows::fs::symlink_dir(&outside, &redirected).is_ok() {
            assert!(
                ensure_trusted_data_subdir(scratch.path(), Path::new("webview2/settings")).is_err()
            );
            assert!(!outside.join("settings").exists());
        }
    }
}

#[cfg(test)]
mod trusted_os_directory_contract_tests {
    #[test]
    fn shipped_platforms_never_derive_data_roots_from_environment_or_cwd() {
        let source = include_str!("paths.rs");
        assert!(source.contains("SHGetKnownFolderPath"));
        assert!(source.contains("FOLDERID_LocalAppData"));
        assert!(source.contains("KF_FLAG_DONT_VERIFY as u32"));
        assert!(source.contains("NSSearchPathForDirectoriesInDomains"));
        assert!(source.contains("ApplicationSupportDirectory"));
        assert!(!source.contains("var_os(\"LOCALAPPDATA\")"));
        assert!(!source.contains("var_os(\"APPDATA\")"));
        assert!(!source.contains("var_os(\"USERPROFILE\")"));
        assert!(!source.contains("var_os(\"HOME\")"));
    }
}
