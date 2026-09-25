//! A single-flight, revision-bound migration transaction. Preview is read-only;
//! commit uses the exact disk revision and Undo refuses intervening edits.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use vocalcode_core::migration::{self as format, Entry, Kind, Preview, MAX_IMPORT_BYTES};

#[derive(Default)]
pub(crate) struct Session {
    serial: u64,
    pending: Option<Pending>,
    undo: Option<Undo>,
}

struct Pending {
    token: u64,
    kind: Kind,
    revision: String,
    before: Vec<Entry>,
    preview: Preview,
}

struct Undo {
    token: u64,
    kind: Kind,
    revision: String,
    before: Vec<Entry>,
}

fn snippets_path(base: &Path) -> Result<PathBuf, String> {
    crate::paths::ensure_trusted_data_subdir(base, Path::new("personalization"))
        .map(|dir| dir.join("snippets.json"))
        .map_err(|error| error.to_string())
}

pub(crate) fn export_destination(base: &Path, chosen: &Path) -> Result<PathBuf, String> {
    if !chosen.is_absolute() {
        return Err("Choose an absolute export location.".into());
    }
    let name = chosen.file_name().ok_or("Choose an export filename")?;
    let parent = chosen
        .parent()
        .ok_or("Choose an export folder")?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let base = base.canonicalize().map_err(|e| e.to_string())?;
    let resolved = parent.join(name);
    // Resolve ../ and directory aliases before comparing. Windows filesystem
    // spelling is case-insensitive even though Path::starts_with is not.
    #[cfg(windows)]
    let inside = PathBuf::from(resolved.to_string_lossy().to_lowercase())
        .starts_with(PathBuf::from(base.to_string_lossy().to_lowercase()));
    #[cfg(not(windows))]
    let inside = resolved.starts_with(&base);
    if inside {
        return Err("Save exports outside VocalCode's application data folder.".into());
    }
    Ok(resolved)
}

fn read_snippets(path: &Path) -> Result<(String, Vec<Entry>), String> {
    let bytes = match crate::read_bounded_bytes(path, MAX_IMPORT_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(("absent".into(), Vec::new()))
        }
        Err(error) => return Err(error.to_string()),
    };
    let revision = format!("{:x}", Sha256::digest(&bytes));
    Ok((revision, snippet_document(&bytes)?))
}

/// Validate a stored `snippets.json` envelope and every entry in it. Also
/// used for the previous VocalCode's copy, which has the same format.
pub(crate) fn snippet_document(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "Snippet file is not UTF-8. It was left untouched.")?;
    // An exported empty collection is valid even though an import must add at
    // least one row. Validate the envelope and every stored entry on load.
    let doc: format::Document =
        serde_json::from_str(text).map_err(|_| "Invalid snippet file. It was left untouched.")?;
    if doc.format != "vocalcode-interchange"
        || doc.version != 1
        || doc.kind != Kind::Snippets
        || doc.entries.len() > format::MAX_IMPORT_ROWS
    {
        return Err("Unsupported snippet document. It was left untouched.".into());
    }
    for entry in &doc.entries {
        format::validate(Kind::Snippets, entry)?;
    }
    Ok(doc.entries)
}

pub(crate) fn snapshot(base: &Path, kind: Kind) -> Result<(String, Vec<Entry>), String> {
    match kind {
        Kind::Dictionary => {
            let doc = crate::load_rules_document(base)?;
            Ok((
                doc.revision.as_str().into(),
                doc.rules
                    .into_iter()
                    .map(|(name, text)| Entry { name, text })
                    .collect(),
            ))
        }
        Kind::Snippets => read_snippets(&snippets_path(base)?),
    }
}

