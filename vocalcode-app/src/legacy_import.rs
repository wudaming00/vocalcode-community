//! Copy-only import from the previous (paid) VocalCode.
//!
//! That edition kept its data in `%LOCALAPPDATA%\VocalCode` on Windows and
//! `~/Library/Application Support/VocalCode` on macOS. This edition has its
//! own "VocalCode Community" folder and deliberately never migrates on its
//! own (see `paths::enter_data_lifecycle`), so people arriving from the paid
//! app found an empty dictionary, no meetings and default shortcuts. This is
//! the explicit, user-started bridge between the two folders:
//!
//! * The previous folder is only ever opened for reading, and only the names
//!   in [`ALLOWED`]. Licence, trial and time-anchor files, logs, locks, the
//!   WebView profile, `calendar/`, `diagnostic-history/`, `secure/` and any
//!   name a later release added are never opened or copied.
//! * Nothing overwrites on its own. The dictionary and snippets merge through
//!   the same preview as Settings → Import (existing entries win); meetings
//!   and model files are published only under names this installation lacks;
//!   app profiles only when none were saved here. Settings are the one part
//!   that replaces values: they go back to the page to be applied as one
//!   ordinary, validated Settings change, the page says so beside the box,
//!   and ticks it by default only for an installation with nothing of its own.
//! * Lifetime counts are added once. `legacy-import.json` records that, and
//!   that the Home suggestion was answered.
//! * What could not be read or copied is reported per part, never dropped:
//!   errors and scan problems are `{part, message}` so the page can name the
//!   part in its own language.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vocalcode_core::config::Config;
use vocalcode_core::limits::{
    MAX_CONFIG_DOCUMENT_BYTES, MAX_DICTIONARY_DOCUMENT_BYTES, MAX_TOTALS_DOCUMENT_BYTES,
};
use vocalcode_core::migration::{self as format, Entry, Kind};
use vocalcode_meeting::{Meeting, MeetingId, MeetingStatus, MeetingStore};

use crate::webui::RuntimeStatus;

/// The only top-level names in the previous folder this module ever opens.
pub(crate) const ALLOWED: &[&str] = &[
    "vocalcode.toml",
    "replacements.txt",
    "totals.json",
    "activity.json",
    "personalization",
    "meetings",
    "models",
];
/// Inside `personalization/`: never its write lock, never anything else.
const PERSONALIZATION: &[&str] = &["snippets.json", "workflows.json"];
/// Inside one meeting directory: its record and transcript, plus finished
/// `.wav` chunks under `audio/`. A writer's hidden temporary is not copied.
const MEETING_FILES: &[&str] = &["meeting.json", "transcript.jsonl"];
const MEETING_AUDIO: &str = "audio";
const MARKER: &str = "legacy-import.json";
/// Meetings are assembled here, beside `meetings/` on the same volume, and
/// renamed into place only when complete. The meeting list refuses to show
/// any meeting if one directory lacks its record, so a half-copied meeting
/// must never appear under `meetings/`.
const STAGE_PREFIX: &str = ".vocalcode-previous-import-";
const MAX_MARKER_BYTES: usize = 4 * 1024;
const MAX_WORKFLOW_BYTES: usize = 128 * 1024;
/// The meeting store's own bound for one `meeting.json`.
const MAX_MEETING_RECORD_BYTES: usize = 4 * 1024 * 1024;

fn allowed(previous: &Path, name: &'static str) -> PathBuf {
    debug_assert!(ALLOWED.contains(&name), "{name} is not allow-listed");
    previous.join(name)
}

fn read(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, String> {
    crate::paths::read_regular_file_bounded(path, limit).map_err(|error| error.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Marker {
    /// "Not now" on the Home suggestion.
    dismissed: bool,
    /// Lifetime totals and daily activity were added. Adding them a second
    /// time would double them, so later imports leave them alone.
    stats_imported: bool,
    /// Last completed import, Unix milliseconds.
    imported_at_ms: u64,
}

const MARKER_UNREADABLE: &str = "The import record in this installation is unreadable.";

fn read_marker(base: &Path) -> Result<Marker, String> {
    match read(&base.join(MARKER), MAX_MARKER_BYTES) {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).map_err(|_| MARKER_UNREADABLE.into()),
        Ok(None) => Ok(Marker::default()),
        Err(_) => Err(MARKER_UNREADABLE.into()),
    }
}

fn write_marker(base: &Path, marker: &Marker) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(marker).map_err(|error| error.to_string())?;
    crate::storage::atomic_write(&base.join(MARKER), bytes).map_err(|error| error.to_string())
}

/// What the person ticked. `dictionary` covers dictionary and snippets.
/// `settings` replaces values here, so it is its own choice; `profiles`
/// (application profiles) only fills an installation that has none.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Parts {
    settings: bool,
    profiles: bool,
    dictionary: bool,
    meetings: bool,
    stats: bool,
    models: bool,
}

/// One part that could not be read or imported. The page names the part in
/// its own language and translates `message` when it is one of the fixed
/// sentences in this module; an operating-system error stays as written.
fn problem(part: &str, message: impl Into<String>) -> Value {
    json!({"part": part, "message": message.into()})
}

pub(crate) fn handle(
    base: &Path,
    status: &RuntimeStatus,
    request: &Value,
) -> Result<Value, String> {
    let op = request["op"].as_str().unwrap_or("");
    // Launch at login is independent of whether that app ever saved data.
    if op == "disable_login" {
        let result = crate::webui::disable_previous_edition_autostart()?;
        return Ok(json!({
            "login_result": result,
            "login": crate::webui::previous_edition_autostart(),
        }));
    }
    let previous = crate::paths::previous_edition_data_dir()
        .filter(|path| crate::paths::is_plain_directory(path));
    let Some(previous) = previous else {
        return match op {
            "scan" => Ok(json!({
                "available": false,
                "login": crate::webui::previous_edition_autostart(),
            })),
            _ => Err("No data folder from the previous VocalCode was found.".into()),
        };
    };
    match op {
        "scan" => Ok(scan(&previous, base, status)),
        "import" => {
            let parts: Parts = serde_json::from_value(request["parts"].clone())
                .map_err(|_| "Choose what to import.".to_string())?;
            import(&previous, base, status, parts, crate::totals_writable())
        }
        "dismiss" => dismiss(base),
        _ => Err("Unknown import operation.".into()),
    }
}

/// "Not now" on the Home suggestion. An unreadable record is left as it is:
/// rewriting it from defaults would forget that usage counts were already
/// added, and the next import would add them a second time.
fn dismiss(base: &Path) -> Result<Value, String> {
    let mut marker = read_marker(base)?;
    marker.dismissed = true;
    write_marker(base, &marker)?;
    Ok(json!({"dismissed": true}))
}

