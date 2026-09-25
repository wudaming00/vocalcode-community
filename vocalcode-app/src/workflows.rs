//! Local, revision-bound preferences. No window titles, audio, credentials or
//! remote service endpoints are stored in this document.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Cleanup {
    Original,
    #[default]
    Light,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub app_id: String,
    pub cleanup: Cleanup,
    pub progressive: Option<bool>,
    pub paste: Option<bool>,
    #[serde(default)]
    pub remove_fillers: Option<bool>,
    #[serde(default)]
    pub chinese_fillers: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Preferences {
    pub schema: u32,
    pub diagnostics: bool,
    /// Zero = unlimited. No time-based deletion. Limits stop recording new
    /// diagnostic entries rather than deleting earlier evidence automatically.
    pub max_entries: u64,
    pub max_bytes: u64,
    pub cleanup: Cleanup,
    /// Missing in existing files means OFF: never silently edit more text after
    /// an upgrade. This option does not require diagnostic persistence.
    pub remove_fillers: bool,
    /// Separate opt-in: enabling English cleanup never enables Chinese deletion.
    pub chinese_fillers: bool,
    pub profiles: Vec<Profile>,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            schema: 1,
            diagnostics: false,
            max_entries: 100_000,
            max_bytes: 2 * 1024 * 1024 * 1024,
            cleanup: Cleanup::Light,
            remove_fillers: false,
            chinese_fillers: false,
            profiles: vec![],
        }
    }
}
impl Preferences {
    pub(crate) fn fillers_for(&self, app: &str, language: &str) -> bool {
        let chinese = vocalcode_core::fillers::is_chinese(language);
        self.profiles
            .iter()
            .find(|p| p.app_id.eq_ignore_ascii_case(app))
            .and_then(|p| {
                if chinese {
                    p.chinese_fillers
                } else {
                    p.remove_fillers
                }
            })
            .unwrap_or(if chinese {
                self.chinese_fillers
            } else {
                self.remove_fillers
            })
    }
    fn validate(&self) -> Result<(), String> {
        if self.schema != 1 || self.profiles.len() > 100 {
            return Err("Unsupported workflow version or more than 100 app profiles.".into());
        }
        if self.max_entries > 10_000_000 || self.max_bytes > 1024 * 1024 * 1024 * 1024 {
            return Err("Choose at most 10 million records / 1 TiB, or 0 for unlimited.".into());
        }
        let mut seen = std::collections::HashSet::new();
        for profile in &self.profiles {
            let id = profile.app_id.trim();
            if id != profile.app_id
                || id.is_empty()
                || id.len() > 200
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '))
                || !seen.insert(id.to_ascii_lowercase())
            {
                return Err("App identities must be unique executable filenames or bundle IDs, not paths or window titles.".into());
            }
        }
        if self.diagnostics && !cfg!(any(windows, target_os = "macos")) {
            return Err("Encrypted diagnostics are unavailable on this platform.".into());
        }
        Ok(())
    }
    pub(crate) fn resolve(
        &self,
        app: &str,
        progressive: bool,
        paste: bool,
    ) -> (Cleanup, bool, bool) {
        self.profiles
            .iter()
            .find(|p| p.app_id.eq_ignore_ascii_case(app))
            .map(|p| {
                (
                    p.cleanup,
                    p.progressive.unwrap_or(progressive),
                    p.paste.unwrap_or(paste),
                )
            })
            .unwrap_or((self.cleanup, progressive, paste))
    }
}
fn path(base: &Path) -> Result<PathBuf, String> {
    crate::paths::ensure_trusted_data_subdir(base, Path::new("personalization"))
        .map(|p| p.join("workflows.json"))
        .map_err(|e| e.to_string())
}
pub(crate) fn load(base: &Path) -> Result<(String, Preferences), String> {
    let target = path(base)?;
    let _guard = crate::lock_rules_writes(&target)?;
    load_locked(base)
}
fn load_locked(base: &Path) -> Result<(String, Preferences), String> {
    let bytes = match crate::read_bounded_bytes(&path(base)?, 128 * 1024) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return migrate_legacy_with(base, crate::diagnostics::check_encryption)
        }
        Err(e) => return Err(e.to_string()),
    };
    let prefs: Preferences = serde_json::from_slice(&bytes)
        .map_err(|_| "Unreadable workflow settings; the file was left untouched.")?;
    prefs.validate()?;
    Ok((format!("{:x}", Sha256::digest(&bytes)), prefs))
}
// Called with the workflow lock held. An explicit new preference always wins;
// the legacy file is read-only, and publication never overwrites another writer.
fn migrate_legacy_with(
    base: &Path,
    check: impl FnOnce() -> Result<(), String>,
) -> Result<(String, Preferences), String> {
    #[derive(Deserialize, Default)]
    struct Legacy {
        local_diagnostic_history: Option<bool>,
    }
    let source = match crate::read_bounded_string(
        &base.join("vocalcode.toml"),
        crate::MAX_CONFIG_DOCUMENT_BYTES,
    ) {
        Ok(source) => source,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(("missing".into(), Preferences::default()));
        }
        Err(_) => {
            return Err("Cannot read legacy diagnostic preferences; nothing was changed.".into())
        }
    };
    crate::reject_future_config_version(&base.join("vocalcode.toml"), &source)?;
    let legacy: Legacy = toml::from_str(&source)
        .map_err(|_| "Invalid legacy diagnostic preferences; nothing was changed.")?;
    let Some(enabled) = legacy.local_diagnostic_history else {
        return Ok(("missing".into(), Preferences::default()));
    };
    let prefs = Preferences {
        diagnostics: enabled,
        ..Preferences::default()
    };
    prefs.validate()?;
    if enabled {
        check()?;
    }
    let bytes = serde_json::to_vec_pretty(&prefs).map_err(|e| e.to_string())?;
    match crate::storage::atomic_write_new(&path(base)?, &bytes) {
        Ok(()) => Ok((format!("{:x}", Sha256::digest(&bytes)), prefs)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => load_locked(base),
        Err(_) => {
            Err("Could not migrate diagnostic preferences; legacy settings were preserved.".into())
        }
    }
}
/// Refusal for a save made against a revision another writer has replaced.
pub(crate) const REVISION_CONFLICT: &str = "Workflow settings were changed elsewhere.";

