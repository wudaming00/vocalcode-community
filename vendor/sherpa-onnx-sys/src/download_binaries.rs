use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use serde_json::Value;

#[path = "download_limits.rs"]
mod download_limits;
pub use download_limits::MAX_SHERPA_ARCHIVE_BYTES;
use download_limits::{parse_content_length, read_archive_limited};

#[path = "archive_policy.rs"]
mod archive_policy;
use crate::source_tree::{
    create_new_plain_file, is_plain_directory, is_plain_file, open_plain_file_readonly,
    remove_validated_direct_child,
};
use archive_policy::{
    aliases_reserved_root, resolve_relative_link_target, ArchiveBudget, SafeEntryKind,
};

pub const EXTRACTION_CONTAINER: &str = "vx";
const EXTRACTION_GENERATION_PREFIX: &str = "g-";
const EXTRACTION_MARKER: &str = ".vocalcode-sherpa-extraction-v1";
const EXTRACTION_MARKER_PREFIX: &[u8] = b"VOCALCODE_SHERPA_EXTRACTION_V1\n";
const MAX_EXTRACTION_GENERATIONS: usize = 128;
pub const MIN_EXTRACTION_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

// Prebuilt sherpa-onnx doesn't have Cuda support
#[cfg(all(
    any(target_os = "windows", target_os = "linux"),
    feature = "download-binaries",
    feature = "cuda"
))]
compile_error!(
    "The 'download-binaries' and 'cuda' features cannot be enabled at the same time.\n\
    To resolve this, please disable the 'download-binaries' feature when using 'cuda'.\n\
    For example, in your Cargo.toml:\n\
    [dependencies]\n\
    sherpa-rs = { default-features = false, features = [\"cuda\"] }"
);

// Prebuilt sherpa-onnx doesn't have DirectML support
#[cfg(all(windows, feature = "download-binaries", feature = "directml"))]
compile_error!(
    "The 'download-binaries' and 'directml' features cannot be enabled at the same time.\n\
    To resolve this, please disable the 'download-binaries' feature when using 'directml'.\n\
    For example, in your Cargo.toml:\n\
    [dependencies]\n\
    sherpa-rs = { default-features = false, features = [\"directml\"] }"
);

// Prebuilt sherpa-onnx does not include TTS in static builds.
#[cfg(all(
    windows,
    feature = "download-binaries",
    feature = "static",
    feature = "tts"
))]
compile_error!(
    "The 'download-binaries', 'static', and 'tts' features cannot be enabled at the same time.\n\
    To resolve this, please disable the 'tts' feature when using 'static' and 'download-binaries' together.\n\
    For example, in your Cargo.toml:\n\
    [dependencies]\n\
    sherpa-rs = { default-features = false, features = [\"static\", \"tts\"] }"
);

macro_rules! debug_log {
    ($($arg:tt)*) => {
        // SHERPA_BUILD_DEBUG=1 cargo build
        if std::env::var("SHERPA_BUILD_DEBUG").unwrap_or_default() == "1" {
            println!("cargo:warning=[DEBUG] {}", format!($($arg)*));
        }
    };
}

pub fn fetch_file(source_url: &str) -> Vec<u8> {
    let resp = ureq::AgentBuilder::new()
        .try_proxy_from_env(true)
        .build()
        .get(source_url)
        .timeout(std::time::Duration::from_secs(1800))
        .call()
        .unwrap_or_else(|err| panic!("Failed to GET `{source_url}`: {err}"));

    let content_lengths = resp.all("Content-Length");
    let len = parse_content_length(&content_lengths, MAX_SHERPA_ARCHIVE_BYTES)
        .unwrap_or_else(|err| panic!("Refusing archive from `{source_url}`: {err}"));
    debug_log!("Fetch file {} {}", source_url, len);
    read_archive_limited(resp.into_reader(), len, MAX_SHERPA_ARCHIVE_BYTES)
        .unwrap_or_else(|err| panic!("Failed to download from `{source_url}`: {err}"))
}

