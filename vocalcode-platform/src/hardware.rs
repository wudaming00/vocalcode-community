//! Lightweight machine capability probe used to pick the right model tier.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HARDWARE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const HARDWARE_PROBE_OUTPUT_LIMIT: u64 = 16 * 1024;

/// Run a small, trusted system probe without allowing it to hold application
/// startup forever or return unbounded output. These probes are only hints for
/// model selection: any failure safely falls back to the CPU/default value.
fn bounded_probe_output(command: &mut Command) -> Option<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + HARDWARE_PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                child
                    .stdout
                    .take()?
                    .take(HARDWARE_PROBE_OUTPUT_LIMIT + 1)
                    .read_to_end(&mut stdout)
                    .ok()?;
                return (status.success()
                    && !stdout.is_empty()
                    && stdout.len() as u64 <= HARDWARE_PROBE_OUTPUT_LIMIT)
                    .then_some(stdout);
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                // `wait` is only bounded after a successful kill. If the OS
                // refuses termination, closing our handles is preferable to
                // turning this optional capability hint into another hang.
                if child.kill().is_ok() {
                    let _ = child.wait();
                } else {
                    let _ = child.try_wait();
                }
                return None;
            }
        }
    }
}

#[cfg(windows)]
fn trusted_nvidia_smi() -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = vec![0_u16; 32_768];
    let written = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if written == 0 || written as usize >= buffer.len() {
        return None;
    }
    let directory =
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..written as usize]));
    let executable = directory.join("nvidia-smi.exe");
    (directory.is_absolute() && executable.is_file()).then_some(executable)
}

/// True if an NVIDIA GPU is available (via `nvidia-smi`). Non-NVIDIA machines
/// fall back to the CPU tier.
#[cfg(windows)]
pub fn has_gpu() -> bool {
    trusted_nvidia_smi()
        .and_then(|executable| bounded_probe_output(Command::new(executable).arg("-L")))
        .is_some()
}

/// Linux is not a release target today. Keep its developer fallback
/// fail-closed too: only conventional absolute install paths are eligible,
/// never an executable supplied by the current directory or `PATH`.
#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn has_gpu() -> bool {
    ["/usr/bin/nvidia-smi", "/usr/local/bin/nvidia-smi"]
        .into_iter()
        .map(std::path::Path::new)
        .find(|path| path.is_file())
        .and_then(|path| bounded_probe_output(Command::new(path).arg("-L")))
        .is_some()
}

/// Always false on macOS: there is no NVIDIA GPU to find, and we do not yet
/// build sherpa-onnx with the CoreML execution provider, so inference is CPU
/// regardless of the Apple GPU. Reporting `Cpu` therefore keeps model selection
/// honest — claiming `Gpu` would pick Whisper Large and run it slowly on CPU.
/// (Shelling out to `nvidia-smi` here would also just be a wasted process spawn.)
#[cfg(target_os = "macos")]
pub fn has_gpu() -> bool {
    false
}

/// Logical CPU cores (fallback 2).
pub fn logical_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
}

/// Installed physical memory in MiB. A failed probe is `None`, never a made-up
/// high-end value: recommendations must degrade conservatively.
#[cfg(windows)]
pub fn memory_mib() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0)
        .then_some(status.ullTotalPhys / (1024 * 1024))
}

#[cfg(target_os = "macos")]
pub fn memory_mib() -> Option<u64> {
    bounded_probe_output(Command::new("/usr/sbin/sysctl").args(["-n", "hw.memsize"]))
        .and_then(|stdout| String::from_utf8(stdout).ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|bytes| bytes / (1024 * 1024))
        .filter(|mib| *mib > 0)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn memory_mib() -> Option<u64> {
    None
}

fn has_fast_vector_instructions() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        std::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        true
    }
}

