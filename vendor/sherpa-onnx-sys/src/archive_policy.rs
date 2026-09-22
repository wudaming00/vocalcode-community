use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

pub const MAX_ARCHIVE_ENTRIES: usize = 20_000;
pub const MAX_EXTRACTED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafeEntryKind {
    File,
    Directory,
    Symlink,
}

fn windows_component_is_safe(value: &str) -> bool {
    if value.is_empty()
        || !value.is_ascii()
        || value.ends_with(['.', ' '])
        || value
            .bytes()
            .any(|byte| byte <= b' ' || b"<>:\"/\\|?*".contains(&byte))
    {
        return false;
    }
    let base = value
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) {
        return false;
    }
    if let Some(number) = base
        .strip_prefix("COM")
        .or_else(|| base.strip_prefix("LPT"))
    {
        if matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9") {
            return false;
        }
    }
    true
}

#[cfg(any(windows, target_os = "macos"))]
type SafePathKey = String;
#[cfg(not(any(windows, target_os = "macos")))]
type SafePathKey = PathBuf;

#[cfg(any(windows, target_os = "macos"))]
fn safe_path_key(path: &Path) -> SafePathKey {
    path.to_string_lossy().to_ascii_lowercase()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn safe_path_key(path: &Path) -> SafePathKey {
    path.to_path_buf()
}

pub fn aliases_reserved_root(path: &Path, reserved: &str) -> bool {
    let Some(Component::Normal(first)) = path.components().next() else {
        return false;
    };
    #[cfg(any(windows, target_os = "macos"))]
    {
        first
            .to_str()
            .is_some_and(|value| value.eq_ignore_ascii_case(reserved))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        first == reserved
    }
}

pub fn safe_relative_path(path: &Path) -> Result<PathBuf, String> {
    let mut safe = PathBuf::new();
    let mut components = 0usize;
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                if part.is_empty() {
                    return Err("archive entry contains an empty path component".to_string());
                }
                // Default Windows and macOS filesystems both admit aliases
                // that a byte-for-byte path set cannot detect. Restrict those
                // targets to portable ASCII components, then case-fold the
                // duplicate key. Rejecting non-ASCII also removes Unicode
                // normalization aliases on macOS.
                #[cfg(any(windows, target_os = "macos"))]
                {
                    let value = part.to_str().ok_or_else(|| {
                        "archive path is not representable as safe portable text".to_string()
                    })?;
                    if !windows_component_is_safe(value) {
                        return Err(format!(
                            "archive entry contains an unsafe portable component `{value}`"
                        ));
                    }
                }
                components = components
                    .checked_add(1)
                    .ok_or_else(|| "archive path component count overflowed".to_string())?;
                if components > 128 {
                    return Err("archive entry path is too deeply nested".to_string());
                }
                safe.push(part);
                if safe.as_os_str().as_encoded_bytes().len() > 4096 {
                    return Err("archive entry path is too long".to_string());
                }
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err("archive entry path is not a safe relative path".to_string());
            }
        }
    }
    if safe.as_os_str().is_empty() {
        return Err("archive entry path is empty".to_string());
    }
    Ok(safe)
}