const SETTINGS_UNREADABLE: &str = "The previous settings file could not be read.";

/// The previous settings as a page save message. They travel back to the
/// page and are applied as one ordinary Settings change, so they meet exactly
/// the validation, persistence and runtime apply of a click there, and a
/// refusal is reported and rolled back the same way. They replace the values
/// here; the page says so and leaves the box unticked for anyone who already
/// has data of their own.
fn previous_settings(previous: &Path) -> Result<Option<Value>, String> {
    let path = allowed(previous, "vocalcode.toml");
    let Some(bytes) = read(&path, MAX_CONFIG_DOCUMENT_BYTES)? else {
        return Ok(None);
    };
    let source = String::from_utf8(bytes).map_err(|_| SETTINGS_UNREADABLE.to_string())?;
    crate::reject_future_config_version(&path, &source)?;
    // The previous edition's settings are a field subset of these: unknown
    // keys are ignored and missing ones take this build's defaults.
    let unreadable = || SETTINGS_UNREADABLE.to_string();
    let written = toml::from_str::<toml::Table>(&source).map_err(|_| unreadable())?;
    let mut config: Config = toml::from_str(&source).map_err(|_| unreadable())?;
    config.validate_bounds()?;
    config.migrate();
    let mut page = crate::webui::config_snapshot_for_page(&config);
    let Some(fields) = page.as_object_mut() else {
        return Ok(None);
    };
    // Only what that file actually says. A setting it never had, such as
    // Writing rules from a newer release, would otherwise arrive as a
    // default and quietly undo the choice made here. The model follows the
    // language because `migrate` resolves one from the other. Launch at login
    // (registered per edition, owned by the OS) and onboarding (about this
    // window) never travel.
    fields.retain(|key, _| {
        !matches!(key.as_str(), "autostart" | "onboarded")
            && (written.contains_key(key) || (key == "model" && written.contains_key("language")))
    });
    // A model this build no longer ships falls back to the language's
    // recommendation; a language never chosen there keeps this one's.
    let routed = crate::models::route_for(&config.model, &config.language).is_some();
    if !routed && (fields.contains_key("model") || fields.contains_key("language")) {
        if crate::models::route_for("", &config.language).is_some() {
            fields.insert("model".into(), json!(""));
        } else {
            fields.remove("model");
            fields.remove("language");
        }
    }
    Ok((!fields.is_empty()).then_some(page))
}

fn previous_rules(previous: &Path) -> Result<Option<format::RulesFile>, String> {
    let Some(bytes) = read(
        &allowed(previous, "replacements.txt"),
        MAX_DICTIONARY_DOCUMENT_BYTES,
    )?
    else {
        return Ok(None);
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| "The previous dictionary could not be read.".to_string())?;
    format::parse_rules_file(&text).map(Some)
}

fn previous_personalization(
    previous: &Path,
    name: &'static str,
    limit: usize,
) -> Result<Option<Vec<u8>>, String> {
    debug_assert!(
        PERSONALIZATION.contains(&name),
        "{name} is not allow-listed"
    );
    let folder = allowed(previous, "personalization");
    if !crate::paths::is_plain_directory(&folder) {
        return Ok(None);
    }
    read(&folder.join(name), limit)
}

fn previous_snippets(previous: &Path) -> Result<Option<Vec<Entry>>, String> {
    previous_personalization(previous, "snippets.json", format::MAX_IMPORT_BYTES)?
        .map(|bytes| crate::migration::snippet_document(&bytes))
        .transpose()
}

/// Application profiles: `Some("new")` when they would be adopted,
/// `Some("kept")` when this installation already saved its own.
fn previous_profiles(previous: &Path, base: &Path) -> Result<Option<&'static str>, String> {
    let Some(bytes) = previous_personalization(previous, "workflows.json", MAX_WORKFLOW_BYTES)?
    else {
        return Ok(None);
    };
    Ok(Some(
        if crate::workflows::previous_importable(base, &bytes)? {
            "new"
        } else {
            "kept"
        },
    ))
}

