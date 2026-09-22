//! Read-only smoke of the native metadata observer. No audio capture or titles
//! in the output. Run manually; not an assertion about real meeting accuracy.
fn main() {
    let started = std::time::Instant::now();
    let windows = vocalcode_platform::meeting_presence::observe();
    println!(
        "METADATA_PROBE windows={} confirmed={} mic_active={} elapsed_ms={}",
        windows.len(),
        windows
            .iter()
            .filter(|w| w.call_state == vocalcode_platform::meeting_presence::CallState::InCall)
            .count(),
        windows.iter().filter(|w| w.microphone_active).count(),
        started.elapsed().as_millis()
    );
}