pub(crate) fn handle(
    base: &Path,
    status: &crate::webui::RuntimeStatus,
    request: &Value,
    output: Option<PathBuf>,
) -> Result<Value, String> {
    match request["op"].as_str().unwrap_or("") {
        "rewrite_models" | "rewrite_providers" | "rewrite_preview" => {
            let mut request = request.clone();
            request["op"] = json!(if request["op"] == "rewrite_models" {
                "models"
            } else if request["op"] == "rewrite_providers" {
                "providers"
            } else {
                "preview"
            });
            crate::rewrite::handle(base, status, &request)
        }
        "load" | "save" => {
            let path = path(base)?;
            let _guard = crate::lock_rules_writes(&path)?;
            let (mut revision, mut prefs) = load_locked(base)?;
            if request["op"] == "save" {
                if request["revision"].as_str() != Some(&revision) {
                    // The page saves each change as it is made and reloads by
                    // itself when this is refused; it translates this text.
                    return Err(REVISION_CONFLICT.into());
                }
                prefs = serde_json::from_value(request["preferences"].clone())
                    .map_err(|_| "Invalid workflow preferences.")?;
                prefs.validate()?;
                // Fail before enabling if account-scoped secure storage is unavailable.
                if prefs.diagnostics {
                    crate::diagnostics::check_encryption()?;
                }
                let bytes = serde_json::to_vec_pretty(&prefs).map_err(|e| e.to_string())?;
                crate::storage::atomic_write(&path, &bytes).map_err(|e| e.to_string())?;
                revision = format!("{:x}", Sha256::digest(&bytes));
            }
            *status.workflows.lock().unwrap_or_else(|p| p.into_inner()) = prefs.clone();
            let storage =
                crate::diagnostics::inventory(base).unwrap_or_else(|error| json!({"error":error}));
            Ok(
                json!({"preferences":prefs, "revision":revision, "saved":request["op"]=="save", "diagnostic_storage":storage}),
            )
        }
        "history" => {
            crate::diagnostics::page(base, request["offset"].as_u64().unwrap_or(0) as usize)
        }
        "export_history" => {
            let destination = output.ok_or("No export destination selected.")?;
            let destination = crate::migration::export_destination(base, &destination)?;
            Ok(json!({"exported":crate::diagnostics::export(base, &destination)?}))
        }
        _ => Err("Unknown workflow operation.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scratch() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "vocalcode-workflow-migration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&base).unwrap();
        base
    }
    #[test]
    fn legacy_opt_in_migrates_once_without_modifying_legacy_or_limits() {
        let base = scratch();
        let old = base.join("vocalcode.toml");
        let source = "local_diagnostic_history = true\nmodel = 'sensevoice'\n";
        std::fs::write(&old, source).unwrap();
        let migrated = migrate_legacy_with(&base, || Ok(())).unwrap();
        assert!(migrated.1.diagnostics);
        assert_eq!(migrated.1.max_entries, 100_000);
        assert_eq!(std::fs::read_to_string(&old).unwrap(), source);
        assert_eq!(load(&base).unwrap().0, migrated.0);
        // An explicit new opt-out wins even while the old flag remains true.
        let mut prefs = migrated.1;
        prefs.diagnostics = false;
        prefs.max_entries = 0;
        prefs.max_bytes = 0;
        std::fs::write(path(&base).unwrap(), serde_json::to_vec(&prefs).unwrap()).unwrap();
        let current = load(&base).unwrap().1;
        assert!(!current.diagnostics);
        assert_eq!(current.max_entries, 0);
        assert_eq!(current.max_bytes, 0);
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn legacy_false_missing_and_invalid_never_enable_or_overwrite_preferences() {
        for source in [
            "",
            "local_diagnostic_history = false",
            "local_diagnostic_history = 'true'",
            "not valid toml",
            "config_version = 999\nlocal_diagnostic_history = true",
        ] {
            let base = scratch();
            std::fs::write(base.join("vocalcode.toml"), source).unwrap();
            let result = migrate_legacy_with(&base, || panic!("no encryption attempt"));
            if source.is_empty() || source.ends_with("false") {
                assert!(!result.unwrap().1.diagnostics);
            } else {
                assert!(result.is_err());
                assert!(!path(&base).unwrap().exists());
            }
            assert_eq!(
                std::fs::read_to_string(base.join("vocalcode.toml")).unwrap(),
                source
            );
            std::fs::remove_dir_all(base).unwrap();
        }
        let base = scratch();
        assert!(!load(&base).unwrap().1.diagnostics);
        assert!(!path(&base).unwrap().exists());
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn migration_failure_and_concurrent_explicit_choice_are_preserved() {
        let base = scratch();
        std::fs::write(
            base.join("vocalcode.toml"),
            "local_diagnostic_history = true",
        )
        .unwrap();
        assert!(migrate_legacy_with(&base, || Err("encryption unavailable".into())).is_err());
        assert!(!path(&base).unwrap().exists());
        let result = migrate_legacy_with(&base, || {
            std::fs::write(
                path(&base).unwrap(),
                br#"{"diagnostics":false,"max_entries":17}"#,
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
        assert!(!result.1.diagnostics);
        assert_eq!(result.1.max_entries, 17);
        std::fs::write(path(&base).unwrap(), b"invalid").unwrap();
        assert!(load(&base).is_err());
        assert_eq!(std::fs::read(path(&base).unwrap()).unwrap(), b"invalid");
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn chinese_opt_in_is_independent_and_inherits_only_its_own_app_override() {
        let old = r#"{"remove_fillers":true,"profiles":[{"app_id":"Code.exe","cleanup":"light","progressive":null,"paste":null,"remove_fillers":true}]}"#;
        let mut p: Preferences = serde_json::from_str(old).unwrap();
        assert!(p.fillers_for("Code.exe", "en"));
        assert!(!p.fillers_for("Code.exe", "zh"));
        assert!(!p.chinese_fillers);
        assert_eq!(p.profiles[0].chinese_fillers, None);
        p.chinese_fillers = true;
        p.remove_fillers = false;
        assert!(p.fillers_for("Code.exe", "zh-CN"));
        assert!(!p.fillers_for("Other.exe", "en"));
        p.profiles[0].chinese_fillers = Some(false);
        assert!(!p.fillers_for("CODE.EXE", "zh"));
        assert!(p.fillers_for("Other.exe", "zh"));
        let round: Preferences = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert!(round.chinese_fillers);
        assert_eq!(round.profiles[0].chinese_fillers, Some(false));
        assert!(serde_json::from_str::<Preferences>(r#"{"chinese_fillers":"on"}"#).is_err());
    }
    #[test]
    fn old_preferences_keep_fillers_and_app_overrides_are_independent() {
        let old = r#"{"schema":1,"cleanup":"light","profiles":[{"app_id":"Code.exe","cleanup":"original","progressive":null,"paste":null}]}"#;
        let mut p: Preferences = serde_json::from_str(old).unwrap();
        assert!(!p.remove_fillers);
        assert!(!p.fillers_for("Code.exe", "en"));
        p.remove_fillers = true;
        assert!(p.fillers_for("Code.exe", "en"));
        p.profiles[0].remove_fillers = Some(false);
        assert!(!p.fillers_for("CODE.exe", "en"));
        assert!(p.fillers_for("OtherCode.exe", "en"));
        assert_eq!(p.resolve("Code.exe", false, false).0, Cleanup::Original);
        let round: Preferences = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert!(round.remove_fillers);
        assert!(!round.fillers_for("Code.exe", "en"));
        assert!(serde_json::from_str::<Preferences>(r#"{"remove_fillers":"yes"}"#).is_err());
    }
    #[test]
    fn preferences_are_conservative_and_app_matching_is_exact() {
        let mut p = Preferences::default();
        assert!(!p.diagnostics);
        p.profiles.push(Profile {
            app_id: "Code.exe".into(),
            cleanup: Cleanup::Original,
            progressive: Some(false),
            paste: None,
            remove_fillers: None,
            chinese_fillers: None,
        });
        p.validate().unwrap();
        assert_eq!(
            p.resolve("code.EXE", true, true),
            (Cleanup::Original, false, true)
        );
        assert_eq!(
            p.resolve("OtherCode.exe", true, false),
            (Cleanup::Light, true, false)
        );
        p.profiles.push(p.profiles[0].clone());
        assert!(p.validate().is_err());
    }
    #[test]
    fn corrupt_or_future_preferences_are_not_accepted() {
        let p: Preferences = serde_json::from_str(r#"{"schema":999}"#).unwrap();
        assert!(p.validate().is_err());
        assert!(serde_json::from_str::<Preferences>(r#"{"api_key":"not-a-setting"}"#).is_err());
    }

    #[test]
    fn preference_save_is_revision_bound_and_applies_only_after_persistence() {
        let base = std::env::temp_dir().join(format!(
            "vocalcode-workflow-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&base).unwrap();
        let status = crate::webui::RuntimeStatus::default();
        let before = handle(&base, &status, &json!({"op":"load"}), None).unwrap();
        let mut prefs = before["preferences"].clone();
        prefs["cleanup"] = json!("original");
        prefs["remove_fillers"] = json!(true);
        handle(
            &base,
            &status,
            &json!({"op":"save","revision":before["revision"],"preferences":prefs}),
            None,
        )
        .unwrap();
        assert_eq!(status.workflows.lock().unwrap().cleanup, Cleanup::Original);
        assert_eq!(load(&base).unwrap().1.cleanup, Cleanup::Original);
        assert!(load(&base).unwrap().1.remove_fillers);
        let bad =
            json!({"op":"save","revision":before["revision"],"preferences":Preferences::default()});
        assert_eq!(
            handle(&base, &status, &bad, None).unwrap_err(),
            REVISION_CONFLICT
        );
        assert_eq!(load(&base).unwrap().1.cleanup, Cleanup::Original);
        std::fs::remove_dir_all(base).unwrap();
    }
}