static DIST_CONTENT: &str = include_str!("../dist.json");
static DIST_CHECKSUM_CONTENT: &str = include_str!("../checksum.txt");
lazy_static::lazy_static! {
    pub static ref DIST_TABLE: DistTable = DistTable::new(DIST_CONTENT);
    pub static ref DIST_CHECKSUM: HashMap<String, String> = {
        DIST_CHECKSUM_CONTENT
            .lines()
            .map(|line| {
                let mut parts = line.split_whitespace();
                let key = parts.next().unwrap().to_string();
                let value = parts.next().unwrap().to_string();
                (key, value)
            })
            .collect()
    };
}

#[derive(Debug, Deserialize)]
pub struct DistTable {
    pub tag: String,
    pub url: String,
    pub targets: HashMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct Dist {
    pub url: String,
    pub name: String,
    pub checksum: String,
    pub libs: Option<Vec<String>>, // Paths to the extracted libraries
}

impl DistTable {
    fn new(content: &str) -> Self {
        let mut table: DistTable = serde_json::from_str(content)
            .unwrap_or_else(|_| panic!("Failed to parse dist.json: {}", content));
        table.url = table.url.replace("{tag}", &table.tag);
        for value in table.targets.values_mut() {
            // expand static with {tag}
            if let Some(static_value) = value.get("static") {
                let static_value = static_value.as_str().unwrap();
                value["static"] = Value::String(static_value.replace("{tag}", &table.tag));
            }
            // expand dynamic with {tag}
            if let Some(dynamic_value) = value.get("dynamic") {
                let dynamic_value = dynamic_value.as_str().unwrap();
                value["dynamic"] = Value::String(dynamic_value.replace("{tag}", &table.tag));
            }
            // expand archive with {tag}
            if let Some(archive_value) = value.get("archive") {
                let archive_value = archive_value.as_str().unwrap();
                value["archive"] = Value::String(archive_value.replace("{tag}", &table.tag));
            }
        }
        table
    }

    pub fn get(&self, target: &str, is_dynamic: &mut bool) -> Option<Dist> {
        debug_log!("Extracting dist for target: {}", target);
        // debug_log!("dist table: {:?}", self);
        let target_dist = if target.contains("android") {
            self.targets.get("android").unwrap()
        } else if target.contains("ios") {
            self.targets.get("ios").unwrap()
        } else {
            self.targets
                .get(target)
                .unwrap_or_else(||
                    panic!("Target {} not found. try to disable download-feature with --no-default-features.", target)
                )
        };
        debug_log!(
            "raw target_dist: {:?}",
            serde_json::to_string(target_dist).unwrap()
        );
        let archive = if target_dist.get("archive").is_some() {
            // archive name
            // static/dynamic located in 'is_dynamic' field
            target_dist.get("archive").unwrap().as_str().unwrap()
        } else if *is_dynamic {
            // dynamic archive name
            target_dist.get("dynamic").unwrap().as_str().unwrap()
        } else {
            // static archive name
            target_dist.get("static").unwrap().as_str().unwrap()
        };
        let name = archive.replace(".tar.bz2", "");
        let name = name.replace(".tar.gz", "");

        let libs: Option<Vec<String>> = target_dist["targets"][target].as_array().map(|libs| {
            libs.iter()
                .map(|lib| lib.as_str().unwrap().to_string())
                .collect()
        });

        let url = self.url.replace("{archive}", archive);
        let checksum = DIST_CHECKSUM.get(archive)?;

        // modify is_dynamic
        debug_log!("checking is_dynamic");
        if let Some(target_dist) = target_dist.get("is_dynamic") {
            *is_dynamic = target_dist.as_bool()?;
            debug_log!("is_dynamic: {}", *is_dynamic);
        }

        let dist = Dist {
            url,
            name,
            checksum: checksum.to_string(),
            libs,
        };
        debug_log!("dist: {:?}", dist);
        Some(dist)
    }
}

#[allow(unused)]
fn hex_str_to_bytes(c: impl AsRef<[u8]>) -> Vec<u8> {
    fn nibble(c: u8) -> u8 {
        match c {
            b'A'..=b'F' => c - b'A' + 10,
            b'a'..=b'f' => c - b'a' + 10,
            b'0'..=b'9' => c - b'0',
            _ => panic!(),
        }
    }

    c.as_ref()
        .chunks(2)
        .map(|n| (nibble(n[0]) << 4) | nibble(n[1]))
        .collect()
}

fn bytes_to_hex_str(bytes: Vec<u8>) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

pub fn sha256(buf: &[u8]) -> String {
    let hash_bytes: Vec<u8> = <sha2::Sha256 as sha2::Digest>::digest(buf).to_vec();
    bytes_to_hex_str(hash_bytes)
}

pub fn read_verified_cached_archive(path: &Path, expected_hash: &str) -> Option<Vec<u8>> {
    // Open without following the final path component, then validate the
    // opened object. This closes the check/open race around a cache candidate.
    let file = open_plain_file_readonly(path).ok()?;
    let metadata = file.metadata().ok()?;
    if metadata.len() == 0 {
        return None;
    }
    let declared_length = metadata.len();
    if declared_length > MAX_SHERPA_ARCHIVE_BYTES {
        return None;
    }
    let bytes = read_archive_limited(file, declared_length, MAX_SHERPA_ARCHIVE_BYTES).ok()?;
    (sha256(&bytes) == expected_hash).then_some(bytes)
}

pub fn cache_verified_archive_if_absent(path: &Path, bytes: &[u8]) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Ok(parent_metadata) = fs::symlink_metadata(parent) else {
        return;
    };
    if !is_plain_directory(&parent_metadata) {
        return;
    }
    let Ok(mut file) = create_new_plain_file(path) else {
        return;
    };
    // A partial file left by an interrupted build is harmless: every reuse
    // re-reads it through the hard limit and verifies the pinned checksum.
    if file.write_all(bytes).is_ok() {
        let _ = file.sync_all();
    }
}