/// Merge through the same preview as Settings → Import: existing entries win,
/// duplicates and conflicts are counted, never overwritten. The save is bound
/// to the revision it previewed; if this window saved in between, merge again
/// onto the newer document.
fn merge(base: &Path, kind: Kind, incoming: &[Entry]) -> Result<Value, String> {
    let mut last_error = String::new();
    for _ in 0..3 {
        let (revision, existing) = crate::migration::snapshot(base, kind)?;
        let preview = format::preview(kind, &existing, incoming)?;
        let counts = json!({
            "added": preview.added,
            "duplicates": preview.duplicates,
            "conflicts": preview.conflicts,
        });
        if preview.added == 0 {
            return Ok(counts);
        }
        match crate::migration::save(base, kind, &revision, &preview.merged) {
            Ok(_) => return Ok(counts),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

#[derive(Debug, Default)]
struct MeetingPlan {
    importable: Vec<MeetingId>,
    /// Already in this installation under the same identifier.
    present: usize,
    /// Still recording or processing in the previous app.
    in_progress: usize,
    /// This build cannot read them; copying them would hide every meeting.
    unreadable: usize,
}

/// Read one meeting's record without its transcript: what a scan needs, and
/// cheap enough to run each time Settings opens.
fn meeting_record(directory: &Path, id: &MeetingId) -> Result<Meeting, String> {
    let bytes = read(&directory.join("meeting.json"), MAX_MEETING_RECORD_BYTES)?
        .ok_or_else(|| "missing meeting record".to_string())?;
    let meeting: Meeting = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if &meeting.id != id {
        return Err("meeting record belongs to another directory".into());
    }
    meeting.validate().map_err(|error| error.to_string())?;
    Ok(meeting)
}

/// `thorough` loads and validates every transcript line, as the import does
/// before copying; a scan reads only each meeting's record.
fn plan_meetings(
    previous: &Path,
    base: &Path,
    thorough: bool,
) -> Result<Option<MeetingPlan>, String> {
    let root = allowed(previous, "meetings");
    if !crate::paths::is_plain_directory(&root) {
        return Ok(None);
    }
    // Opening an existing real directory creates and changes nothing.
    let store = MeetingStore::open(&root).map_err(|error| error.to_string())?;
    let local = base.join("meetings");
    let mut entries = std::fs::read_dir(&root)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    let mut plan = MeetingPlan::default();
    for entry in entries {
        let Some(Ok(id)) = entry
            .file_name()
            .to_str()
            .map(|name| MeetingId::parse(name.to_string()))
        else {
            continue;
        };
        if !crate::paths::is_plain_directory(&entry.path()) {
            continue;
        }
        if std::fs::symlink_metadata(local.join(id.as_str())).is_ok() {
            plan.present += 1;
            continue;
        }
        let meeting = if thorough {
            // `load` opens the record and transcript by path, which would
            // follow a link in their place. Refuse a folder with any link in
            // it before reading anything there; the copy would refuse it too.
            crate::paths::validate_tree_without_reparse(&entry.path())
                .map_err(|error| error.to_string())
                .and_then(|()| store.load(&id).map_err(|error| error.to_string()))
        } else {
            meeting_record(&entry.path(), &id)
        };
        match meeting {
            Ok(meeting)
                if matches!(
                    meeting.status,
                    MeetingStatus::Recording | MeetingStatus::Processing
                ) =>
            {
                plan.in_progress += 1
            }
            Ok(_) => plan.importable.push(id),
            Err(_) => plan.unreadable += 1,
        }
    }
    Ok(Some(plan))
}

/// Copy one meeting into `stage`, then rename it to `target` only if that
/// name is still free. On any failure the staged copy is removed; the
/// source is only ever read.
fn copy_meeting(source: &Path, stage: &Path, target: &Path) -> std::io::Result<()> {
    crate::paths::validate_tree_without_reparse(source)?;
    let name = target.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "meeting has no name")
    })?;
    let staged = stage.join(name);
    std::fs::create_dir(&staged)?;
    let result = (|| {
        for file in MEETING_FILES {
            let from = source.join(file);
            match std::fs::symlink_metadata(&from) {
                Ok(_) => crate::paths::copy_file_synced(&from, &staged.join(file))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        let audio = source.join(MEETING_AUDIO);
        if crate::paths::is_plain_directory(&audio) {
            let into = staged.join(MEETING_AUDIO);
            std::fs::create_dir(&into)?;
            for item in std::fs::read_dir(&audio)? {
                let item = item?;
                let file_name = item.file_name();
                let Some(text) = file_name.to_str() else {
                    continue;
                };
                if text.starts_with('.')
                    || !text.to_ascii_lowercase().ends_with(".wav")
                    || !std::fs::symlink_metadata(item.path())?.is_file()
                {
                    continue;
                }
                crate::paths::copy_file_synced(&item.path(), &into.join(&file_name))?;
            }
        }
        crate::paths::validate_tree_without_reparse(&staged)?;
        if std::fs::symlink_metadata(target).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "a meeting with this identifier appeared meanwhile",
            ));
        }
        crate::storage::atomic_move_into_reserved(&staged, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staged);
    }
    result
}

