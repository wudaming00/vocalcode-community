//! Test-only scratch directories whose cleanup can never fail a test.
//!
//! Windows releases file handles asynchronously: Defender and the indexer open
//! freshly written files without delete sharing and hold them for a scan that
//! grows with CPU load, so an immediate `remove_dir_all(..).unwrap()` flaked
//! in cleanup after the behaviour under test had passed. (It never caught our
//! own leaks: `remove_dir_all` deletes around `std::fs` handles, which always
//! allow deletion.) The app crate keeps the same guard in its `test_support`.

use std::io::Write as _;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const ATTEMPTS: u32 = 10;
/// Linear backoff: 50 ms, 100 ms, ... 450 ms, about 2.25 s in total.
const BACKOFF_STEP: Duration = Duration::from_millis(50);

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh, empty directory that is removed (best effort) when dropped.
#[derive(Debug)]
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub(crate) fn new(label: &str) -> Self {
        for _ in 0..128 {
            let path = std::env::temp_dir().join(format!(
                "vocalcode-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            // A directory left behind by an earlier process with the same id
            // is skipped, never reused.
            match std::fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test directory {}: {error}", path.display()),
            }
        }
        panic!("could not create a unique test directory for {label}")
    }
}

impl Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Err(error) = remove_tree(&self.path) {
            // Leaking a temp directory is harmless; failing the test is not.
            // `eprintln!` would panic if stderr were closed, so write directly.
            let _ = writeln!(
                std::io::stderr(),
                "warning: left test directory {} behind: {error}",
                self.path.display()
            );
        }
    }
}

fn remove_tree(path: &Path) -> std::io::Result<()> {
    let mut attempt = 1;
    loop {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) if attempt == ATTEMPTS => return Err(error),
            Err(_) => std::thread::sleep(BACKOFF_STEP * attempt),
        }
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_briefly_held_file_delays_cleanup_without_failing_it() {
        let directory = TempDir::new("meeting-support");
        let path = directory.to_path_buf();
        std::fs::create_dir(directory.join("audio")).unwrap();
        std::fs::write(directory.join("audio/chunk.wav"), b"RIFF").unwrap();
        #[cfg(windows)]
        {
            // Stand in for a scanner: a reader that refuses delete sharing.
            use std::os::windows::fs::OpenOptionsExt as _;
            const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;
            let scanner = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ_WRITE)
                .open(directory.join("audio/chunk.wav"))
                .unwrap();
            assert!(std::fs::remove_dir_all(&path).is_err());
            let releaser = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(120));
                drop(scanner);
            });
            drop(directory);
            releaser.join().unwrap();
        }
        #[cfg(not(windows))]
        drop(directory);
        assert!(!path.exists(), "cleanup gave up while the hold was brief");
    }
}