/// Cores worth handing to the ASR thread pool.
///
/// On Apple Silicon, `logical_cores` counts efficiency cores too (e.g. an
/// M1 Max reports 10 = 8 performance + 2 efficiency). Scheduling ONNX threads
/// onto the E-cores makes the slowest thread dominate and hurts latency, so we
/// report the performance-core count from `hw.perflevel0.logicalcpu` instead.
#[cfg(target_os = "macos")]
pub fn inference_cores() -> usize {
    bounded_probe_output(Command::new("/usr/sbin/sysctl").args(["-n", "hw.perflevel0.logicalcpu"]))
        .and_then(|stdout| String::from_utf8(stdout).ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        // Intel Macs have no perflevel keys — fall back to the logical count.
        .unwrap_or_else(logical_cores)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardwareProfile {
    pub logical_cores: usize,
    pub inference_cores: usize,
    pub memory_mib: Option<u64>,
    pub fast_vector: bool,
}

impl HardwareProfile {
    pub fn detect() -> Self {
        Self {
            logical_cores: logical_cores(),
            inference_cores: inference_cores(),
            memory_mib: memory_mib(),
            fast_vector: has_fast_vector_instructions(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerformanceClass {
    Compact,
    Standard,
    Performance,
}

/// Pure classification used by both the model recommendation and its tests.
/// Unknown RAM intentionally cannot qualify for the performance tier.
///
/// `inference_cores` counts physical performance cores, so the thresholds are
/// in cores, not hardware threads: a 4-core/8-thread laptop is Standard, while
/// fewer than four cores (or too little RAM, or no AVX2) is Compact.
pub fn performance_class(profile: HardwareProfile) -> PerformanceClass {
    if profile.inference_cores < 4
        || profile.memory_mib.is_some_and(|memory| memory < 8 * 1024)
        || !profile.fast_vector
    {
        return PerformanceClass::Compact;
    }
    if profile.inference_cores >= 8 && profile.memory_mib.is_some_and(|memory| memory >= 16 * 1024)
    {
        PerformanceClass::Performance
    } else {
        PerformanceClass::Standard
    }
}

/// Thread caps measured against the shipped runtime. Small encoder models stop
/// improving around four threads. Qwen gains from four to six physical cores
/// and not measurably beyond (16-core desktop, eight English clips, median
/// 944 / 785 / 814 / 796 ms at 4 / 6 / 8 / 12 threads), while every extra
/// thread is taken from the rest of the machine for the whole utterance.
/// Threads beyond the physical performance cores would land on SMT siblings
/// or efficiency cores, where the slowest thread sets the pace.
pub fn asr_threads(profile: HardwareProfile, heavyweight: bool) -> usize {
    let ceiling = if heavyweight { 6 } else { 4 };
    profile.inference_cores.clamp(1, ceiling)
}

/// Physical performance cores on Windows.
///
/// `available_parallelism` counts hardware threads, which made a
/// 4-core/8-thread laptop look like an 8-core Performance machine and gave
/// ONNX Runtime SMT siblings as if they were cores. On a hybrid CPU only the
/// fastest efficiency class counts, matching the macOS `perflevel0` probe:
/// work split evenly across P- and E-cores finishes when the E-core does.
#[cfg(windows)]
pub fn inference_cores() -> usize {
    windows_processor_core_records()
        .and_then(|records| performance_core_count(&records))
        // The API has existed since Windows 7, so this is a should-not-happen
        // path. Halving assumes SMT: an underestimate costs a thread, while an
        // overestimate is exactly the misclassification this probe fixes.
        .unwrap_or_else(|| (logical_cores() / 2).max(1))
}

/// Linux is a developer target only; it has no hybrid/SMT probe yet.
#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn inference_cores() -> usize {
    logical_cores()
}

/// Raw `RelationProcessorCore` records from `GetLogicalProcessorInformationEx`,
/// one per physical core across every processor group.
#[cfg(windows)]
fn windows_processor_core_records() -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_INSUFFICIENT_BUFFER};
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore,
    };

    // A core can be hot-added between the size query and the read; retry a
    // couple of times rather than trusting a stale length.
    for _ in 0..3 {
        let mut length = 0_u32;
        let sized = unsafe {
            GetLogicalProcessorInformationEx(
                RelationProcessorCore,
                std::ptr::null_mut(),
                &mut length,
            )
        };
        if sized != 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || length == 0 {
            return None;
        }
        // u64 storage keeps the records 8-byte aligned, as the API expects.
        let mut buffer = vec![0_u64; (length as usize).div_ceil(8)];
        let read = unsafe {
            GetLogicalProcessorInformationEx(
                RelationProcessorCore,
                buffer.as_mut_ptr().cast(),
                &mut length,
            )
        };
        if read != 0 {
            let bytes: Vec<u8> = buffer.iter().flat_map(|word| word.to_ne_bytes()).collect();
            return bytes.get(..length as usize).map(<[u8]>::to_vec);
        }
        if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return None;
        }
    }
    None
}

/// Count the cores in the highest efficiency class from packed
/// `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` records. Non-hybrid CPUs report
/// class 0 for every core, so they count every physical core. A malformed
/// buffer is `None` so the caller falls back rather than trusting a partial
/// count.
#[cfg(any(windows, test))]
fn performance_core_count(records: &[u8]) -> Option<usize> {
    // Relationship (i32), Size (u32), then PROCESSOR_RELATIONSHIP's Flags
    // (u8) and EfficiencyClass (u8).
    const RELATION_PROCESSOR_CORE: i32 = 0;
    const EFFICIENCY_CLASS_OFFSET: usize = 9;

    let mut classes = Vec::new();
    let mut offset = 0;
    while offset < records.len() {
        let record = records.get(offset..)?;
        let relationship = i32::from_ne_bytes(record.get(0..4)?.try_into().ok()?);
        let size = u32::from_ne_bytes(record.get(4..8)?.try_into().ok()?) as usize;
        if size <= EFFICIENCY_CLASS_OFFSET || size > record.len() {
            return None;
        }
        if relationship == RELATION_PROCESSOR_CORE {
            classes.push(record[EFFICIENCY_CLASS_OFFSET]);
        }
        offset += size;
    }
    let fastest = *classes.iter().max()?;
    Some(classes.iter().filter(|class| **class == fastest).count())
}

/// Coarse performance tier for model selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// NVIDIA GPU present — can run large models.
    Gpu,
    /// CPU only — use light/CPU-real-time models.
    Cpu,
}

