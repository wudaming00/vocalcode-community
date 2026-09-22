use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_SOURCE_TREE_ENTRIES: usize = 50_000;
pub const MAX_SOURCE_TREE_BYTES: u64 = 3 * 1024 * 1024 * 1024;
pub const MAX_SOURCE_TREE_DEPTH: usize = 128;

fn is_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub fn is_plain_file(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_file() && !is_reparse(metadata)
}

pub fn is_plain_directory(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_dir() && !is_reparse(metadata)
}

fn no_follow_open_options(options: &mut OpenOptions) {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        const O_NOFOLLOW: i32 = 0x0002_0000;
        options.custom_flags(O_NOFOLLOW);
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        const O_NOFOLLOW: i32 = 0x0000_0100;
        options.custom_flags(O_NOFOLLOW);
    }
}

pub fn open_plain_file_readonly(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    no_follow_open_options(&mut options);
    let file = options.open(path)?;
    if !is_plain_file(&file.metadata()?) {
        return Err(std::io::Error::other("path is not a plain file"));
    }
    Ok(file)
}

pub fn create_new_plain_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    no_follow_open_options(&mut options);
    let file = options.open(path)?;
    if !is_plain_file(&file.metadata()?) {
        return Err(std::io::Error::other("new destination is not a plain file"));
    }
    Ok(file)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: both pointers refer to live, NUL-terminated UTF-16 buffers for
    // the duration of the call.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

static PUBLISH_NONCE: AtomicU64 = AtomicU64::new(0);