pub(crate) fn save(
    base: &Path,
    kind: Kind,
    expected: &str,
    entries: &[Entry],
) -> Result<String, String> {
    for entry in entries {
        format::validate(kind, entry)?;
    }
    match kind {
        Kind::Dictionary => {
            let revision = crate::RulesRevision::parse(expected)?;
            let lines = entries
                .iter()
                .map(|entry| format!("{} => {}", entry.name, entry.text))
                .collect::<Vec<_>>();
            crate::save_rules_if_current(base, &revision, &lines)
                .map(|doc| doc.revision.as_str().to_string())
                .map_err(|error| error.to_string())
        }
        Kind::Snippets => {
            let path = snippets_path(base)?;
            let _guard = crate::lock_rules_writes(&path)?;
            let (current, _) = read_snippets(&path)?;
            if current != expected {
                return Err("Snippets changed since the preview. Reload and preview again; nothing was overwritten.".into());
            }
            let text = format::export(kind, entries.to_vec())?;
            if text.len() > MAX_IMPORT_BYTES {
                return Err("The merged snippet file exceeds 3 MiB. Nothing was saved.".into());
            }
            crate::storage::atomic_write(&path, text.as_bytes())
                .map_err(|error| error.to_string())?;
            Ok(format!("{:x}", Sha256::digest(text.as_bytes())))
        }
    }
}

pub(crate) fn publish_runtime(
    base: &Path,
    status: &crate::webui::RuntimeStatus,
) -> Result<Value, String> {
    let mut state = json!({});
    let mut warnings = Vec::new();
    // Independent collections: a damaged snippet file must not turn a committed
    // dictionary import into a reported failure, or clear the last good state.
    match snapshot(base, Kind::Dictionary) {
        Ok((revision, entries)) => {
            let rules = entries
                .iter()
                .map(|e| (e.name.clone(), e.text.clone()))
                .collect::<Vec<_>>();
            *status.rules.lock().unwrap_or_else(|p| p.into_inner()) = crate::merge_rules(&rules);
            state["dictionary"] = json!(entries);
            state["dictionary_revision"] = json!(revision);
        }
        Err(error) => warnings.push(error),
    }
    match snapshot(base, Kind::Snippets) {
        Ok((revision, entries)) => {
            *status.snippets.lock().unwrap_or_else(|p| p.into_inner()) = entries.clone();
            state["snippets"] = json!(entries);
            state["snippets_revision"] = json!(revision);
        }
        Err(error) => warnings.push(error),
    }
    state["warnings"] = json!(warnings);
    Ok(state)
}