pub fn detect_tier() -> Tier {
    if has_gpu() {
        Tier::Gpu
    } else {
        Tier::Cpu
    }
}

#[cfg(test)]
mod recommendation_tests {
    use super::*;

    fn profile(cores: usize, memory_gib: Option<u64>, fast_vector: bool) -> HardwareProfile {
        machine(cores, cores, memory_gib, fast_vector)
    }

    fn machine(
        physical: usize,
        logical: usize,
        memory_gib: Option<u64>,
        fast_vector: bool,
    ) -> HardwareProfile {
        HardwareProfile {
            logical_cores: logical,
            inference_cores: physical,
            memory_mib: memory_gib.map(|gib| gib * 1024),
            fast_vector,
        }
    }

    #[test]
    fn capability_tiers_are_conservative() {
        assert_eq!(
            performance_class(profile(3, Some(32), true)),
            PerformanceClass::Compact
        );
        assert_eq!(
            performance_class(profile(8, Some(8), true)),
            PerformanceClass::Standard
        );
        assert_eq!(
            performance_class(profile(12, None, true)),
            PerformanceClass::Standard
        );
        assert_eq!(
            performance_class(profile(8, Some(16), true)),
            PerformanceClass::Performance
        );
        assert_eq!(
            performance_class(profile(16, Some(64), false)),
            PerformanceClass::Compact
        );
        assert_eq!(
            performance_class(profile(8, Some(7), true)),
            PerformanceClass::Compact
        );
    }