/// Publishes a runtime asset through a private sibling file and an atomic
/// directory-entry replacement. The destination is never opened for writing,
/// so a concurrently introduced symlink or reparse point cannot redirect the
/// copy into another file.
pub fn publish_file_replacing(source: &Path, destination: &Path) -> Result<(), String> {
    let mut input = open_plain_file_readonly(source)
        .map_err(|err| format!("failed to open plain source {}: {err}", source.display()))?;
    let source_len = input
        .metadata()
        .map_err(|err| format!("failed to inspect source {}: {err}", source.display()))?
        .len();
    let parent = destination
        .parent()
        .ok_or_else(|| "publish destination has no parent".to_string())?;
    let parent_metadata = fs::symlink_metadata(parent)
        .map_err(|err| format!("failed to inspect publish directory: {err}"))?;
    if !is_plain_directory(&parent_metadata) {
        return Err("publish destination parent is not a plain directory".to_string());
    }

    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut temporary = None;
    for _ in 0..64 {
        let nonce = PUBLISH_NONCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".vocalcode-publish-{}-{epoch}-{nonce}.tmp",
            std::process::id()
        ));
        match create_new_plain_file(&candidate) {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("failed to create private publish file: {err}")),
        }
    }
    let (temporary_path, mut output) =
        temporary.ok_or_else(|| "failed to allocate a private publish file".to_string())?;
    let result = (|| {
        let copied = std::io::copy(&mut (&mut input).take(source_len + 1), &mut output)
            .map_err(|err| format!("failed to copy {}: {err}", source.display()))?;
        if copied != source_len {
            return Err(format!(
                "source file changed while publishing {}",
                source.display()
            ));
        }
        output
            .flush()
            .map_err(|err| format!("failed to flush {}: {err}", temporary_path.display()))?;
        output
            .sync_all()
            .map_err(|err| format!("failed to sync {}: {err}", temporary_path.display()))?;
        drop(output);
        atomic_replace(&temporary_path, destination).map_err(|err| {
            format!(
                "failed to atomically publish {} to {}: {err}",
                source.display(),
                destination.display()
            )
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn root_scripts(name: &std::ffi::OsStr) -> bool {
    #[cfg(windows)]
    {
        name.to_str()
            .is_some_and(|value| value.eq_ignore_ascii_case("scripts"))
    }
    #[cfg(not(windows))]
    {
        name == "scripts"
    }
}

struct TreeBudget {
    entries: usize,
    bytes: u64,
}

impl TreeBudget {
    fn new() -> Self {
        Self {
            entries: 0,
            bytes: 0,
        }
    }

    fn admit(&mut self, metadata: &fs::Metadata, depth: usize) -> Result<(), String> {
        if depth > MAX_SOURCE_TREE_DEPTH {
            return Err("source tree exceeds the depth limit".to_string());
        }
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| "source tree entry count overflowed".to_string())?;
        if self.entries > MAX_SOURCE_TREE_ENTRIES {
            return Err("source tree exceeds the entry-count limit".to_string());
        }
        if is_plain_file(metadata) {
            self.bytes = self
                .bytes
                .checked_add(metadata.len())
                .ok_or_else(|| "source tree byte count overflowed".to_string())?;
            if self.bytes > MAX_SOURCE_TREE_BYTES {
                return Err("source tree exceeds the byte limit".to_string());
            }
        }
        Ok(())
    }
}

fn sorted_entries(directory: &Path) -> Result<Vec<fs::DirEntry>, String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)
        .map_err(|err| format!("failed to enumerate {}: {err}", directory.display()))?
    {
        if entries.len() == MAX_SOURCE_TREE_ENTRIES {
            return Err("one source directory exceeds the entry-count limit".to_string());
        }
        entries.push(
            entry.map_err(|err| format!("failed to enumerate {}: {err}", directory.display()))?,
        );
    }
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

pub fn copy_source_tree(source: &Path, destination: &Path) -> Result<(), String> {
    let source_metadata = fs::symlink_metadata(source)
        .map_err(|err| format!("failed to inspect {}: {err}", source.display()))?;
    if !is_plain_directory(&source_metadata) {
        return Err("source root is not a plain directory".to_string());
    }
    if fs::symlink_metadata(destination).is_ok() {
        return Err("copy destination already exists".to_string());
    }
    fs::create_dir(destination)
        .map_err(|err| format!("failed to create {}: {err}", destination.display()))?;

    let mut budget = TreeBudget::new();
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf(), 0usize)];
    while let Some((source_dir, destination_dir, depth)) = pending.pop() {
        for entry in sorted_entries(&source_dir)? {
            if depth == 0 && root_scripts(&entry.file_name()) {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|err| format!("failed to inspect source entry: {err}"))?;
            if is_reparse(&metadata) {
                return Err("source tree contains a symlink or reparse point".to_string());
            }
            budget.admit(&metadata, depth + 1)?;
            let target = destination_dir.join(entry.file_name());
            if is_plain_directory(&metadata) {
                fs::create_dir(&target)
                    .map_err(|err| format!("failed to create {}: {err}", target.display()))?;
                pending.push((entry.path(), target, depth + 1));
            } else if is_plain_file(&metadata) {
                let mut input = File::open(entry.path())
                    .map_err(|err| format!("failed to open source file: {err}"))?;
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .map_err(|err| format!("failed to create {}: {err}", target.display()))?;
                let copied = std::io::copy(&mut (&mut input).take(metadata.len() + 1), &mut output)
                    .map_err(|err| format!("failed to copy {}: {err}", entry.path().display()))?;
                if copied != metadata.len() {
                    return Err(format!(
                        "source file changed while copying {}",
                        entry.path().display()
                    ));
                }
                output
                    .flush()
                    .map_err(|err| format!("failed to flush {}: {err}", target.display()))?;
            } else {
                return Err("source tree contains a special file".to_string());
            }
        }
    }
    Ok(())
}

pub fn validate_plain_tree(root: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|err| format!("failed to inspect {}: {err}", root.display()))?;
    if !is_plain_directory(&metadata) {
        return Err("tree root is not a plain directory".to_string());
    }
    let mut budget = TreeBudget::new();
    let mut pending: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((directory, depth)) = pending.pop() {
        for entry in sorted_entries(&directory)? {
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|err| format!("failed to inspect tree entry: {err}"))?;
            if is_reparse(&metadata) {
                return Err("tree contains a symlink or reparse point".to_string());
            }
            budget.admit(&metadata, depth + 1)?;
            if is_plain_directory(&metadata) {
                pending.push((entry.path(), depth + 1));
            } else if !is_plain_file(&metadata) {
                return Err("tree contains a special file".to_string());
            }
        }
    }
    Ok(())
}