pub(crate) fn handle(
    base: &Path,
    status: &crate::webui::RuntimeStatus,
    request: &Value,
    input_file: Option<PathBuf>,
    output_file: Option<PathBuf>,
) -> Result<Value, String> {
    let mut session = status
        .migration
        .lock()
        .map_err(|_| "Migration state is unavailable")?;
    let op = request["op"]
        .as_str()
        .ok_or("Missing migration operation")?;
    let kind: Kind = serde_json::from_value(request["kind"].clone())
        .map_err(|_| "Choose Dictionary or Snippets")?;
    match op {
        "load" => Ok(
            json!({"state":publish_runtime(base,status)?,"undo_token":session.undo.as_ref().map(|u|u.token)}),
        ),
        "preview" | "pick" => {
            session.pending = None;
            let text = if let Some(path) = input_file {
                let bytes = crate::read_bounded_bytes(&path, MAX_IMPORT_BYTES)
                    .map_err(|error| error.to_string())?;
                String::from_utf8(bytes)
                    .map_err(|_| "The import must be UTF-8 CSV or JSON. No data was changed.")?
            } else {
                request["text"]
                    .as_str()
                    .ok_or("Paste CSV or JSON first")?
                    .to_string()
            };
            let layout = if let Some(value) = request.get("layout") {
                serde_json::from_value(value.clone())
                    .map_err(|_| "Choose a supported input layout")?
            } else {
                format::Layout::Auto
            };
            let incoming = format::parse_with_layout(
                &text,
                kind,
                request["header"].as_bool().unwrap_or(false),
                layout,
            )?;
            let (revision, before) = snapshot(base, kind)?;
            let preview = format::preview(kind, &before, &incoming)?;
            // Validate normalized serialized bounds now, not just after Confirm.
            if kind == Kind::Dictionary {
                let lines = preview
                    .merged
                    .iter()
                    .map(|e| format!("{} => {}", e.name, e.text))
                    .collect::<Vec<_>>();
                crate::normalize_rule_lines(&lines)?;
            } else if format::export(kind, preview.merged.clone())?.len() > MAX_IMPORT_BYTES {
                return Err("The merged snippet document would exceed 3 MiB.".into());
            }
            session.serial += 1;
            let token = session.serial;
            let result = json!({"preview":preview,"token":token,"kind":kind});
            session.pending = Some(Pending {
                token,
                kind,
                revision,
                before,
                preview,
            });
            Ok(result)
        }
        "commit" => {
            let pending = session.pending.as_ref().ok_or("Preview the import first")?;
            if request["token"].as_u64() != Some(pending.token) || kind != pending.kind {
                return Err("This preview is no longer current. Preview again.".into());
            }
            if pending.preview.added == 0 {
                return Err("There are no new entries to import.".into());
            }
            let revision = save(base, kind, &pending.revision, &pending.preview.merged)?;
            let pending = session.pending.take().expect("validated preview");
            session.undo = Some(Undo {
                token: pending.token,
                kind,
                revision,
                before: pending.before,
            });
            Ok(
                json!({"saved":true,"undo_token":pending.token,"state":publish_runtime(base,status)?}),
            )
        }
        "undo" => {
            let undo = session
                .undo
                .as_ref()
                .ok_or("There is no import to undo in this app session")?;
            if request["token"].as_u64() != Some(undo.token) {
                return Err("This Undo action is no longer current.".into());
            }
            // Full-document CAS deliberately refuses *any* intervening edit.
            // It never reverts subsequent manual or automatic learning changes.
            save(base, undo.kind, &undo.revision, &undo.before)?;
            session.undo = None;
            session.pending = None;
            Ok(json!({"undone":true,"state":publish_runtime(base,status)?}))
        }
        "delete_snippet" => {
            let (revision, mut entries) = snapshot(base, Kind::Snippets)?;
            if request["revision"].as_str() != Some(revision.as_str()) {
                return Err("Snippets changed. Reload before deleting.".into());
            }
            let name = request["name"].as_str().ok_or("Choose a snippet")?;
            let index = entries
                .iter()
                .position(|entry| entry.name == name)
                .ok_or("Snippet no longer exists")?;
            entries.remove(index);
            save(base, Kind::Snippets, &revision, &entries)?;
            Ok(json!({"deleted":true,"state":publish_runtime(base,status)?}))
        }
        "export" => {
            let (_, entries) = snapshot(base, kind)?;
            let destination = output_file.ok_or("Choose where to save the export")?;
            // The destination is supplied by the native picker, never by IPC.
            let destination = export_destination(base, &destination)?;
            crate::storage::atomic_write(&destination, format::export(kind, entries)?.as_bytes())
                .map_err(|e| e.to_string())?;
            Ok(json!({"exported":true}))
        }
        _ => Err("Unknown migration operation".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    fn test_dir() -> TempDir {
        TempDir::new("migration")
    }

    // Explicit local replay only: never embed personal vocabulary in fixtures or
    // logs, and never point the destination at the installed application store.
    #[test]
    #[ignore = "requires explicitly selected local competitor exports"]
    fn real_local_competitor_export_round_trip() {
        let files = std::env::var_os("VOCALCODE_QA_MIGRATION_FILES")
            .expect("Set VOCALCODE_QA_MIGRATION_FILES to explicit export paths");
        let paths = std::env::split_paths(&files).collect::<Vec<_>>();
        assert!(!paths.is_empty());
        for path in paths {
            let source = crate::read_bounded_bytes(&path, MAX_IMPORT_BYTES).unwrap();
            let doc: format::Document = serde_json::from_slice(&source).unwrap();
            let kind = doc.kind;
            let incoming = format::parse(std::str::from_utf8(&source).unwrap(), kind, false)
                .expect("Export must parse as supported interchange data");
            let dir = test_dir();
            let status = crate::webui::RuntimeStatus::default();
            let (revision, _) = snapshot(&dir, kind).unwrap();
            // Exercise preservation of a pre-existing entry as well as imports.
            let sentinel = Entry {
                name: "vocalcode migration qa sentinel".into(),
                text: "VocalCode migration QA sentinel".into(),
            };
            assert!(!incoming.iter().any(|e| e.name == sentinel.name));
            save(&dir, kind, &revision, &[sentinel]).unwrap();
            let before = snapshot(&dir, kind).unwrap();
            let expected = format::preview(kind, &before.1, &incoming).unwrap();
            assert!(expected.added > 0);
            let preview = handle(
                &dir,
                &status,
                &json!({"op":"pick","kind":kind}),
                Some(path.clone()),
                None,
            )
            .unwrap();
            assert!(
                snapshot(&dir, kind).unwrap() == before,
                "Preview mutated data"
            );
            assert_eq!(preview["preview"]["added"], expected.added);
            let committed = handle(
                &dir,
                &status,
                &json!({"op":"commit","kind":kind,"token":preview["token"]}),
                None,
                None,
            )
            .unwrap();
            assert_eq!(committed["saved"], true);
            let saved = snapshot(&dir, kind).unwrap().1;
            // Dictionary persistence may sort rows; compare exact field pairs,
            // never print personal contents even if this assertion fails.
            assert_eq!(saved.len(), expected.merged.len());
            assert!(expected.merged.iter().all(|entry| saved.contains(entry)));
            let duplicate = handle(
                &dir,
                &status,
                &json!({"op":"pick","kind":kind}),
                Some(path.clone()),
                None,
            )
            .unwrap();
            assert_eq!(duplicate["preview"]["added"], 0);
            assert!(handle(
                &dir,
                &status,
                &json!({"op":"commit","kind":kind,"token":duplicate["token"]}),
                None,
                None,
            )
            .is_err());
            let undone = handle(
                &dir,
                &status,
                &json!({"op":"undo","kind":kind,"token":committed["undo_token"]}),
                None,
                None,
            )
            .unwrap();
            assert_eq!(undone["undone"], true);
            assert!(snapshot(&dir, kind).unwrap().1 == before.1);
            assert!(
                std::fs::read(&path).unwrap() == source,
                "Source was modified"
            );
            eprintln!(
                "local migration replay: kind={kind:?}, source_rows={}, added={}, preview/commit/duplicate/undo passed",
                incoming.len(), expected.added
            );
        }
    }
    #[test]
    fn preview_commit_undo_and_conflicts_use_disk_revision() {
        let dir = test_dir();
        let status = crate::webui::RuntimeStatus::default();
        let request = |op: &str, token: Value| json!({"op":op,"kind":"dictionary","text":"myword,MyWord","token":token});
        let before = snapshot(&dir, Kind::Dictionary).unwrap();
        let p = handle(&dir, &status, &request("preview", Value::Null), None, None).unwrap();
        assert_eq!(snapshot(&dir, Kind::Dictionary).unwrap(), before);
        handle(
            &dir,
            &status,
            &request("commit", p["token"].clone()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            status
                .rules
                .lock()
                .unwrap()
                .iter()
                .filter(|(a, _)| a == "myword")
                .count(),
            1
        );
        handle(
            &dir,
            &status,
            &request("undo", p["token"].clone()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(snapshot(&dir, Kind::Dictionary).unwrap().1, before.1);
        let p = handle(&dir, &status, &request("preview", Value::Null), None, None).unwrap();
        handle(
            &dir,
            &status,
            &request("commit", p["token"].clone()),
            None,
            None,
        )
        .unwrap();
        let (rev, mut entries) = snapshot(&dir, Kind::Dictionary).unwrap();
        entries.push(Entry {
            name: "later".into(),
            text: "Later".into(),
        });
        save(&dir, Kind::Dictionary, &rev, &entries).unwrap();
        assert!(handle(
            &dir,
            &status,
            &request("undo", p["token"].clone()),
            None,
            None
        )
        .is_err());
        assert_eq!(snapshot(&dir, Kind::Dictionary).unwrap().1, entries);
    }
    #[test]
    fn a_pasted_replacements_file_previews_as_rules() {
        let dir = test_dir();
        let status = crate::webui::RuntimeStatus::default();
        let text = "# VocalCode — your own recognition fixes.\n\
                    # One rule per line:  heard text => what to write\n\
                    collie      => Collie\n\
                    lang chain  => LangChain\n";
        for layout in [Value::Null, json!("rules")] {
            let mut request = json!({"op":"preview","kind":"dictionary","text":text});
            if !layout.is_null() {
                request["layout"] = layout;
            }
            let preview = handle(&dir, &status, &request, None, None).unwrap();
            assert_eq!(preview["preview"]["added"], 2, "{request}");
            assert_eq!(
                preview["preview"]["rows"][1]["entry"],
                json!({"name":"lang chain","text":"LangChain"})
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn snippets_survive_reload_and_stale_writes_fail() {
        let dir = test_dir();
        let entries = vec![Entry {
            name: "greet".into(),
            text: "Hi!".into(),
        }];
        let revision = save(&dir, Kind::Snippets, "absent", &entries).unwrap();
        assert_eq!(
            snapshot(&dir, Kind::Snippets).unwrap(),
            (revision.clone(), entries)
        );
        assert!(save(&dir, Kind::Snippets, "absent", &[]).is_err());
        save(&dir, Kind::Snippets, &revision, &[]).unwrap();
        assert!(snapshot(&dir, Kind::Snippets).unwrap().1.is_empty());
    }

    #[test]
    fn stale_preview_is_rejected_without_overwriting_new_dictionary() {
        let dir = test_dir();
        let status = crate::webui::RuntimeStatus::default();
        let preview = handle(
            &dir,
            &status,
            &json!({"op":"preview","kind":"dictionary","text":"new,New"}),
            None,
            None,
        )
        .unwrap();
        let (revision, mut entries) = snapshot(&dir, Kind::Dictionary).unwrap();
        entries.push(Entry {
            name: "external".into(),
            text: "External".into(),
        });
        save(&dir, Kind::Dictionary, &revision, &entries).unwrap();
        assert!(handle(
            &dir,
            &status,
            &json!({"op":"commit","kind":"dictionary","token":preview["token"]}),
            None,
            None
        )
        .is_err());
        assert_eq!(snapshot(&dir, Kind::Dictionary).unwrap().1, entries);
    }

    #[test]
    fn corrupt_other_collection_cannot_hide_a_successful_commit() {
        let dir = test_dir();
        let status = crate::webui::RuntimeStatus::default();
        let path = snippets_path(&dir).unwrap();
        crate::storage::atomic_write(&path, b"broken JSON").unwrap();
        let preview = handle(
            &dir,
            &status,
            &json!({"op":"preview","kind":"dictionary","text":"new,New"}),
            None,
            None,
        )
        .unwrap();
        let result = handle(
            &dir,
            &status,
            &json!({"op":"commit","kind":"dictionary","token":preview["token"]}),
            None,
            None,
        )
        .unwrap();
        assert_eq!(result["saved"], true);
        assert_eq!(result["state"]["warnings"].as_array().unwrap().len(), 1);
        assert!(result["state"].get("snippets").is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"broken JSON");
    }

    #[test]
    fn exports_cannot_overwrite_application_data_via_parent_aliases() {
        let dir = test_dir();
        let nested = dir.join("nested");
        std::fs::create_dir(&nested).unwrap();
        assert!(export_destination(&dir, &dir.join("out.json")).is_err());
        assert!(export_destination(&dir, &nested.join("..").join("out.json")).is_err());
        assert!(export_destination(&dir, Path::new("relative.json")).is_err());
        #[cfg(windows)]
        assert!(export_destination(
            &dir,
            &PathBuf::from(dir.to_string_lossy().to_uppercase()).join("out.json")
        )
        .is_err());
        assert!(export_destination(&nested, &dir.join("out.json")).is_ok());
    }
}