/// Resolves an archive symlink target using POSIX tar semantics while keeping
/// the result inside the extraction root. The returned path is root-relative;
/// callers still have to require that it names a regular archive entry before
/// materializing it.
pub fn resolve_relative_link_target(link: &Path, target: &Path) -> Result<PathBuf, String> {
    let link = safe_relative_path(link)?;
    let mut resolved: Vec<PathBuf> = link
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .map(|component| match component {
            Component::Normal(part) => PathBuf::from(part),
            _ => unreachable!("safe_relative_path returned a non-normal component"),
        })
        .collect();

    for component in target.components() {
        match component {
            Component::Normal(part) => resolved.push(PathBuf::from(part)),
            Component::CurDir => {}
            Component::ParentDir => {
                if resolved.pop().is_none() {
                    return Err("archive symlink target escapes the extraction root".to_string());
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("archive symlink target must be relative".to_string());
            }
        }
        if resolved.len() > 128 {
            return Err("archive symlink target is too deeply nested".to_string());
        }
    }

    let mut normalized = PathBuf::new();
    for component in resolved {
        normalized.push(component);
    }
    safe_relative_path(&normalized)
}

pub struct ArchiveBudget {
    paths: HashMap<SafePathKey, SafeEntryKind>,
    entries: usize,
    extracted_bytes: u64,
}

impl ArchiveBudget {
    pub fn new() -> Self {
        Self {
            paths: HashMap::new(),
            entries: 0,
            extracted_bytes: 0,
        }
    }

    pub fn admit(
        &mut self,
        path: &Path,
        kind: SafeEntryKind,
        size: u64,
    ) -> Result<PathBuf, String> {
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| "archive entry count overflowed".to_string())?;
        if self.entries > MAX_ARCHIVE_ENTRIES {
            return Err(format!(
                "archive contains more than {MAX_ARCHIVE_ENTRIES} entries"
            ));
        }

        if kind != SafeEntryKind::File && size != 0 {
            return Err("archive directory or symlink entry has a nonzero size".to_string());
        }
        self.extracted_bytes = self
            .extracted_bytes
            .checked_add(size)
            .ok_or_else(|| "archive extracted-size total overflowed".to_string())?;
        if self.extracted_bytes > MAX_EXTRACTED_BYTES {
            return Err(format!(
                "archive expands beyond the {MAX_EXTRACTED_BYTES}-byte safety limit"
            ));
        }

        let safe = safe_relative_path(path)?;
        if self.paths.insert(safe_path_key(&safe), kind).is_some() {
            return Err(format!("archive repeats path `{}`", safe.display()));
        }
        Ok(safe)
    }

    pub fn kind_of(&self, path: &Path) -> Option<SafeEntryKind> {
        self.paths.get(&safe_path_key(path)).copied()
    }

    pub fn admit_materialized_bytes(&mut self, size: u64) -> Result<(), String> {
        self.extracted_bytes = self
            .extracted_bytes
            .checked_add(size)
            .ok_or_else(|| "archive extracted-size total overflowed".to_string())?;
        if self.extracted_bytes > MAX_EXTRACTED_BYTES {
            return Err(format!(
                "archive expands beyond the {MAX_EXTRACTED_BYTES}-byte safety limit"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_normal_relative_paths_are_accepted() {
        assert_eq!(
            safe_relative_path(Path::new("root/lib/file.lib")).unwrap(),
            PathBuf::from("root/lib/file.lib")
        );
        for path in ["", ".", "../escape", "root/../escape", "/absolute"] {
            assert!(safe_relative_path(Path::new(path)).is_err(), "{path}");
        }
    }

    #[test]
    fn relative_link_targets_are_resolved_without_escaping() {
        assert_eq!(
            resolve_relative_link_target(
                Path::new("root/lib/libcurrent.dylib"),
                Path::new("libversioned.dylib")
            )
            .unwrap(),
            PathBuf::from("root/lib/libversioned.dylib")
        );
        assert_eq!(
            resolve_relative_link_target(
                Path::new("root/lib/current"),
                Path::new("../bin/versioned")
            )
            .unwrap(),
            PathBuf::from("root/bin/versioned")
        );
        for target in ["/outside", "../../../outside"] {
            assert!(
                resolve_relative_link_target(Path::new("root/lib/current"), Path::new(target))
                    .is_err()
            );
        }
    }

    #[test]
    fn windows_alias_rules_cover_devices_ads_and_normalization() {
        for component in [
            "CON",
            "con.txt",
            "PRN",
            "AUX.log",
            "NUL",
            "COM1",
            "lpt9.txt",
            "file:stream",
            "trailing.",
            "trailing ",
            "embedded space",
            "question?",
            "nonascii-é",
        ] {
            assert!(!windows_component_is_safe(component), "{component}");
        }
        assert!(windows_component_is_safe("libsherpa-onnx-c-api.dll"));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn case_insensitive_filesystem_aliases_are_duplicate_paths() {
        let mut budget = ArchiveBudget::new();
        budget
            .admit(Path::new("Root/Lib/File.dll"), SafeEntryKind::File, 1)
            .unwrap();
        assert!(budget
            .admit(Path::new("root/lib/file.DLL"), SafeEntryKind::File, 1)
            .is_err());
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn unix_names_are_not_subject_to_windows_alias_rules() {
        assert!(safe_relative_path(Path::new("root/CON")).is_ok());
        assert!(safe_relative_path(Path::new("root/file:stream")).is_ok());
        assert!(safe_relative_path(Path::new("root/trailing.")).is_ok());
    }

    #[test]
    fn reserved_root_rejects_itself_and_descendants() {
        assert!(aliases_reserved_root(
            Path::new(".vocalcode-marker"),
            ".vocalcode-marker"
        ));
        assert!(aliases_reserved_root(
            Path::new(".vocalcode-marker/child"),
            ".vocalcode-marker"
        ));
        assert!(!aliases_reserved_root(
            Path::new("safe/.vocalcode-marker"),
            ".vocalcode-marker"
        ));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn reserved_root_comparison_follows_case_insensitive_aliasing() {
        assert!(aliases_reserved_root(
            Path::new(".VOCALCODE-MARKER/payload"),
            ".vocalcode-marker"
        ));
    }

    #[test]
    fn duplicate_paths_are_rejected() {
        let mut budget = ArchiveBudget::new();
        budget
            .admit(Path::new("root/file"), SafeEntryKind::File, 1)
            .unwrap();
        assert!(budget
            .admit(Path::new("root/file"), SafeEntryKind::File, 1)
            .is_err());
    }

    #[test]
    fn extracted_size_limit_is_exact() {
        let mut budget = ArchiveBudget::new();
        budget
            .admit(
                Path::new("root/a"),
                SafeEntryKind::File,
                MAX_EXTRACTED_BYTES,
            )
            .unwrap();
        assert!(budget
            .admit(Path::new("root/b"), SafeEntryKind::File, 1)
            .is_err());
    }

    #[test]
    fn directories_cannot_smuggle_payload_bytes() {
        let mut budget = ArchiveBudget::new();
        assert!(budget
            .admit(Path::new("root"), SafeEntryKind::Directory, 1)
            .is_err());
    }

    #[test]
    fn entry_count_limit_is_exact() {
        let mut budget = ArchiveBudget::new();
        for index in 0..MAX_ARCHIVE_ENTRIES {
            budget
                .admit(Path::new(&format!("root/{index}")), SafeEntryKind::File, 0)
                .unwrap();
        }
        assert!(budget
            .admit(Path::new("root/overflow"), SafeEntryKind::File, 0)
            .is_err());
    }
}