/// Remove stages an interrupted import left behind. They are never the only
/// copy of anything: the previous folder still has every meeting in them.
fn clean_stale_stages(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let stale = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(STAGE_PREFIX));
        if stale && crate::paths::validate_tree_without_reparse(&entry.path()).is_ok() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn import_meetings(previous: &Path, base: &Path) -> Result<Option<Value>, String> {
    let Some(plan) = plan_meetings(previous, base, true)? else {
        return Ok(None);
    };
    let mut copied = 0usize;
    let mut failed = plan.unreadable;
    if !plan.importable.is_empty() {
        let local = crate::paths::ensure_trusted_data_subdir(base, Path::new("meetings"))
            .map_err(|error| error.to_string())?;
        clean_stale_stages(base);
        let stage = base.join(format!("{STAGE_PREFIX}{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir(&stage).map_err(|error| error.to_string())?;
        let root = allowed(previous, "meetings");
        for id in &plan.importable {
            match copy_meeting(&root.join(id.as_str()), &stage, &local.join(id.as_str())) {
                Ok(()) => copied += 1,
                Err(error) => {
                    log::warn!("previous meeting {} was not imported: {error}", id.as_str());
                    failed += 1;
                }
            }
        }
        let _ = std::fs::remove_dir_all(&stage);
    }
    Ok(Some(json!({
        "copied": copied,
        "present": plan.present,
        "in_progress": plan.in_progress,
        "failed": failed,
    })))
}

fn previous_stats(
    previous: &Path,
) -> Result<(Option<crate::Totals>, Option<crate::activity::Activity>), String> {
    let unreadable = |_| "The previous usage counts could not be read.".to_string();
    let totals = read(&allowed(previous, "totals.json"), MAX_TOTALS_DOCUMENT_BYTES)?
        .map(|bytes| serde_json::from_slice::<crate::Totals>(&bytes))
        .transpose()
        .map_err(unreadable)?;
    let activity = read(
        &allowed(previous, "activity.json"),
        crate::activity::MAX_FILE_BYTES,
    )?
    .map(|bytes| serde_json::from_slice::<crate::activity::Activity>(&bytes))
    .transpose()
    .map_err(unreadable)?;
    Ok((totals, activity))
}

/// Add the previous usage counts, once. Returns what landed; each part that
/// could not be added is pushed to `failures` on its own, so a partial
/// import is reported next to what did arrive.
fn import_stats(
    previous: &Path,
    base: &Path,
    status: &RuntimeStatus,
    marker: &mut Result<Marker, String>,
    totals_writable: bool,
    failures: &mut Vec<String>,
) -> Option<Value> {
    let Ok(marker) = marker.as_mut() else {
        failures.push("The import record in this installation is unreadable, so usage counts were not added: they must never be counted twice.".into());
        return None;
    };
    if marker.stats_imported {
        return Some(json!({"already": true}));
    }
    let (totals, activity) = match previous_stats(previous) {
        Ok(found) => found,
        Err(error) => {
            failures.push(error);
            return None;
        }
    };
    let mut summary = json!({});
    if let Some(activity) = activity {
        match status.activity.merge_imported(&activity) {
            Ok(days) => summary["days"] = json!(days),
            Err(error) => failures.push(error),
        }
    }
    if let Some(totals) = totals {
        match crate::add_imported_totals(status, &base.join("totals.json"), totals_writable, totals)
        {
            Ok(_) => {
                summary["dictations"] = json!(totals.dictations);
                summary["words"] = json!(totals.words);
            }
            Err(error) => failures.push(error),
        }
    }
    // Record the addition as soon as any of it landed: a count left out can
    // be explained, a count added twice cannot be taken back.
    if summary.as_object().is_some_and(|fields| !fields.is_empty()) {
        marker.stats_imported = true;
        if let Err(error) = write_marker(base, marker) {
            failures.push(error);
        }
        return Some(summary);
    }
    None
}

fn import(
    previous: &Path,
    base: &Path,
    status: &RuntimeStatus,
    parts: Parts,
    totals_writable: bool,
) -> Result<Value, String> {
    let mut result = json!({"imported": true});
    let mut errors = Vec::new();
    let mut marker = read_marker(base);
    if parts.settings {
        match previous_settings(previous) {
            Ok(settings) => result["settings"] = json!(settings),
            Err(error) => errors.push(problem("settings", error)),
        }
    }
    if parts.profiles {
        let workflows = previous_personalization(previous, "workflows.json", MAX_WORKFLOW_BYTES)
            .and_then(|bytes| {
                bytes
                    .map(|bytes| crate::workflows::import_previous(base, status, &bytes))
                    .transpose()
            });
        match workflows {
            Ok(Some(true)) => result["workflows"] = json!("imported"),
            Ok(Some(false)) => result["workflows"] = json!("kept"),
            Ok(None) => {}
            Err(error) => errors.push(problem("profiles", error)),
        }
    }
    if parts.dictionary {
        match previous_rules(previous).and_then(|rules| {
            rules
                .map(|rules| {
                    merge(base, Kind::Dictionary, &rules.entries).map(|mut counts| {
                        counts["ignored"] = json!(rules.ignored);
                        counts
                    })
                })
                .transpose()
        }) {
            Ok(counts) => result["dictionary"] = json!(counts),
            Err(error) => errors.push(problem("dictionary", error)),
        }
        match previous_snippets(previous).and_then(|entries| {
            entries
                .map(|entries| merge(base, Kind::Snippets, &entries))
                .transpose()
        }) {
            Ok(counts) => result["snippets"] = json!(counts),
            Err(error) => errors.push(problem("dictionary", error)),
        }
        // The engine applies the merged words from the next dictation on.
        match crate::migration::publish_runtime(base, status) {
            Ok(state) => result["state"] = state,
            Err(error) => errors.push(problem("dictionary", error)),
        }
    }
    if parts.meetings {
        match import_meetings(previous, base) {
            Ok(summary) => {
                if summary
                    .as_ref()
                    .is_some_and(|summary| summary["copied"].as_u64().unwrap_or(0) > 0)
                {
                    let _ = status
                        .meetings
                        .refresh("Meetings from the previous VocalCode were added.".into());
                }
                result["meetings"] = json!(summary);
            }
            Err(error) => errors.push(problem("meetings", error)),
        }
    }
    if parts.stats {
        let mut failures = Vec::new();
        let summary = import_stats(
            previous,
            base,
            status,
            &mut marker,
            totals_writable,
            &mut failures,
        );
        result["stats"] = json!(summary);
        errors.extend(failures.into_iter().map(|error| problem("stats", error)));
    }
    if parts.models {
        let models = allowed(previous, "models");
        if crate::paths::is_plain_directory(&models) {
            match crate::models::import_previous_models(base, &models, true) {
                Ok(summary) => {
                    if summary.files > 0 {
                        // A route that was waiting for these files can load now.
                        status.reload_model.store(true, Ordering::Release);
                    }
                    result["models"] = json!(summary);
                }
                Err(error) => errors.push(problem("models", error)),
            }
        }
    }
    if let Ok(marker) = marker.as_mut() {
        marker.imported_at_ms = now_ms();
        if let Err(error) = write_marker(base, marker) {
            errors.push(problem("record", error));
        }
    }
    result["errors"] = json!(errors);
    Ok(result)
}

/// Whether this installation already has anything a person made: the Home
/// suggestion is only for someone who has not started over here yet.
fn community_has_user_data(base: &Path, status: &RuntimeStatus) -> bool {
    let dictated = status
        .totals
        .lock()
        .map(|totals| totals.dictations > 0)
        .unwrap_or(true);
    let has_entries = |kind| {
        crate::migration::snapshot(base, kind)
            .map(|(_, entries)| !entries.is_empty())
            .unwrap_or(true)
    };
    let has_meetings = std::fs::read_dir(base.join("meetings")).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| MeetingId::parse(name.to_string()).is_ok())
        })
    });
    dictated
        || !status.activity.is_empty()
        || has_entries(Kind::Dictionary)
        || has_entries(Kind::Snippets)
        || has_meetings
}

/// Read-only: what an import would bring. Every part reports on its own, so
/// one unreadable file does not hide the rest.
fn scan(previous: &Path, base: &Path, status: &RuntimeStatus) -> Value {
    let marker = read_marker(base).unwrap_or_default();
    let mut problems = Vec::new();
    let mut note = |part: &str, error: String| problems.push(problem(part, error));
    let settings = previous_settings(previous)
        .map(|settings| settings.is_some())
        .unwrap_or_else(|error| {
            note("settings", error);
            false
        });
    let profiles = previous_profiles(previous, base).unwrap_or_else(|error| {
        note("profiles", error);
        None
    });
    let rules = previous_rules(previous)
        .map(|rules| rules.map_or(0, |rules| rules.entries.len()))
        .unwrap_or_else(|error| {
            note("dictionary", error);
            0
        });
    let snippets = previous_snippets(previous)
        .map(|entries| entries.map_or(0, |entries| entries.len()))
        .unwrap_or_else(|error| {
            note("dictionary", error);
            0
        });
    let meetings = plan_meetings(previous, base, false)
        .unwrap_or_else(|error| {
            note("meetings", error);
            None
        })
        .unwrap_or_default();
    let stats = match previous_stats(previous) {
        Ok((None, None)) => Value::Null,
        Ok((totals, activity)) => json!({
            "dictations": totals.map_or(0, |totals| totals.dictations),
            "words": totals.map_or(0, |totals| totals.words),
            "days": activity.map_or(0, |activity| activity.days.len()),
        }),
        Err(error) => {
            note("stats", error);
            Value::Null
        }
    };
    let models_folder = allowed(previous, "models");
    let models = if crate::paths::is_plain_directory(&models_folder) {
        crate::models::import_previous_models(base, &models_folder, false).unwrap_or_else(|error| {
            note("models", error);
            Default::default()
        })
    } else {
        Default::default()
    };
    json!({
        "available": true,
        "path": previous.display().to_string(),
        "settings": settings,
        "profiles": profiles,
        "rules": rules,
        "snippets": snippets,
        "meetings": meetings.importable.len(),
        "meetings_present": meetings.present,
        "meetings_in_progress": meetings.in_progress,
        "meetings_unreadable": meetings.unreadable,
        "stats": stats,
        "stats_imported": marker.stats_imported,
        "models": models,
        "problems": problems,
        "community_empty": !community_has_user_data(base, status),
        "answered": marker.dismissed || marker.imported_at_ms > 0,
        "login": crate::webui::previous_edition_autostart(),
    })
}

