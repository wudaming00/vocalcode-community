//! "Mute other audio while dictating".
//!
//! On Windows this mutes every *other* process's active playback session on
//! the default output device while a dictation is recording, and afterwards
//! unmutes exactly the sessions it muted. It deliberately never mutes the
//! whole endpoint: that would also silence VocalCode's own start/stop cues,
//! and restoring an endpoint the person muted themselves mid-dictation would
//! undo their choice. A session the person had already muted, or unmuted by
//! hand while dictating, is left as they set it.
//!
//! All COM work happens on one worker thread that owns the session handles;
//! callers only publish the desired state, so the input/engine thread never
//! waits on the audio stack. Other platforms: a no-op.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

struct Command {
    mute: bool,
    ack: Option<mpsc::Sender<()>>,
}

static DESIRED: AtomicBool = AtomicBool::new(false);
static WORKER: OnceLock<Mutex<Option<mpsc::Sender<Command>>>> = OnceLock::new();

fn worker() -> Option<mpsc::Sender<Command>> {
    let slot = WORKER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Command>();
        let spawned = std::thread::Builder::new()
            .name("vocalcode-mute".into())
            .spawn(move || run(rx));
        Mutex::new(spawned.ok().map(|_| tx))
    });
    slot.lock().ok().and_then(|tx| tx.clone())
}

/// Publish whether other audio should be muted now. Cheap and idempotent:
/// only a change of state reaches the worker.
pub fn set_others_muted(mute: bool) {
    if DESIRED.swap(mute, Ordering::AcqRel) == mute {
        return;
    }
    if let Some(tx) = worker() {
        let _ = tx.send(Command { mute, ack: None });
    }
}

/// Restore anything this process muted and wait briefly for it to happen.
/// For shutdown, where an asynchronous request could die with the process.
pub fn restore_blocking(timeout: Duration) {
    DESIRED.store(false, Ordering::Release);
    let Some(tx) = worker() else { return };
    let (ack, done) = mpsc::channel();
    if tx
        .send(Command {
            mute: false,
            ack: Some(ack),
        })
        .is_ok()
    {
        let _ = done.recv_timeout(timeout);
    }
}

fn run(rx: mpsc::Receiver<Command>) {
    let mut held = platform::Held::default();
    while let Ok(first) = rx.recv() {
        // Coalesce a burst (press/release/press) into its final state.
        let mut want = first.mute;
        let mut acks: Vec<mpsc::Sender<()>> = first.ack.into_iter().collect();
        while let Ok(next) = rx.try_recv() {
            want = next.mute;
            acks.extend(next.ack);
        }
        if want {
            held.mute_others();
        } else {
            held.restore();
        }
        for ack in acks {
            let _ = ack.send(());
        }
    }
    held.restore();
}

#[cfg(windows)]
mod platform {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
        IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    #[derive(Default)]
    pub(super) struct Held {
        com: bool,
        muted: Vec<ISimpleAudioVolume>,
    }

    impl Held {
        fn ensure_com(&mut self) {
            if !self.com {
                // S_FALSE (already initialised) is fine; this thread is ours.
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                self.com = true;
            }
        }

        pub(super) fn mute_others(&mut self) {
            if !self.muted.is_empty() {
                return;
            }
            self.ensure_com();
            match unsafe { mute_other_sessions() } {
                Ok(muted) => {
                    if !muted.is_empty() {
                        log::info!("mute: muted {} other playback session(s)", muted.len());
                    }
                    self.muted = muted;
                }
                Err(error) => log::warn!("mute: could not mute other audio: {error}"),
            }
        }

        pub(super) fn restore(&mut self) {
            let mut restored = 0;
            for volume in self.muted.drain(..) {
                // Only undo our own change: if the person unmuted it by hand
                // meanwhile, it is already how they want it.
                let still_muted = unsafe { volume.GetMute() }
                    .map(|muted| muted.as_bool())
                    .unwrap_or(false);
                if still_muted && unsafe { volume.SetMute(false, std::ptr::null()) }.is_ok() {
                    restored += 1;
                }
            }
            if restored > 0 {
                log::info!("mute: restored {restored} playback session(s)");
            }
        }
    }

    unsafe fn mute_other_sessions() -> windows::core::Result<Vec<ISimpleAudioVolume>> {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
        let sessions = manager.GetSessionEnumerator()?;
        let own = std::process::id();
        let mut muted = Vec::new();
        for index in 0..sessions.GetCount()? {
            let Ok(control) = sessions.GetSession(index) else {
                continue;
            };
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            // System sounds (notification dings) are not "music or video".
            if control2.IsSystemSoundsSession() == windows::Win32::Foundation::S_OK {
                continue;
            }
            let Ok(pid) = control2.GetProcessId() else {
                continue;
            };
            if pid == 0 || pid == own {
                continue;
            }
            if control.GetState().ok() != Some(AudioSessionStateActive) {
                continue;
            }
            let Ok(volume) = control.cast::<ISimpleAudioVolume>() else {
                continue;
            };
            let already = volume.GetMute().map(|m| m.as_bool()).unwrap_or(true);
            if already {
                continue;
            }
            if volume.SetMute(true, std::ptr::null()).is_ok() {
                muted.push(volume);
            }
        }
        Ok(muted)
    }
}

#[cfg(not(windows))]
mod platform {
    #[derive(Default)]
    pub(super) struct Held;

    impl Held {
        pub(super) fn mute_others(&mut self) {}
        pub(super) fn restore(&mut self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_requests_are_idempotent_and_restore_returns() {
        // Publishing the same state twice is a no-op, and a blocking restore
        // returns within its bound even when nothing was muted.
        set_others_muted(false);
        set_others_muted(false);
        let started = std::time::Instant::now();
        restore_blocking(Duration::from_millis(500));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!DESIRED.load(Ordering::Acquire));
    }
}
