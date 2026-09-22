//! Crash-safe writes for the small pieces of mutable application state.
//!
//! Settings, counters and dictionary rules are read during startup.  Truncating
//! the destination in place means a power loss (or two app instances racing)
//! can turn a perfectly good file into an empty/half-TOML file.  Always write a
//! sibling, flush it, then atomically replace the destination.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

fn temp_path(path: &Path) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy();
    let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(".{name}.tmp-{}-{nonce}", std::process::id())))
}

fn sync_parent(parent: &Path) {
    // Directory handles are not normally openable as `File` on Windows. On
    // platforms that support it, flushing the directory also persists the new
    // name/link; the file contents themselves are always flushed first.
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
}

fn write_flushed_temp(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    for _ in 0..128 {
        let tmp = temp_path(path)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Settings are personal and recovery files can contain an old
            // reusable licence key. Do not let a permissive process umask make
            // app state readable by other local accounts.
            options.mode(0o600);
        }
        let mut file = match options.open(&tmp) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        return Ok(tmp);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary file",
    ))
}

/// Write `bytes` beside `path`, sync them, and replace `path` in one operation.
pub fn atomic_write_stream(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = temp_path(path)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    let result = write(&mut file).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| replace(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    } else {
        sync_parent(parent);
    }
    result
}

/// Write `bytes` beside `path`, sync them, and replace `path` in one operation.
pub fn atomic_write(path: &Path, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    std::fs::create_dir_all(parent)?;

    let tmp = write_flushed_temp(path, bytes.as_ref())?;
    let result = (|| {
        replace(&tmp, path)?;
        sync_parent(parent);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Write and flush `bytes`, then publish them only if `path` is still absent.
///
/// POSIX publishes through an atomic hard link; Windows uses a write-through
/// move without `REPLACE_EXISTING`. Both fail if the destination exists and
/// leave at least one durable name at every point in the transaction. This is
/// used by invalid-file recovery, where a concurrent writer owns any path it
/// recreates after the invalid source has been moved aside.
pub fn atomic_write_new(path: &Path, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    std::fs::create_dir_all(parent)?;

    let tmp = write_flushed_temp(path, bytes.as_ref())?;
    let result = (|| {
        publish_new(&tmp, path)?;
        sync_parent(parent);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Move `from` into a destination inside a freshly reserved private directory.
/// The caller guarantees that `to` has never existed. Windows additionally uses
/// write-through move semantics; POSIX durability comes from flushing both
/// affected directories after the atomic rename.
pub fn atomic_move_into_reserved(from: &Path, to: &Path) -> io::Result<()> {
    move_new(from, to)?;
    if let Some(parent) = from.parent() {
        sync_parent(parent);
    }
    if let Some(parent) = to.parent() {
        sync_parent(parent);
    }
    Ok(())
}

#[cfg(not(windows))]
fn publish_new(from: &Path, to: &Path) -> io::Result<()> {
    // `link` creates the destination iff it is absent. The flushed temporary
    // inode remains reachable throughout; unlinking its temporary name is safe.
    std::fs::hard_link(from, to)?;
    let _ = std::fs::remove_file(from);
    Ok(())
}

#[cfg(windows)]
fn publish_new(from: &Path, to: &Path) -> io::Result<()> {
    move_new(from, to)
}

#[cfg(not(windows))]
fn move_new(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(windows)]
fn move_new(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }

    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    // POSIX rename replaces an existing regular file atomically.
    std::fs::rename(from, to)
}

#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }

    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_file_without_leaving_a_temp() {
        let dir = std::env::temp_dir().join(format!(
            "vocalcode-atomic-write-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        std::fs::write(&path, b"old").unwrap();

        atomic_write(&path, b"new").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn no_clobber_write_publishes_only_when_destination_is_absent() {
        let dir = std::env::temp_dir().join(format!(
            "vocalcode-atomic-write-new-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");

        atomic_write_new(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        let error = atomic_write_new(&path, b"second").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
}