pub fn remove_validated_direct_child(root: &Path, parent: &Path) -> Result<(), String> {
    let original_metadata = fs::symlink_metadata(root)
        .map_err(|err| format!("failed to inspect cleanup root: {err}"))?;
    if !is_plain_directory(&original_metadata) {
        return Err("cleanup root is not a plain directory".to_string());
    }
    let parent = parent
        .canonicalize()
        .map_err(|err| format!("failed to canonicalize cleanup parent: {err}"))?;
    let root = root
        .canonicalize()
        .map_err(|err| format!("failed to canonicalize cleanup root: {err}"))?;
    if root.parent() != Some(parent.as_path()) {
        return Err("cleanup root is not a direct child of its validated parent".to_string());
    }
    validate_plain_tree(&root)?;
    fs::remove_dir_all(&root)
        .map_err(|err| format!("failed to remove validated tree {}: {err}", root.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "vocalcode-source-tree-{}-{nonce}-{}",
                std::process::id(),
                NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn copy_is_pure_rust_bounded_and_skips_root_scripts() {
        let temp = TestDir::new();
        let source = temp.0.join("source");
        let destination = temp.0.join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("scripts")).unwrap();
        fs::write(source.join("scripts/ignored"), b"ignored").unwrap();
        fs::create_dir(source.join("include")).unwrap();
        fs::write(source.join("include/header.h"), b"header").unwrap();

        copy_source_tree(&source, &destination).unwrap();
        assert_eq!(
            fs::read(destination.join("include/header.h")).unwrap(),
            b"header"
        );
        assert!(!destination.join("scripts").exists());
        assert!(source.join("scripts/ignored").exists());
    }

    #[test]
    fn existing_destination_is_never_adopted() {
        let temp = TestDir::new();
        let source = temp.0.join("source");
        let destination = temp.0.join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        assert!(copy_source_tree(&source, &destination).is_err());
    }

    #[test]
    fn publish_atomically_replaces_an_existing_asset() {
        let temp = TestDir::new();
        let source = temp.0.join("source.dll");
        let destination = temp.0.join("destination.dll");
        fs::write(&source, b"new verified asset").unwrap();
        fs::write(&destination, b"old asset").unwrap();

        publish_file_replacing(&source, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new verified asset");
    }

    #[cfg(unix)]
    #[test]
    fn publish_replaces_a_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;
        let temp = TestDir::new();
        let source = temp.0.join("source.dylib");
        let victim = temp.0.join("victim");
        let destination = temp.0.join("destination.dylib");
        fs::write(&source, b"new verified asset").unwrap();
        fs::write(&victim, b"must survive").unwrap();
        symlink(&victim, &destination).unwrap();

        publish_file_replacing(&source, &destination).unwrap();
        assert_eq!(fs::read(victim).unwrap(), b"must survive");
        assert_eq!(fs::read(&destination).unwrap(), b"new verified asset");
        assert!(!fs::symlink_metadata(&destination)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn sparse_limit_plus_one_source_is_rejected() {
        let temp = TestDir::new();
        let source = temp.0.join("source");
        fs::create_dir(&source).unwrap();
        File::create(source.join("huge"))
            .unwrap()
            .set_len(MAX_SOURCE_TREE_BYTES + 1)
            .unwrap();
        assert!(copy_source_tree(&source, &temp.0.join("destination")).is_err());
    }

    #[test]
    fn cleanup_is_limited_to_a_plain_direct_child() {
        let temp = TestDir::new();
        let child = temp.0.join("child");
        fs::create_dir(&child).unwrap();
        fs::write(child.join("owned"), b"data").unwrap();
        remove_validated_direct_child(&child, &temp.0).unwrap();
        assert!(!child.exists());
        assert!(remove_validated_direct_child(&temp.0, &temp.0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn source_symlink_is_rejected() {
        use std::os::unix::fs::symlink;
        let temp = TestDir::new();
        let source = temp.0.join("source");
        fs::create_dir(&source).unwrap();
        symlink(&temp.0, source.join("link")).unwrap();
        assert!(copy_source_tree(&source, &temp.0.join("destination")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn cleanup_never_follows_a_directory_reparse_point() {
        use std::os::windows::fs::symlink_dir;
        let temp = TestDir::new();
        let victim = temp.0.join("victim");
        let redirected = temp.0.join("redirected");
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join("must-survive"), b"data").unwrap();
        if symlink_dir(&victim, &redirected).is_err() {
            return;
        }
        assert!(remove_validated_direct_child(&redirected, &temp.0).is_err());
        assert!(victim.join("must-survive").exists());
    }
}