fn has_valid_extraction_marker(generation: &Path) -> bool {
    let marker = generation.join(EXTRACTION_MARKER);
    let Ok(marker_metadata) = fs::symlink_metadata(&marker) else {
        return false;
    };
    let expected_marker_len = EXTRACTION_MARKER_PREFIX.len() + 64 + 1;
    if !is_plain_file(&marker_metadata) || marker_metadata.len() != expected_marker_len as u64 {
        return false;
    }
    let Ok(marker_file) = open_plain_file_readonly(&marker) else {
        return false;
    };
    let Ok(marker_bytes) = read_archive_limited(
        marker_file,
        expected_marker_len as u64,
        expected_marker_len as u64,
    ) else {
        return false;
    };
    marker_bytes.starts_with(EXTRACTION_MARKER_PREFIX)
        && marker_bytes.last() == Some(&b'\n')
        && marker_bytes[EXTRACTION_MARKER_PREFIX.len()..expected_marker_len - 1]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit())
}

pub fn isolated_extraction_dir(out_dir: &Path, checksum: &str) -> PathBuf {
    if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        panic!("Sherpa checksum is not a strict SHA-256 hex digest");
    }
    if !out_dir.is_absolute() {
        panic!("Sherpa OUT_DIR must be absolute");
    }
    // Keep this lexical absolute path for linker inputs. On Windows,
    // canonicalize() returns a \\?\ verbatim path that link.exe cannot open for
    // these import libraries. The canonical forms below remain the authority
    // for containment checks.
    let linker_container = out_dir.join(EXTRACTION_CONTAINER);
    let canonical_out_dir = out_dir
        .canonicalize()
        .unwrap_or_else(|err| panic!("Failed to canonicalize OUT_DIR: {err}"));
    match fs::symlink_metadata(&linker_container) {
        Ok(metadata) => {
            if !is_plain_directory(&metadata) {
                panic!("Refusing unsafe sherpa extraction container");
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&linker_container)
                .unwrap_or_else(|err| panic!("Failed to create extraction container: {err}"));
        }
        Err(err) => panic!("Failed to inspect extraction container: {err}"),
    }
    let canonical_container = linker_container
        .canonicalize()
        .unwrap_or_else(|err| panic!("Failed to canonicalize extraction container: {err}"));
    if canonical_container.parent() != Some(canonical_out_dir.as_path()) {
        panic!("Sherpa extraction container escaped OUT_DIR");
    }

    let mut entries = Vec::with_capacity(MAX_EXTRACTION_GENERATIONS);
    for entry in fs::read_dir(&canonical_container)
        .unwrap_or_else(|err| panic!("Failed to enumerate extraction generations: {err}"))
    {
        if entries.len() == MAX_EXTRACTION_GENERATIONS {
            panic!("Sherpa extraction container exceeds its generation limit");
        }
        entries.push(
            entry.unwrap_or_else(|err| panic!("Failed to enumerate extraction generation: {err}")),
        );
    }
    let now = SystemTime::now();
    let mut retained_entries = 0usize;
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            retained_entries += 1;
            continue;
        };
        if !name.starts_with(EXTRACTION_GENERATION_PREFIX) {
            retained_entries += 1;
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())
            .unwrap_or_else(|err| panic!("Failed to inspect extraction generation: {err}"));
        if !is_plain_directory(&metadata) {
            retained_entries += 1;
            continue;
        }
        if !has_valid_extraction_marker(&entry.path()) {
            retained_entries += 1;
            continue;
        }
        // Cargo can run multiple build-script instances concurrently for
        // different feature/target closures. Their linker inputs outlive the
        // build-script process, so immediately deleting a prior generation can
        // remove a .lib while another rustc is about to link it. Release jobs
        // time out well inside this retention window and use a fresh target
        // directory; only an old, owned generation is eligible for cleanup.
        let marker_modified = fs::symlink_metadata(entry.path().join(EXTRACTION_MARKER))
            .and_then(|metadata| metadata.modified());
        let is_old = marker_modified
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= MIN_EXTRACTION_RETENTION);
        if !is_old {
            retained_entries += 1;
            continue;
        }
        remove_validated_direct_child(&entry.path(), &canonical_container)
            .unwrap_or_else(|err| panic!("Refusing unsafe extraction cleanup: {err}"));
    }
    if retained_entries >= MAX_EXTRACTION_GENERATIONS {
        panic!("Sherpa extraction container has no bounded room for a new generation");
    }

    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..64u32 {
        let path = linker_container.join(format!(
            "{EXTRACTION_GENERATION_PREFIX}{}-{epoch}-{attempt}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                let marker = path.join(EXTRACTION_MARKER);
                let marker_content = format!("VOCALCODE_SHERPA_EXTRACTION_V1\n{checksum}\n");
                let mut marker_file = create_new_plain_file(&marker)
                    .unwrap_or_else(|err| panic!("Failed to create extraction marker: {err}"));
                marker_file
                    .write_all(marker_content.as_bytes())
                    .unwrap_or_else(|err| panic!("Failed to write extraction marker: {err}"));
                marker_file
                    .sync_all()
                    .unwrap_or_else(|err| panic!("Failed to sync extraction marker: {err}"));
                return path;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => panic!("Failed to create isolated sherpa extraction directory: {err}"),
        }
    }
    panic!("Failed to create a unique isolated sherpa extraction directory")
}

