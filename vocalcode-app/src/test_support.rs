//! Test-only scratch directories whose cleanup can never fail a test.
//!
//! Windows releases file handles asynchronously. Defender and the indexer open
//! freshly written files (the bundled speech-gate model, WAV chunks) without
//! delete sharing and hold them for a scan that grows with CPU load, so an
//! immediate `remove_dir_all(..).unwrap()` flaked in cleanup even though the
//! behaviour under test had passed. It never caught our own leaks either:
//! `remove_dir_all` deletes around handles opened by `std::fs`, which always
//! allow deletion. Tests for which "every handle is closed" is a product
//! property say so with [`assert_directory_released`] instead.

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
        Self::new_in(&std::env::temp_dir(), label)
    }

    pub(crate) fn new_in(parent: &Path, label: &str) -> Self {
        for _ in 0..128 {
            let path = parent.join(format!(
                "vocalcode-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            // `create_dir`, not `create_dir_all`: a directory left behind by an
            // earlier process with the same id is skipped, never reused.
            match std::fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test directory {}: {error}", path.display()),
            }
        }
        panic!("could not create a unique test directory for {label}")
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
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

/// Asserts that nothing below `path` is still open, by renaming it away and
/// back. Windows refuses to rename a directory while any file beneath it has
/// an open handle, whatever that handle's sharing mode, so unlike deleting the
/// tree this catches a leaked `std::fs::File`. A scanner's brief hold is
/// retried like cleanup; a handle this process leaked fails every attempt.
/// Elsewhere the rename always succeeds and only proves the tree exists.
#[track_caller]
pub(crate) fn assert_directory_released(path: &Path) {
    let mut moved = path.as_os_str().to_owned();
    moved.push(".released");
    let moved = PathBuf::from(moved);
    if let Err(error) = retry(|| std::fs::rename(path, &moved)) {
        panic!(
            "{} still has an open handle beneath it: {error}",
            path.display()
        );
    }
    std::fs::rename(&moved, path).unwrap();
}

fn remove_tree(path: &Path) -> std::io::Result<()> {
    retry(|| match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    })
}

fn retry(mut operation: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    let mut attempt = 1;
    loop {
        match operation() {
            Ok(()) => return Ok(()),
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
    fn directories_are_unique_empty_and_removed_on_drop() {
        let first = TempDir::new("support");
        let second = TempDir::new("support");
        assert_ne!(first.path(), second.path());
        assert!(first.is_dir());
        assert_eq!(std::fs::read_dir(&first).unwrap().count(), 0);
        std::fs::create_dir_all(first.join("nested/deeper")).unwrap();
        std::fs::write(first.join("nested/deeper/file.bin"), b"bytes").unwrap();
        let path = first.path().to_path_buf();
        drop(first);
        assert!(!path.exists());
        // Deleting the tree early is not an error for the guard.
        std::fs::remove_dir_all(&second).unwrap();
        drop(second);
    }

    #[test]
    fn a_directory_left_by_a_recycled_process_id_is_never_reused() {
        let parent = TempDir::new("support-parent");
        let sequence = NEXT.load(Ordering::Relaxed);
        let stale: Vec<_> = (0..4)
            .map(|offset| {
                let stale = parent.join(format!(
                    "vocalcode-stale-{}-{}",
                    std::process::id(),
                    sequence + offset
                ));
                std::fs::create_dir(&stale).unwrap();
                std::fs::write(stale.join("old.txt"), b"old").unwrap();
                stale
            })
            .collect();
        let fresh = TempDir::new_in(&parent, "stale");
        assert_eq!(std::fs::read_dir(&fresh).unwrap().count(), 0);
        assert!(!stale.iter().any(|old| old == fresh.path()));
        drop(fresh);
        assert!(stale.iter().all(|old| old.join("old.txt").is_file()));
    }

    /// Stands in for Defender or the indexer: a reader that refuses delete
    /// sharing, which is what made `remove_dir_all(..).unwrap()` flake.
    #[cfg(windows)]
    fn open_like_a_scanner(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ_WRITE)
            .open(path)
            .unwrap()
    }

    #[test]
    fn a_briefly_held_file_delays_cleanup_without_failing_it() {
        let directory = TempDir::new("support-held");
        let path = directory.path().to_path_buf();
        std::fs::write(directory.join("model.onnx"), b"model").unwrap();
        #[cfg(windows)]
        {
            let scanner = open_like_a_scanner(&directory.join("model.onnx"));
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

    #[test]
    fn a_hold_that_outlasts_every_retry_is_an_error_not_a_panic() {
        let directory = TempDir::new("support-outlasted");
        std::fs::write(directory.join("model.onnx"), b"model").unwrap();
        #[cfg(windows)]
        {
            let scanner = open_like_a_scanner(&directory.join("model.onnx"));
            assert!(remove_tree(&directory).is_err());
            drop(scanner);
        }
        remove_tree(&directory).unwrap();
        assert!(!directory.exists());
        // Already gone: dropping the guard is still quiet.
        drop(directory);
    }

    #[test]
    fn released_directories_pass_and_open_handles_fail_on_windows() {
        let directory = TempDir::new("support-release");
        let tree = directory.join("meetings");
        std::fs::create_dir_all(tree.join("one/audio")).unwrap();
        let file = std::fs::File::create(tree.join("one/audio/a.wav")).unwrap();
        let held = std::panic::catch_unwind(|| assert_directory_released(&tree));
        assert_eq!(held.is_err(), cfg!(windows));
        drop(file);
        assert_directory_released(&tree);
        assert!(tree.join("one/audio/a.wav").is_file());
    }
}
