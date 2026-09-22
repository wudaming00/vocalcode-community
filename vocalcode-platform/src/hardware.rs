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
pub fn performance_class(profile: HardwareProfile) -> PerformanceClass {
    if profile.inference_cores <= 4
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
/// improving around four threads; Qwen improved through eight on the benchmark
/// machine and regressed at sixteen.
pub fn asr_threads(profile: HardwareProfile, heavyweight: bool) -> usize {
    let ceiling = if heavyweight {
        match performance_class(profile) {
            PerformanceClass::Compact => 4,
            PerformanceClass::Standard => 6,
            PerformanceClass::Performance => 8,
        }
    } else {
        4
    };
    profile.inference_cores.clamp(1, ceiling)
}

#[cfg(not(target_os = "macos"))]
pub fn inference_cores() -> usize {
    logical_cores()
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
        HardwareProfile {
            logical_cores: cores,
            inference_cores: cores,
            memory_mib: memory_gib.map(|gib| gib * 1024),
            fast_vector,
        }
    }

    #[test]
    fn capability_tiers_are_conservative() {
        assert_eq!(
            performance_class(profile(4, Some(32), true)),
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
    }

    #[test]
    fn heavyweight_models_get_only_the_measured_thread_range() {
        assert_eq!(asr_threads(profile(2, Some(8), true), false), 2);
        assert_eq!(asr_threads(profile(32, Some(64), true), false), 4);
        assert_eq!(asr_threads(profile(6, Some(12), true), true), 6);
        assert_eq!(asr_threads(profile(32, Some(64), true), true), 8);
    }
}