    /// Windows used to report hardware threads here, so this ordinary laptop
    /// was a Performance machine and was handed eight Qwen threads on four
    /// cores. Tiers and thread pools are sized in physical cores.
    #[test]
    fn hardware_threads_do_not_promote_a_laptop() {
        let laptop = machine(4, 8, Some(16), true);
        assert_eq!(performance_class(laptop), PerformanceClass::Standard);
        assert_eq!(asr_threads(laptop, true), 4);
        assert_eq!(asr_threads(laptop, false), 4);

        let dual_core = machine(2, 4, Some(16), true);
        assert_eq!(performance_class(dual_core), PerformanceClass::Compact);
        assert_eq!(asr_threads(dual_core, false), 2);

        let hybrid = machine(6, 20, Some(32), true);
        assert_eq!(performance_class(hybrid), PerformanceClass::Standard);
        assert_eq!(asr_threads(hybrid, true), 6);

        let desktop = machine(16, 32, Some(64), true);
        assert_eq!(performance_class(desktop), PerformanceClass::Performance);
    }

    #[test]
    fn heavyweight_models_get_only_the_measured_thread_range() {
        assert_eq!(asr_threads(profile(2, Some(8), true), false), 2);
        assert_eq!(asr_threads(profile(32, Some(64), true), false), 4);
        assert_eq!(asr_threads(profile(2, Some(8), true), true), 2);
        assert_eq!(asr_threads(profile(6, Some(12), true), true), 6);
        assert_eq!(asr_threads(profile(32, Some(64), true), true), 6);
    }

    /// One packed `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` record as Windows
    /// lays it out: 48 bytes for a single-group core.
    fn record(relationship: i32, efficiency_class: u8) -> Vec<u8> {
        let mut bytes = vec![0_u8; 48];
        bytes[0..4].copy_from_slice(&relationship.to_ne_bytes());
        bytes[4..8].copy_from_slice(&48_u32.to_ne_bytes());
        bytes[8] = 1; // LTP_PC_SMT
        bytes[9] = efficiency_class;
        bytes
    }

    fn records(classes: &[u8]) -> Vec<u8> {
        classes.iter().flat_map(|class| record(0, *class)).collect()
    }

    #[test]
    fn performance_cores_are_the_fastest_efficiency_class() {
        // Uniform CPU: every core reports class 0 and every core counts.
        assert_eq!(performance_core_count(&records(&[0; 16])), Some(16));
        // Two P-cores and eight E-cores (a U-series laptop).
        assert_eq!(
            performance_core_count(&records(&[1, 1, 0, 0, 0, 0, 0, 0, 0, 0])),
            Some(2)
        );
        // Three tiers (P, E, low-power E): only the P-cores are pool-worthy.
        let mut three_tier = vec![2; 6];
        three_tier.extend([1; 8]);
        three_tier.extend([0; 2]);
        assert_eq!(performance_core_count(&records(&three_tier)), Some(6));
        // Records of another relationship never count as cores.
        let mut mixed = records(&[0; 4]);
        mixed.extend(record(1, 7));
        assert_eq!(performance_core_count(&mixed), Some(4));
    }

    #[test]
    fn malformed_processor_records_fall_back_instead_of_guessing() {
        assert_eq!(performance_core_count(&[]), None);
        let mut zero_size = records(&[0; 2]);
        zero_size[52..56].copy_from_slice(&0_u32.to_ne_bytes());
        assert_eq!(performance_core_count(&zero_size), None);
        let truncated = records(&[0; 2]);
        assert_eq!(performance_core_count(&truncated[..60]), None);
        let mut oversized = records(&[0; 1]);
        oversized[4..8].copy_from_slice(&4096_u32.to_ne_bytes());
        assert_eq!(performance_core_count(&oversized), None);
    }

    #[cfg(windows)]
    #[test]
    fn the_live_windows_probe_counts_physical_cores() {
        let records = windows_processor_core_records().expect("processor core records");
        let cores = performance_core_count(&records).expect("parsable records");
        assert!(cores >= 1);
        assert!(cores <= logical_cores(), "{cores} > {}", logical_cores());
        assert_eq!(inference_cores(), cores);
    }
}
