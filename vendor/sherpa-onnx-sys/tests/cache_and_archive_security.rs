#![allow(dead_code)]

#[path = "../src/source_tree.rs"]
mod source_tree;

#[path = "../src/download_binaries.rs"]
mod download_binaries;

use bzip2::write::BzEncoder;
use bzip2::Compression;
use download_binaries::{
    cache_verified_archive_if_absent, extract_tbz, isolated_extraction_dir,
    read_verified_cached_archive, sha256, EXTRACTION_CONTAINER, MAX_SHERPA_ARCHIVE_BYTES,
    MIN_EXTRACTION_RETENTION,
};
use std::fs::{self, File};
use std::io::Cursor;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tar::{Builder, EntryType, Header};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "vocalcode-sherpa-{label}-{}-{nonce}",
            std::process::id()
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

fn archive_with(entries: &[(&str, EntryType, &[u8])]) -> Vec<u8> {
    let encoder = BzEncoder::new(Vec::new(), Compression::best());
    let mut builder = Builder::new(encoder);
    for (path, entry_type, body) in entries {
        let mut header = Header::new_gnu();
        header.set_entry_type(*entry_type);
        header.set_mode(0o644);
        header.set_size(body.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, path, Cursor::new(*body))
            .unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

fn archive_with_symlink(link: &str, target: &str) -> Vec<u8> {
    let encoder = BzEncoder::new(Vec::new(), Compression::best());
    let mut builder = Builder::new(encoder);

    let mut file = Header::new_gnu();
    file.set_entry_type(EntryType::Regular);
    file.set_mode(0o644);
    file.set_size(b"versioned dylib".len() as u64);
    file.set_cksum();
    builder
        .append_data(
            &mut file,
            "sherpa/lib/libonnxruntime.1.17.1.dylib",
            Cursor::new(b"versioned dylib"),
        )
        .unwrap();

    let mut symlink = Header::new_gnu();
    symlink.set_entry_type(EntryType::Symlink);
    symlink.set_mode(0o777);
    symlink.set_size(0);
    symlink.set_link_name(target).unwrap();
    symlink.set_cksum();
    builder
        .append_data(&mut symlink, link, Cursor::new([]))
        .unwrap();

    builder.into_inner().unwrap().finish().unwrap()
}

#[test]
fn cache_requires_the_pinned_hash_on_every_read() {
    let temp = TestDir::new("cache-hash");
    let path = temp.0.join("candidate.tbz");
    fs::write(&path, b"authentic archive bytes").unwrap();
    let expected = sha256(b"authentic archive bytes");
    assert_eq!(
        read_verified_cached_archive(&path, &expected).unwrap(),
        b"authentic archive bytes"
    );

    fs::write(&path, b"tampered archive bytes!").unwrap();
    assert!(read_verified_cached_archive(&path, &expected).is_none());
}

#[test]
fn poisoned_cache_is_never_deleted_or_overwritten() {
    let temp = TestDir::new("cache-no-clobber");
    let path = temp.0.join("candidate.tbz");
    fs::write(&path, b"attacker-owned candidate").unwrap();
    cache_verified_archive_if_absent(&path, b"verified replacement");
    assert_eq!(fs::read(path).unwrap(), b"attacker-owned candidate");
}

#[test]
fn build_contract_never_reuses_persistent_extracted_libraries() {
    let build = include_str!("../build.rs");
    let downloader = include_str!("../src/download_binaries.rs");
    assert!(build.contains("read_verified_cached_archive"));
    assert!(build.contains("verify_checksum(&sha256(&archive), &dist.checksum)"));
    assert!(build.contains("isolated_extraction_dir(&out_dir, &dist.checksum)"));
    assert!(!build.contains("!lib_dir.exists()"));
    assert!(!build.contains("Using cache from"));
    assert!(build.contains("source_tree::copy_source_tree"));
    assert!(downloader.contains("let linker_container = out_dir.join(EXTRACTION_CONTAINER)"));
    assert!(downloader.contains("let canonical_out_dir = out_dir"));
    assert!(downloader.contains("let path = linker_container.join"));
    assert!(!build.contains("Command::new(\"robocopy.exe\")"));
    assert!(!build.contains("Command::new(\"cp\")"));
    assert!(!build.contains("delete_folder(&sherpa_src"));
    // The reviewed v1.13.6 macOS archive contains a plain unversioned
    // libonnxruntime.dylib whose Mach-O ID and sherpa dependency are both
    // @rpath/libonnxruntime.dylib. Reintroducing the legacy 1.17.1 alias would
    // make every clean macOS build fail after the verified assets are copied.
    assert!(!build.contains("libonnxruntime.1.17.1.dylib"));
    assert!(!build.contains("publish_relative_symlink_replacing"));
    let extract = build
        .find("extract_tbz(&archive, &extraction_dir)")
        .expect("verified archive must be extracted");
    let publish = build
        .find("env::set_var(\"SHERPA_LIB_PATH\"")
        .expect("verified extraction must be selected for linking");
    assert!(
        extract < publish,
        "link inputs cannot publish before extraction succeeds"
    );
    let validate = build
        .find("source_tree::validate_plain_tree(&extraction_dir)")
        .expect("extracted tree must be validated without following links");
    assert!(extract < validate && validate < publish);
}

#[test]
fn recent_parallel_extraction_generations_are_retained_and_old_owned_ones_are_cleaned() {
    let temp = TestDir::new("generation-cleanup");
    let checksum = "a".repeat(64);
    let first = isolated_extraction_dir(&temp.0, &checksum);
    fs::write(first.join("partial-file"), b"partial").unwrap();
    let second = isolated_extraction_dir(&temp.0, &checksum);
    assert!(first.exists());
    assert!(second.exists());

    let old_marker = File::options()
        .write(true)
        .open(first.join(".vocalcode-sherpa-extraction-v1"))
        .unwrap();
    old_marker
        .set_times(
            fs::FileTimes::new().set_modified(
                SystemTime::now() - MIN_EXTRACTION_RETENTION - Duration::from_secs(1),
            ),
        )
        .unwrap();

    let container = second.parent().unwrap();
    let foreign = container.join("g-foreign-without-marker");
    fs::create_dir(&foreign).unwrap();
    let third = isolated_extraction_dir(&temp.0, &checksum);
    assert!(!first.exists());
    assert!(second.exists());
    assert!(foreign.exists());
    assert!(third.exists());
}

#[test]
fn foreign_extraction_entries_have_a_hard_container_limit() {
    let temp = TestDir::new("generation-limit");
    let container = temp.0.join(EXTRACTION_CONTAINER);
    fs::create_dir(&container).unwrap();
    for index in 0..128 {
        fs::create_dir(container.join(format!("foreign-{index}"))).unwrap();
    }
    let checksum = "b".repeat(64);
    assert!(std::panic::catch_unwind(|| isolated_extraction_dir(&temp.0, &checksum)).is_err());
    assert_eq!(fs::read_dir(container).unwrap().count(), 128);
}

#[test]
fn sparse_oversized_cache_is_rejected_without_reading_it() {
    let temp = TestDir::new("cache-size");
    let path = temp.0.join("candidate.tbz");
    let file = File::create(&path).unwrap();
    file.set_len(MAX_SHERPA_ARCHIVE_BYTES + 1).unwrap();
    assert!(read_verified_cached_archive(&path, &sha256(b"irrelevant")).is_none());
}

#[test]
fn safe_regular_archive_extracts() {
    let temp = TestDir::new("safe-extract");
    let archive = archive_with(&[(
        "sherpa/lib/example.lib",
        EntryType::Regular,
        b"library bytes",
    )]);
    extract_tbz(&archive, &temp.0);
    assert_eq!(
        fs::read(temp.0.join("sherpa/lib/example.lib")).unwrap(),
        b"library bytes"
    );
}

#[test]
fn in_tree_dylib_symlink_is_materialized_as_a_plain_file() {
    let temp = TestDir::new("link-materialize");
    let archive = archive_with_symlink(
        "sherpa/lib/libonnxruntime.dylib",
        "libonnxruntime.1.17.1.dylib",
    );
    extract_tbz(&archive, &temp.0);
    let materialized = temp.0.join("sherpa/lib/libonnxruntime.dylib");
    assert_eq!(fs::read(&materialized).unwrap(), b"versioned dylib");
    assert!(fs::symlink_metadata(materialized)
        .unwrap()
        .file_type()
        .is_file());
}

#[test]
fn archive_symlink_escape_is_rejected() {
    let temp = TestDir::new("link-escape-reject");
    let archive = archive_with_symlink("sherpa/lib/link", "../../../outside");
    assert!(std::panic::catch_unwind(|| extract_tbz(&archive, &temp.0)).is_err());
}

#[test]
fn archive_symlink_must_target_a_declared_regular_file() {
    let temp = TestDir::new("link-undeclared-reject");
    let archive = archive_with_symlink("sherpa/lib/link", "missing.dylib");
    assert!(std::panic::catch_unwind(|| extract_tbz(&archive, &temp.0)).is_err());
}

#[test]
fn archive_hard_links_are_rejected() {
    let temp = TestDir::new("hard-link-reject");
    let archive = archive_with(&[("sherpa/link", EntryType::Link, b"")]);
    assert!(std::panic::catch_unwind(|| extract_tbz(&archive, &temp.0)).is_err());
}

#[test]
fn duplicate_archive_paths_are_rejected() {
    let temp = TestDir::new("duplicate-reject");
    let archive = archive_with(&[
        ("sherpa/lib/file", EntryType::Regular, b"first"),
        ("sherpa/lib/file", EntryType::Regular, b"second"),
    ]);
    assert!(std::panic::catch_unwind(|| extract_tbz(&archive, &temp.0)).is_err());
}

#[test]
fn archive_cannot_overwrite_the_private_generation_marker() {
    let temp = TestDir::new("marker-reject");
    let archive = archive_with(&[(
        ".vocalcode-sherpa-extraction-v1",
        EntryType::Regular,
        b"forged marker",
    )]);
    assert!(std::panic::catch_unwind(|| extract_tbz(&archive, &temp.0)).is_err());
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn archive_cannot_alias_the_private_generation_marker_by_case() {
    let temp = TestDir::new("marker-case-alias-reject");
    let archive = archive_with(&[(
        ".VOCALCODE-SHERPA-EXTRACTION-V1",
        EntryType::Regular,
        b"forged marker",
    )]);
    assert!(std::panic::catch_unwind(|| extract_tbz(&archive, &temp.0)).is_err());
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn case_aliased_archive_paths_are_rejected_before_writing() {
    let temp = TestDir::new("path-case-alias-reject");
    let archive = archive_with(&[
        ("sherpa/Lib/file.dll", EntryType::Regular, b"first"),
        ("sherpa/lib/FILE.dll", EntryType::Regular, b"second"),
    ]);
    assert!(std::panic::catch_unwind(|| extract_tbz(&archive, &temp.0)).is_err());
}

#[test]
fn archive_file_never_replaces_an_existing_destination() {
    let temp = TestDir::new("extract-create-new");
    let destination = temp.0.join("sherpa/lib/file");
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::write(&destination, b"must survive").unwrap();
    let archive = archive_with(&[("sherpa/lib/file", EntryType::Regular, b"replacement")]);

    assert!(std::panic::catch_unwind(|| extract_tbz(&archive, &temp.0)).is_err());
    assert_eq!(fs::read(destination).unwrap(), b"must survive");
}

#[cfg(unix)]
#[test]
fn symlink_cache_candidate_is_rejected() {
    use std::os::unix::fs::symlink;
    let temp = TestDir::new("cache-symlink");
    let target = temp.0.join("target");
    let candidate = temp.0.join("candidate");
    fs::write(&target, b"authentic archive bytes").unwrap();
    symlink(&target, &candidate).unwrap();
    assert!(
        read_verified_cached_archive(&candidate, &sha256(b"authentic archive bytes")).is_none()
    );
}