pub fn extract_tbz(buf: &[u8], output: &Path) {
    debug_log!("extracting tbz to {}", output.display());
    let buf: std::io::BufReader<&[u8]> = std::io::BufReader::new(buf);
    let tar = bzip2::read::BzDecoder::new(buf); // Use BzDecoder for .bz2
    let mut archive = tar::Archive::new(tar);
    let mut budget = ArchiveBudget::new();
    let mut pending_links = Vec::new();
    let entries = archive.entries().expect("Failed to read .tbz archive");
    for entry in entries {
        let mut entry = entry.expect("Failed to read .tbz entry");
        let entry_type = entry.header().entry_type();
        let kind = if entry_type.is_file() {
            SafeEntryKind::File
        } else if entry_type.is_dir() {
            SafeEntryKind::Directory
        } else if entry_type.is_symlink() {
            SafeEntryKind::Symlink
        } else {
            panic!("Refusing hard link or special entry in sherpa archive");
        };
        let size = entry
            .header()
            .size()
            .expect("Failed to read .tbz entry size");
        let path = entry.path().expect("Failed to read .tbz entry path");
        let safe_path = budget
            .admit(&path, kind, size)
            .unwrap_or_else(|err| panic!("Refusing unsafe sherpa archive: {err}"));
        if aliases_reserved_root(&safe_path, EXTRACTION_MARKER) {
            panic!("Refusing reserved extraction marker path in sherpa archive");
        }
        let destination = output.join(&safe_path);
        if kind == SafeEntryKind::Directory {
            match fs::create_dir(&destination) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(&destination).unwrap_or_else(|inspect| {
                        panic!("Failed to inspect existing archive directory: {inspect}")
                    });
                    if !is_plain_directory(&metadata) {
                        panic!("Refusing non-directory archive path collision");
                    }
                }
                Err(err) => panic!("Failed to create archive directory: {err}"),
            }
        } else if kind == SafeEntryKind::File {
            let parent = destination
                .parent()
                .expect("Validated archive file must have a parent");
            fs::create_dir_all(parent)
                .unwrap_or_else(|err| panic!("Failed to create archive parent: {err}"));
            let mut output = create_new_plain_file(&destination)
                .unwrap_or_else(|err| panic!("Failed to create archive file: {err}"));
            let copied = std::io::copy(&mut (&mut entry).take(size + 1), &mut output)
                .unwrap_or_else(|err| panic!("Failed to extract archive file: {err}"));
            if copied != size {
                panic!("Archive entry size changed while extracting");
            }
            output
                .flush()
                .unwrap_or_else(|err| panic!("Failed to flush archive file: {err}"));
        } else {
            let target = entry
                .link_name()
                .unwrap_or_else(|err| panic!("Failed to read archive symlink target: {err}"))
                .unwrap_or_else(|| panic!("Archive symlink has no target"));
            let resolved_target = resolve_relative_link_target(&safe_path, &target)
                .unwrap_or_else(|err| panic!("Refusing unsafe archive symlink: {err}"));
            if aliases_reserved_root(&resolved_target, EXTRACTION_MARKER) {
                panic!("Refusing archive symlink to the private generation marker");
            }
            let parent = destination
                .parent()
                .expect("Validated archive symlink must have a parent");
            fs::create_dir_all(parent)
                .unwrap_or_else(|err| panic!("Failed to create archive symlink parent: {err}"));
            pending_links.push((safe_path, resolved_target));
        }
    }

    // The signed upstream macOS archive uses an unversioned dylib symlink.
    // Keep the extracted tree link-free by copying the declared regular-file
    // target after every archive entry has been validated. This preserves the
    // linker-visible filename without allowing links, link chains, or targets
    // outside the verified archive to survive into later build steps.
    for (link, target) in pending_links {
        if budget.kind_of(&target) != Some(SafeEntryKind::File) {
            panic!("Archive symlink target is not a declared regular file");
        }
        let source_path = output.join(target);
        let mut source = open_plain_file_readonly(&source_path)
            .unwrap_or_else(|err| panic!("Failed to open archive symlink target: {err}"));
        let size = source
            .metadata()
            .unwrap_or_else(|err| panic!("Failed to inspect archive symlink target: {err}"))
            .len();
        budget
            .admit_materialized_bytes(size)
            .unwrap_or_else(|err| panic!("Refusing unsafe archive symlink: {err}"));
        let destination = output.join(link);
        let mut materialized = create_new_plain_file(&destination)
            .unwrap_or_else(|err| panic!("Failed to materialize archive symlink: {err}"));
        let copied = std::io::copy(&mut (&mut source).take(size + 1), &mut materialized)
            .unwrap_or_else(|err| panic!("Failed to copy archive symlink target: {err}"));
        if copied != size {
            panic!("Archive symlink target changed while materializing");
        }
        materialized
            .flush()
            .unwrap_or_else(|err| panic!("Failed to flush materialized archive symlink: {err}"));
    }
}

pub fn extract_lib_name<P: AsRef<Path>>(path: P) -> String {
    path.as_ref()
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.strip_prefix("lib")
                .unwrap_or(name)
                .replace(".so", "")
                .replace(".dylib", "")
                .replace(".a", "")
        })
        .unwrap_or_else(|| "".to_string())
}