/// The previous edition's single-instance name. Its GUI holds this mutex for
/// its whole lifetime, so its installer can tell whether it is running.
#[cfg(windows)]
const PREVIOUS_EDITION_MUTEX: &str = r"Local\VocalCode.Desktop";

#[cfg(windows)]
fn named_mutex_exists(name: &str) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};

    let name = name.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the call.
    let handle = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
    if handle.is_null() {
        // That app's mutex carries a private DACL. A name we may not open
        // still proves it is held; only "not found" means not running.
        // SAFETY: reads this thread's last-error value, set by the call above.
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    }
    // SAFETY: `handle` is the valid handle just returned and is closed once.
    unsafe { CloseHandle(handle) };
    true
}

/// Whether the previous VocalCode is running now. Both would type every
/// dictation, each with its own hotkey hook and microphone.
#[cfg(windows)]
pub(crate) fn previous_edition_running() -> bool {
    crate::community::ENABLED && named_mutex_exists(PREVIOUS_EDITION_MUTEX)
}

#[cfg(target_os = "macos")]
pub(crate) fn previous_edition_running() -> bool {
    use objc2_app_kit::NSRunningApplication;
    use objc2_foundation::NSString;

    if !crate::community::ENABLED {
        return false;
    }
    let identifier = NSString::from_str(crate::webui::PREVIOUS_BUNDLE_ID);
    !NSRunningApplication::runningApplicationsWithBundleIdentifier(&identifier).is_empty()
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn previous_edition_running() -> bool {
    false
}

/// [`previous_edition_running`] for the status tick: re-checked at most every
/// few seconds, so the banner clears soon after the other app quits.
pub(crate) fn previous_edition_running_cached() -> bool {
    static CACHE: std::sync::Mutex<Option<(Instant, bool)>> = std::sync::Mutex::new(None);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((checked, running)) = *cache {
        if checked.elapsed() < Duration::from_secs(3) {
            return running;
        }
    }
    let running = previous_edition_running();
    *cache = Some((Instant::now(), running));
    running
}

#[cfg(test)]
mod tests {
    use super::*;
    use vocalcode_meeting::{
        AudioRetention, AudioSource, MeetingSource, NewMeeting, Speaker, TranscriptSegment,
    };

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "vocalcode-previous-{name}-{}-{}-{}",
                std::process::id(),
                now_ms(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(path.join("previous")).unwrap();
            std::fs::create_dir_all(path.join("community")).unwrap();
            Self(path)
        }
        fn previous(&self) -> PathBuf {
            self.0.join("previous")
        }
        fn community(&self) -> PathBuf {
            self.0.join("community")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: PathBuf, bytes: impl AsRef<[u8]>) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn status_for(base: &Path) -> RuntimeStatus {
        let status = RuntimeStatus::default();
        status.activity.load(base);
        status
    }

    fn all() -> Parts {
        Parts {
            settings: true,
            profiles: true,
            dictionary: true,
            meetings: true,
            stats: true,
            models: true,
        }
    }

    /// A finished meeting with one transcript line and one audio chunk, made
    /// by the real store so the fixture is exactly what the app writes.
    fn meeting(root: &Path, now_ms: u64, status: MeetingStatus) -> MeetingId {
        let store = MeetingStore::open(root).unwrap();
        let mut meeting = store
            .create(NewMeeting {
                title: "Standup".into(),
                now_ms,
                source: MeetingSource::Live {
                    microphone: true,
                    system_audio: false,
                },
                language: "en".into(),
                audio_retention: AudioRetention::KeepUntilDeleted,
            })
            .unwrap();
        meeting.speakers.push(Speaker {
            id: "me".into(),
            label: "Me".into(),
            source: AudioSource::Microphone,
        });
        meeting.status = status;
        meeting.duration_ms = 2_000;
        meeting.ended_at_ms = Some(now_ms + 2_000);
        store.save(&meeting).unwrap();
        store
            .append_segment(
                &meeting.id,
                &TranscriptSegment {
                    id: 1,
                    start_ms: 0,
                    end_ms: 1_500,
                    speaker_id: "me".into(),
                    source: AudioSource::Microphone,
                    text: "Ship the import today.".into(),
                },
            )
            .unwrap();
        let audio = store.audio_directory(&meeting.id).unwrap();
        std::fs::write(audio.join("microphone-000000.wav"), b"RIFF finished chunk").unwrap();
        std::fs::write(
            audio.join(".microphone-000001.wav.part-7-1"),
            b"interrupted writer",
        )
        .unwrap();
        meeting.id
    }

    const PREVIOUS_RULES: &str = "# VocalCode — your own recognition fixes.\n\
        # One rule per line:  heard text => what to write\n\
        #\n\
        # Example (edit or delete):\n\
        # my startup name => Acme\n\
        collie          => Collie\n\
        vocal code      => VocalCode\n\
        lang chain      => LangChain\n\
        a line the previous app ignored too\n";

    /// Everything the paid edition could have in its folder, including every
    /// credential-looking and unknown name that must never be copied.
    fn previous_tree(previous: &Path) -> MeetingId {
        write(
            previous.join("vocalcode.toml"),
            "language = \"zh\"\nmodel = \"\"\nautostart = true\nonboarded = true\n\
             cue_sounds = false\nconfig_version = 3\ntalk_mode = \"toggle\"\n\
             retired_setting = \"kept by serde, ignored here\"\n",
        );
        write(previous.join("replacements.txt"), PREVIOUS_RULES);
        write(
            previous.join("totals.json"),
            r#"{"dictations":40,"words":900,"chars":5000}"#,
        );
        write(
            previous.join("activity.json"),
            r#"{"version":1,"days":{"2026-09-01":{"dictations":3,"words":30,"speech_ms":9000},"2026-09-02":{"dictations":1,"words":5,"speech_ms":2000}}}"#,
        );
        write(
            previous.join("personalization/snippets.json"),
            format::export(
                Kind::Snippets,
                vec![Entry {
                    name: "sig".into(),
                    text: "Best, Daming".into(),
                }],
            )
            .unwrap(),
        );
        write(
            previous.join("personalization/workflows.json"),
            r#"{"schema":1,"diagnostics":true,"cleanup":"original","profiles":[{"app_id":"Code.exe","cleanup":"light","progressive":null,"paste":null}]}"#,
        );
        write(
            previous.join("personalization/.vocalcode-rules-write.lock"),
            b"",
        );
        let id = meeting(
            &previous.join("meetings"),
            1_780_000_000_000,
            MeetingStatus::Completed,
        );
        meeting(
            &previous.join("meetings"),
            1_780_000_100_000,
            MeetingStatus::Recording,
        );
        write(previous.join("meetings/not-a-meeting/secret.txt"), "no");
        for denied in [
            "vocalcode-license.json",
            "vocalcode-license.legacy.json",
            "vocalcode-trial.dat",
            "vocalcode-time-anchor.bin",
            "vocalcode-time-anchor.json",
            "vocalcode.log",
            "vocalcode.log.1",
            ".vocalcode-rules-write.lock",
            ".vocalcode-config-write.lock",
            "secure/diagnostics.key",
            "calendar/google-oauth.json",
            "diagnostic-history/2026-09.jsonl",
            "webview2/EBWebView/Local State",
            "unknown-future-file.json",
            "models/not-in-manifest/model.onnx",
        ] {
            write(previous.join(denied), "must never be copied");
        }
        id
    }

    fn files_under(root: &Path) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    out.push(
                        path.strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    #[test]
    fn copies_only_the_allow_list_and_never_changes_the_previous_folder() {
        let scratch = Scratch::new("allow-list");
        let (previous, base) = (scratch.previous(), scratch.community());
        let id = previous_tree(&previous);
        let before = crate::paths::hash_migration_tree(&previous).unwrap();
        let status = status_for(&base);

        let scan = scan(&previous, &base, &status);
        assert_eq!(scan["rules"], 3);
        assert_eq!(scan["snippets"], 1);
        assert_eq!(scan["meetings"], 1);
        assert_eq!(scan["meetings_in_progress"], 1);
        assert_eq!(scan["settings"], true);
        assert_eq!(scan["profiles"], "new");
        assert_eq!(scan["community_empty"], true);
        assert_eq!(scan["problems"], json!([]));
        assert_eq!(scan["stats"]["dictations"], 40);
        assert_eq!(
            crate::paths::hash_migration_tree(&previous).unwrap(),
            before,
            "a scan must not write to the previous folder"
        );

        let result = import(&previous, &base, &status, all(), true).unwrap();
        assert_eq!(result["errors"], json!([]), "{result}");
        assert_eq!(
            crate::paths::hash_migration_tree(&previous).unwrap(),
            before,
            "the previous folder must be byte-for-byte untouched"
        );

        let copied = files_under(&base);
        for denied in [
            "license",
            "trial",
            "anchor",
            ".log",
            "secure",
            "calendar",
            "diagnostic",
            "webview2",
            "unknown",
            "secret",
            "not-in-manifest",
            ".part-",
        ] {
            assert!(
                copied.iter().all(|path| !path.contains(denied)),
                "{denied} leaked into {copied:?}"
            );
        }
        let meeting_dir = format!("meetings/{}", id.as_str());
        for expected in [
            format!("{meeting_dir}/meeting.json"),
            format!("{meeting_dir}/transcript.jsonl"),
            format!("{meeting_dir}/audio/microphone-000000.wav"),
            "personalization/snippets.json".to_string(),
            "personalization/workflows.json".to_string(),
            "replacements.txt".to_string(),
            "totals.json".to_string(),
            "activity.json".to_string(),
            MARKER.to_string(),
        ] {
            assert!(
                copied.contains(&expected),
                "{expected} missing in {copied:?}"
            );
        }
        assert!(
            copied.iter().all(|path| !path.starts_with(STAGE_PREFIX)),
            "{copied:?}"
        );
        // Only the finished meeting; the one still recording stays behind.
        assert_eq!(
            MeetingStore::open(base.join("meetings"))
                .unwrap()
                .list()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(result["meetings"]["copied"], 1);
        assert_eq!(result["meetings"]["in_progress"], 1);

        // Settings come back as a page save, minus what this install owns.
        let settings = &result["settings"];
        assert_eq!(settings["language"], "zh");
        assert_eq!(settings["talk_mode"], "toggle");
        assert_eq!(settings["cue_sounds"], false);
        for owned in ["autostart", "onboarded", "mute_available"] {
            assert!(settings.get(owned).is_none(), "{owned}");
        }
        // Application profiles carry over; encrypted text history does not.
        let workflows: Value = serde_json::from_slice(
            &std::fs::read(base.join("personalization/workflows.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(workflows["diagnostics"], false);
        assert_eq!(workflows["profiles"][0]["app_id"], "Code.exe");
        assert_eq!(result["workflows"], "imported");
        // Now this installation has data and profiles of its own.
        let after = super::scan(&previous, &base, &status);
        assert_eq!(after["community_empty"], false);
        assert_eq!(after["profiles"], "kept");

        assert_eq!(result["snippets"]["added"], 1);
        assert_eq!(status.snippets.lock().unwrap().len(), 1);
        let totals: Value =
            serde_json::from_slice(&std::fs::read(base.join("totals.json")).unwrap()).unwrap();
        assert_eq!(totals["dictations"], 40);
        assert_eq!(result["stats"]["days"], 2);
        assert!(!status.activity.is_empty());
    }

    #[test]
    fn previous_rules_merge_through_the_dictionary_importer() {
        let scratch = Scratch::new("rules");
        let (previous, base) = (scratch.previous(), scratch.community());
        write(previous.join("replacements.txt"), PREVIOUS_RULES);
        let status = status_for(&base);
        // This installation already has words: one the same, one different.
        let (revision, _) = crate::migration::snapshot(&base, Kind::Dictionary).unwrap();
        crate::migration::save(
            &base,
            Kind::Dictionary,
            &revision,
            &[
                Entry {
                    name: "collie".into(),
                    text: "Collie".into(),
                },
                Entry {
                    name: "vocal code".into(),
                    text: "Vocal Code".into(),
                },
            ],
        )
        .unwrap();
        let parts = Parts {
            dictionary: true,
            ..Parts::default()
        };

        let result = import(&previous, &base, &status, parts, true).unwrap();
        let counts = &result["dictionary"];
        assert_eq!(
            (
                &counts["added"],
                &counts["duplicates"],
                &counts["conflicts"],
                &counts["ignored"]
            ),
            (&json!(1), &json!(1), &json!(1), &json!(1))
        );
        let (_, saved) = crate::migration::snapshot(&base, Kind::Dictionary).unwrap();
        let pairs = saved
            .iter()
            .map(|entry| (entry.name.as_str(), entry.text.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            pairs,
            [
                ("collie", "Collie"),
                // Existing entries win a conflict.
                ("vocal code", "Vocal Code"),
                ("lang chain", "LangChain"),
            ]
        );
        assert!(status
            .rules
            .lock()
            .unwrap()
            .iter()
            .any(|(from, to)| from == "lang chain" && to == "LangChain"));

        // Importing again adds nothing and changes nothing.
        let again = import(&previous, &base, &status, parts, true).unwrap();
        assert_eq!(again["dictionary"]["added"], 0);
        assert_eq!(
            crate::migration::snapshot(&base, Kind::Dictionary)
                .unwrap()
                .1,
            saved
        );
    }

    #[test]
    fn the_built_in_dictionary_file_reads_like_the_app_reads_it() {
        let file = format::parse_rules_file(crate::DEFAULT_RULES_DOC).unwrap();
        let app = crate::default_rules();
        assert_eq!(file.ignored, 0);
        assert_eq!(
            file.entries
                .into_iter()
                .map(|entry| (entry.name, entry.text))
                .collect::<Vec<_>>(),
            app
        );
    }

    #[test]
    fn usage_counts_are_added_once_and_meetings_are_never_duplicated() {
        let scratch = Scratch::new("once");
        let (previous, base) = (scratch.previous(), scratch.community());
        previous_tree(&previous);
        write(
            base.join("totals.json"),
            r#"{"dictations":2,"words":10,"chars":50}"#,
        );
        let status = status_for(&base);
        *status.totals.lock().unwrap() = crate::Totals {
            dictations: 2,
            words: 10,
            chars: 50,
        };
        let parts = Parts {
            meetings: true,
            stats: true,
            ..Parts::default()
        };
        import(&previous, &base, &status, parts, true).unwrap();
        let second = import(&previous, &base, &status, parts, true).unwrap();
        assert_eq!(second["stats"]["already"], true);
        assert_eq!(second["meetings"]["copied"], 0);
        assert_eq!(second["meetings"]["present"], 1);
        assert_eq!(status.totals.lock().unwrap().dictations, 42);
        let totals: Value =
            serde_json::from_slice(&std::fs::read(base.join("totals.json")).unwrap()).unwrap();
        assert_eq!(totals["words"], 910);
        assert_eq!(scan(&previous, &base, &status)["answered"], true);
    }

    #[test]
    fn counts_are_not_written_over_totals_this_session_could_not_read() {
        let scratch = Scratch::new("unreadable-totals");
        let (previous, base) = (scratch.previous(), scratch.community());
        write(
            previous.join("totals.json"),
            r#"{"dictations":40,"words":900,"chars":5000}"#,
        );
        write(base.join("totals.json"), "damaged, preserved for recovery");
        let status = status_for(&base);
        let parts = Parts {
            stats: true,
            ..Parts::default()
        };
        let result = import(&previous, &base, &status, parts, false).unwrap();
        // Named by part, with the sentence itself for the page to translate.
        assert_eq!(
            result["errors"],
            json!([{
                "part": "stats",
                "message": "This installation's usage totals could not be read, so nothing was added to them.",
            }]),
            "{result}"
        );
        assert_eq!(
            std::fs::read(base.join("totals.json")).unwrap(),
            b"damaged, preserved for recovery"
        );
        assert!(!read_marker(&base).unwrap().stats_imported);
    }

    #[test]
    fn an_unreadable_meeting_is_left_behind_so_the_list_keeps_working() {
        let scratch = Scratch::new("bad-meeting");
        let (previous, base) = (scratch.previous(), scratch.community());
        let good = meeting(
            &previous.join("meetings"),
            1_780_000_000_000,
            MeetingStatus::Completed,
        );
        let bad = meeting(
            &previous.join("meetings"),
            1_780_000_200_000,
            MeetingStatus::Completed,
        );
        write(
            previous
                .join("meetings")
                .join(bad.as_str())
                .join("meeting.json"),
            "{ not a meeting",
        );
        // A good record over a damaged transcript: only the full check sees it.
        let torn = meeting(
            &previous.join("meetings"),
            1_780_000_300_000,
            MeetingStatus::Completed,
        );
        write(
            previous
                .join("meetings")
                .join(torn.as_str())
                .join("transcript.jsonl"),
            "{\"id\":1}\n{\"id\":2}\n",
        );
        let status = status_for(&base);
        let scanned = scan(&previous, &base, &status);
        assert_eq!(
            (&scanned["meetings"], &scanned["meetings_unreadable"]),
            (&json!(2), &json!(1))
        );
        let parts = Parts {
            meetings: true,
            ..Parts::default()
        };
        let result = import(&previous, &base, &status, parts, true).unwrap();
        assert_eq!(result["meetings"]["copied"], 1);
        assert_eq!(result["meetings"]["failed"], 2);
        let listed = MeetingStore::open(base.join("meetings"))
            .unwrap()
            .list()
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, good);
    }

    /// The full check reads each transcript through the meeting store, which
    /// opens files by path. A folder with a link anywhere in it is refused
    /// before anything in it is read, not only later by the copy.
    #[test]
    fn a_meeting_folder_containing_a_link_is_refused_before_it_is_read() {
        let scratch = Scratch::new("inner-link");
        let (previous, base) = (scratch.previous(), scratch.community());
        let id = meeting(
            &previous.join("meetings"),
            1_780_000_000_000,
            MeetingStatus::Completed,
        );
        let folder = previous.join("meetings").join(id.as_str());
        let outside = scratch.0.join("outside-audio");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("microphone-000009.wav"), b"elsewhere").unwrap();
        std::fs::remove_dir_all(folder.join("audio")).unwrap();
        link_directory(&folder.join("audio"), &outside);

        let plan = plan_meetings(&previous, &base, true).unwrap().unwrap();
        assert!(plan.importable.is_empty());
        assert_eq!(plan.unreadable, 1);
        let status = status_for(&base);
        let parts = Parts {
            meetings: true,
            ..Parts::default()
        };
        let result = import(&previous, &base, &status, parts, true).unwrap();
        assert_eq!(
            (&result["meetings"]["copied"], &result["meetings"]["failed"]),
            (&json!(0), &json!(1))
        );
        assert!(!base.join("meetings").join(id.as_str()).exists());
    }

    /// "Not now" over an import record this build cannot read must not
    /// replace it with a fresh one that forgets the counts were added.
    #[test]
    fn not_now_leaves_an_unreadable_import_record_alone() {
        let scratch = Scratch::new("dismiss");
        let base = scratch.community();
        assert_eq!(dismiss(&base).unwrap()["dismissed"], true);
        assert!(read_marker(&base).unwrap().dismissed);
        write(base.join(MARKER), "{ damaged");
        assert_eq!(dismiss(&base).unwrap_err(), MARKER_UNREADABLE);
        assert_eq!(std::fs::read(base.join(MARKER)).unwrap(), b"{ damaged");
    }

    /// Application profiles are their own part: they arrive even when the
    /// settings file has nothing usable, and never over profiles saved here.
    #[test]
    fn app_profiles_import_on_their_own_and_never_over_existing_ones() {
        let scratch = Scratch::new("profiles");
        let (previous, base) = (scratch.previous(), scratch.community());
        write(previous.join("vocalcode.toml"), "config_version = 3\n");
        write(
            previous.join("personalization/workflows.json"),
            r#"{"schema":1,"diagnostics":true,"cleanup":"original","profiles":[{"app_id":"Code.exe","cleanup":"light","progressive":null,"paste":null}]}"#,
        );
        let status = status_for(&base);
        let scanned = scan(&previous, &base, &status);
        assert_eq!(
            (&scanned["settings"], &scanned["profiles"]),
            (&json!(false), &json!("new"))
        );
        let parts = Parts {
            profiles: true,
            ..Parts::default()
        };
        let result = import(&previous, &base, &status, parts, true).unwrap();
        assert_eq!(result["workflows"], "imported");
        assert!(result.get("settings").is_none(), "{result}");
        let saved = std::fs::read(base.join("personalization/workflows.json")).unwrap();
        write(
            previous.join("personalization/workflows.json"),
            r#"{"schema":1,"cleanup":"light","profiles":[]}"#,
        );
        assert_eq!(scan(&previous, &base, &status)["profiles"], "kept");
        let again = import(&previous, &base, &status, parts, true).unwrap();
        assert_eq!(again["workflows"], "kept");
        assert_eq!(
            std::fs::read(base.join("personalization/workflows.json")).unwrap(),
            saved
        );
        write(previous.join("personalization/workflows.json"), "{ damaged");
        let problems = scan(&previous, &base, &status)["problems"].clone();
        assert_eq!(
            problems,
            json!([{"part": "profiles", "message": "The previous app profiles could not be read."}])
        );
    }

    #[test]
    fn a_future_settings_file_is_refused_and_an_unchosen_language_is_kept() {
        let scratch = Scratch::new("settings");
        let previous = scratch.previous();
        write(previous.join("vocalcode.toml"), "config_version = 999\n");
        assert!(previous_settings(&previous).unwrap_err().contains("newer"));
        write(
            previous.join("vocalcode.toml"),
            "language = \"auto\"\nmodel = \"retired-model\"\n",
        );
        assert_eq!(
            previous_settings(&previous).unwrap(),
            None,
            "nothing usable is not reported as imported settings"
        );
        write(
            previous.join("vocalcode.toml"),
            "language = \"auto\"\nmodel = \"retired-model\"\ncue_sounds = false\n",
        );
        let settings = previous_settings(&previous).unwrap().unwrap();
        assert!(settings.get("language").is_none());
        assert!(settings.get("model").is_none());
        assert_eq!(settings["cue_sounds"], false);
        write(
            previous.join("vocalcode.toml"),
            "language = \"en\"\nmodel = \"retired-model\"\n",
        );
        let settings = previous_settings(&previous).unwrap().unwrap();
        assert_eq!(settings["language"], "en");
        assert_eq!(settings["model"], "");
    }

    /// Settings a previous release never wrote (Writing rules, the noise
    /// filter, the control bar) keep this installation's values instead of
    /// arriving as defaults.
    #[test]
    fn only_settings_the_previous_file_contains_are_carried_over() {
        let scratch = Scratch::new("settings-subset");
        let previous = scratch.previous();
        write(
            previous.join("vocalcode.toml"),
            "talk = [{ key = \"rightctrl\" }]\nlanguage = \"zh\"\nconfig_version = 2\n\
             onboarded = true\nautostart = true\n",
        );
        let settings = previous_settings(&previous).unwrap().unwrap();
        let mut keys = settings
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(keys, ["language", "model", "talk"]);
        // `migrate` canonicalises the old key spelling and picks the model the
        // current release recommends for that language.
        assert_eq!(settings["talk"], json!(["key:ControlRight"]));
        assert_eq!(settings["model"], "sensevoice");
    }

    /// A directory link, like a junction on Windows (which needs no
    /// Developer Mode, so this runs on ordinary test machines too).
    fn link_directory(link: &Path, target: &Path) {
        #[cfg(windows)]
        let created = std::process::Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap()
            .status
            .success();
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(target, link).is_ok();
        assert!(created, "could not link {}", link.display());
    }

    #[test]
    fn a_linked_meeting_or_personalization_folder_is_not_followed() {
        let scratch = Scratch::new("links");
        let (previous, base) = (scratch.previous(), scratch.community());
        let outside = scratch.0.join("outside");
        let id = meeting(
            &outside.join("meetings"),
            1_780_000_000_000,
            MeetingStatus::Completed,
        );
        let snippets = format::export(
            Kind::Snippets,
            vec![Entry {
                name: "outside".into(),
                text: "outside the previous folder".into(),
            }],
        )
        .unwrap();
        write(outside.join("personalization/snippets.json"), snippets);
        std::fs::create_dir_all(previous.join("meetings")).unwrap();
        link_directory(
            &previous.join("personalization"),
            &outside.join("personalization"),
        );
        link_directory(
            &previous.join("meetings").join(id.as_str()),
            &outside.join("meetings").join(id.as_str()),
        );
        let before = crate::paths::hash_migration_tree(&outside).unwrap();

        assert_eq!(previous_snippets(&previous).unwrap(), None);
        let status = status_for(&base);
        let result = import(&previous, &base, &status, all(), true).unwrap();
        assert!(result["snippets"].is_null(), "{result}");
        assert_eq!(result["meetings"]["copied"], 0);
        assert!(!base.join("personalization/snippets.json").exists());
        assert!(!base.join("meetings").join(id.as_str()).exists());
        assert_eq!(crate::paths::hash_migration_tree(&outside).unwrap(), before);
    }

    #[cfg(windows)]
    #[test]
    fn a_held_named_mutex_reads_as_a_running_edition() {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::CreateMutexW;

        let name = format!(r"Local\VocalCode.Test.{}.{}", std::process::id(), now_ms());
        assert!(!named_mutex_exists(&name));
        let wide = name.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        // SAFETY: `wide` is NUL-terminated; default security, not owned.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        assert!(!handle.is_null());
        assert!(named_mutex_exists(&name));
        // SAFETY: the handle created above, closed exactly once.
        unsafe { CloseHandle(handle) };
        assert!(!named_mutex_exists(&name));
    }
}
