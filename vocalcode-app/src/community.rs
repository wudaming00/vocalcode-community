//! Compile-time boundary between the free, open-source build and the old
//! paid build this code base can still produce for tests. Never infer the
//! edition from user data, an environment variable at runtime, or a WebView
//! message.
//!
//! There is one product, VocalCode. It installs with the identity the paid
//! releases (up to 1.2.1) used: the same data folder, login item, bundle,
//! installer registration and single-running-copy name. That is what lets a
//! paid installation's own updater replace it in place with this free build,
//! keeping the person's settings, dictionary, meetings and models where they
//! are. The early free builds, published as "VocalCode Community" 1.3.1 and
//! 1.4.0, had an identity of their own; [`early`] names it so this build can
//! import from that folder and tell whether that app is still running.
//!
//! Only a packaging build carries that installed identity ([`release`]). Any
//! other build (`cargo build`, `cargo run`, QA and diagnostic builds) uses
//! [`development`]: its own data folder, login item and running-copy names,
//! so running it on a computer with VocalCode installed never reads or
//! rewrites that installation's settings. That matters beyond tidiness: once
//! this build saves settings, paid VocalCode 1.2.1 and earlier refuse to start
//! with that folder (its config version is newer than theirs).

pub const ENABLED: bool = cfg!(feature = "community");

/// True only in a binary built with `VOCALCODE_RELEASE_IDENTITY=1` in the
/// compiler's environment, which the packaging build sets (see BUILDING.md).
/// Decided at compile time, never from the environment at run time.
pub const RELEASE_IDENTITY: bool =
    release_identity_requested(option_env!("VOCALCODE_RELEASE_IDENTITY"));

const fn release_identity_requested(value: Option<&str>) -> bool {
    match value {
        Some(value) => matches!(value.as_bytes(), [b'1']),
        None => false,
    }
}

const fn pick(release: &'static str, development: &'static str) -> &'static str {
    if RELEASE_IDENTITY {
        release
    } else {
        development
    }
}

/// The installed app's identity, exactly as the paid releases (up to 1.2.1)
/// installed: their data folder, login item, bundle identifier, lifecycle
/// locks and the running-copy mutex their installer and updater wait on.
pub mod release {
    pub const DATA_DIR_NAME: &str = "VocalCode";
    #[cfg(any(test, windows))]
    pub const AUTOSTART_NAME: &str = "VocalCode";
    #[cfg(any(test, target_os = "macos"))]
    pub const BUNDLE_ID: &str = "app.vocalcode.VocalCode";
    pub const DATA_LOCK_NAME: &str = ".vocalcode-data-lifecycle.lock";
    pub const TRANSITION_LOCK_NAME: &str = ".vocalcode-data-transition.lock";
    #[cfg(windows)]
    pub const INSTALLER_MUTEX: &str = r"Local\VocalCode.Desktop";
}

/// Every build that is not a packaging build. Distinct in each name that
/// could reach an installed VocalCode: a different bundle identifier also
/// makes the macOS updater refuse to replace an installed app from here, and
/// the Windows installer refuses an update whose relaunch target is not the
/// installed VocalCode.exe.
pub mod development {
    pub const DATA_DIR_NAME: &str = "VocalCode Dev";
    #[cfg(any(test, windows))]
    pub const AUTOSTART_NAME: &str = "VocalCodeDev";
    #[cfg(any(test, target_os = "macos"))]
    pub const BUNDLE_ID: &str = "app.vocalcode.VocalCode.dev";
    pub const DATA_LOCK_NAME: &str = ".vocalcode-dev-data-lifecycle.lock";
    pub const TRANSITION_LOCK_NAME: &str = ".vocalcode-dev-data-transition.lock";
    #[cfg(windows)]
    pub const INSTALLER_MUTEX: &str = r"Local\VocalCode.Dev.Desktop";
}

pub const DATA_DIR_NAME: &str = pick(release::DATA_DIR_NAME, development::DATA_DIR_NAME);
#[cfg(any(test, windows))]
pub const AUTOSTART_NAME: &str = pick(release::AUTOSTART_NAME, development::AUTOSTART_NAME);
#[cfg(any(test, target_os = "macos"))]
pub const BUNDLE_ID: &str = pick(release::BUNDLE_ID, development::BUNDLE_ID);
/// The bundle a paid VocalCode's updater downloads and swaps in. A
/// development build is never that bundle, and its own bundle identifier
/// keeps its updater from replacing one.
#[cfg(any(test, target_os = "macos"))]
pub const BUNDLE_NAME: &str = "VocalCode.app";
#[cfg(any(test, target_os = "macos"))]
pub const UPDATE_STEM: &str = ".VocalCode-update";
pub const DATA_LOCK_NAME: &str = pick(release::DATA_LOCK_NAME, development::DATA_LOCK_NAME);
pub const TRANSITION_LOCK_NAME: &str = pick(
    release::TRANSITION_LOCK_NAME,
    development::TRANSITION_LOCK_NAME,
);
#[cfg(windows)]
pub const INSTALLER_MUTEX: &str = pick(release::INSTALLER_MUTEX, development::INSTALLER_MUTEX);
/// Shown where the paid build showed its plan.
pub const LABEL: &str = "Free and open source (AGPL-3.0)";
pub const ACTIVATION_NOTICE: &str = "VocalCode is free: there is nothing to buy or activate.";
pub const UPDATE_MANIFEST_URL: &str =
    "https://github.com/wudaming00/vocalcode-community/releases/latest/download/latest.json";

/// The early free builds, "VocalCode Community" 1.3.1 and 1.4.0, exactly as
/// they installed. Only for importing from that folder and for noticing that
/// app: nothing here is ever written, and its data folder is only read.
pub mod early {
    pub const DATA_DIR_NAME: &str = "VocalCode Community";
    #[cfg(any(test, windows))]
    pub const AUTOSTART_NAME: &str = "VocalCodeCommunity";
    #[cfg(any(test, windows))]
    pub const EXECUTABLE: &str = "VocalCodeCommunity.exe";
    #[cfg(any(test, windows))]
    pub const MUTEX: &str = r"Local\VocalCode.Community.Desktop";
    #[cfg(any(test, target_os = "macos"))]
    pub const BUNDLE_ID: &str = "app.vocalcode.Community";
}

pub fn artifact_url(platform: &str, version: &str) -> Option<String> {
    crate::release_version(version)?;
    let file = match platform {
        "windows" => "VocalCodeSetup.exe".to_string(),
        "macos" => format!("VocalCode-{version}.dmg"),
        _ => return None,
    };
    Some(format!(
        "https://github.com/wudaming00/vocalcode-community/releases/download/v{version}/{file}"
    ))
}

/// Where GitHub may send a release download. The repository may later be
/// renamed from `vocalcode-community` to `vocalcode`, and GitHub then
/// redirects the old path to the new one, so both are accepted here.
const RELEASE_PATHS: [&str; 2] = [
    "/wudaming00/vocalcode-community/releases/",
    "/wudaming00/vocalcode/releases/",
];

/// Redirects are a GitHub transport detail, not a new update authority.
/// Publisher signatures, exact hashes/sizes and product identity are still
/// verified before running any downloaded code.
pub fn trusted_download_location(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    match url.host_str() {
        Some("github.com") => {
            RELEASE_PATHS
                .iter()
                .any(|path| url.path().starts_with(path))
                && url.query().is_none()
        }
        Some("release-assets.githubusercontent.com") => {
            url.path().starts_with("/github-production-release-asset/")
        }
        _ => false,
    }
}

pub fn resolve_download_url(value: &str, cancelled: impl Fn() -> bool) -> Result<String, String> {
    let mut current = value.to_string();
    for _ in 0..4 {
        if cancelled() {
            return Err("update cancelled".into());
        }
        if !trusted_download_location(&current) {
            return Err("untrusted GitHub release redirect".into());
        }
        let response = ureq::head(&current)
            .config()
            .https_only(true)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(10)))
            .build()
            .call()
            .map_err(|_| "GitHub release request failed")?;
        if response.status().is_success() {
            return Ok(current);
        }
        if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Err(format!(
                "GitHub release HTTP {}",
                response.status().as_u16()
            ));
        }
        current = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or("GitHub release redirect omitted Location")?
            .to_string();
    }
    Err("too many GitHub release redirects".into())
}

pub fn blocked_ipc_reason(message_type: Option<&str>) -> Option<&'static str> {
    blocked_ipc_for_edition(ENABLED, message_type)
}

fn blocked_ipc_for_edition(community: bool, message_type: Option<&str>) -> Option<&'static str> {
    if !community {
        return None;
    }
    match message_type {
        Some("activate" | "buy" | "restore") => Some(ACTIVATION_NOTICE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn community_refuses_commerce_commands() {
        for command in ["activate", "buy", "restore"] {
            assert!(blocked_ipc_for_edition(true, Some(command)).is_some());
            assert!(blocked_ipc_for_edition(false, Some(command)).is_none());
            assert_eq!(blocked_ipc_reason(Some(command)).is_some(), ENABLED);
        }
    }

    #[test]
    fn shipped_page_has_no_commerce_ui_to_send_those_commands() {
        // The native refusal above is defence in depth. The page itself has
        // no plan card, licence key, checkout, restore or support-mail action.
        let page = include_str!("webui.html");
        for command in ["activate", "buy", "restore"] {
            assert!(!page.contains(&format!("type:\"{command}\"")), "{command}");
        }
        for element in [
            "data-panel=\"license\"",
            "id=\"planBadge\"",
            "data-pro-badge",
        ] {
            assert!(!page.contains(element), "{element}");
        }
        assert!(page.contains("<div class=\"panel\" data-panel=\"about\">"));
    }

    #[test]
    fn updater_offers_only_the_free_release_assets() {
        for command in ["checkupdate", "update"] {
            assert!(blocked_ipc_for_edition(true, Some(command)).is_none());
        }
        assert_eq!(
            artifact_url("windows", "1.4.1").unwrap(),
            "https://github.com/wudaming00/vocalcode-community/releases/download/v1.4.1/VocalCodeSetup.exe"
        );
        assert_eq!(
            artifact_url("macos", "1.4.1").unwrap(),
            "https://github.com/wudaming00/vocalcode-community/releases/download/v1.4.1/VocalCode-1.4.1.dmg"
        );
        assert!(artifact_url("macos", "../evil").is_none());
        assert!(artifact_url("linux", "1.3.1").is_none());
    }

    /// The identity the paid releases installed with, byte for byte. A paid
    /// installation's updater replaces it in place only if these agree:
    /// the same data folder, login item, bundle and running-copy name.
    #[test]
    fn identity_is_the_one_paid_installations_already_have() {
        assert_eq!(release::DATA_DIR_NAME, "VocalCode");
        assert_eq!(release::AUTOSTART_NAME, "VocalCode");
        assert_eq!(release::BUNDLE_ID, "app.vocalcode.VocalCode");
        assert_eq!(BUNDLE_NAME, "VocalCode.app");
        assert_eq!(UPDATE_STEM, ".VocalCode-update");
        assert_eq!(release::DATA_LOCK_NAME, ".vocalcode-data-lifecycle.lock");
        assert_eq!(
            release::TRANSITION_LOCK_NAME,
            ".vocalcode-data-transition.lock"
        );
        #[cfg(windows)]
        assert_eq!(release::INSTALLER_MUTEX, r"Local\VocalCode.Desktop");
    }

    /// Only `VOCALCODE_RELEASE_IDENTITY=1` at compile time selects the
    /// installed identity; anything else is a development build.
    #[test]
    fn only_a_packaging_build_has_the_installed_identity() {
        assert!(release_identity_requested(Some("1")));
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("1 "),
            Some("11"),
        ] {
            assert!(!release_identity_requested(value), "{value:?}");
        }
        let (data, autostart, bundle, lock, transition) = if RELEASE_IDENTITY {
            (
                release::DATA_DIR_NAME,
                release::AUTOSTART_NAME,
                release::BUNDLE_ID,
                release::DATA_LOCK_NAME,
                release::TRANSITION_LOCK_NAME,
            )
        } else {
            (
                development::DATA_DIR_NAME,
                development::AUTOSTART_NAME,
                development::BUNDLE_ID,
                development::DATA_LOCK_NAME,
                development::TRANSITION_LOCK_NAME,
            )
        };
        assert_eq!(DATA_DIR_NAME, data);
        assert_eq!(AUTOSTART_NAME, autostart);
        assert_eq!(BUNDLE_ID, bundle);
        assert_eq!(DATA_LOCK_NAME, lock);
        assert_eq!(TRANSITION_LOCK_NAME, transition);
        #[cfg(windows)]
        assert_eq!(
            INSTALLER_MUTEX,
            if RELEASE_IDENTITY {
                release::INSTALLER_MUTEX
            } else {
                development::INSTALLER_MUTEX
            }
        );
    }

    /// A development build shares no name with the installed app or with the
    /// early free builds, so it can never read, lock or relaunch either.
    #[test]
    fn a_development_build_shares_no_name_with_an_installation() {
        let pairs = [
            (development::DATA_DIR_NAME, release::DATA_DIR_NAME),
            (development::AUTOSTART_NAME, release::AUTOSTART_NAME),
            (development::BUNDLE_ID, release::BUNDLE_ID),
            (development::DATA_LOCK_NAME, release::DATA_LOCK_NAME),
            (
                development::TRANSITION_LOCK_NAME,
                release::TRANSITION_LOCK_NAME,
            ),
            (development::DATA_DIR_NAME, early::DATA_DIR_NAME),
            (development::AUTOSTART_NAME, early::AUTOSTART_NAME),
            (development::BUNDLE_ID, early::BUNDLE_ID),
        ];
        for (development, installed) in pairs {
            assert!(
                !development.eq_ignore_ascii_case(installed),
                "{development}"
            );
        }
        #[cfg(windows)]
        {
            assert_ne!(development::INSTALLER_MUTEX, release::INSTALLER_MUTEX);
            assert_ne!(development::INSTALLER_MUTEX, early::MUTEX);
        }
    }

    #[test]
    fn the_early_free_builds_keep_their_own_names() {
        assert_eq!(early::DATA_DIR_NAME, "VocalCode Community");
        assert_eq!(early::AUTOSTART_NAME, "VocalCodeCommunity");
        assert_eq!(early::EXECUTABLE, "VocalCodeCommunity.exe");
        assert_eq!(early::MUTEX, r"Local\VocalCode.Community.Desktop");
        assert_eq!(early::BUNDLE_ID, "app.vocalcode.Community");
        assert_ne!(early::DATA_DIR_NAME, release::DATA_DIR_NAME);
        assert_ne!(early::AUTOSTART_NAME, release::AUTOSTART_NAME);
        assert_ne!(early::BUNDLE_ID, release::BUNDLE_ID);
    }

    #[test]
    fn github_transport_rejects_untrusted_redirects() {
        assert!(trusted_download_location(UPDATE_MANIFEST_URL));
        assert!(trusted_download_location(
            &artifact_url("windows", "1.4.1").unwrap()
        ));
        // A later rename of the repository redirects here.
        assert!(trusted_download_location(
            "https://github.com/wudaming00/vocalcode/releases/download/v1.4.1/VocalCodeSetup.exe"
        ));
        assert!(trusted_download_location("https://release-assets.githubusercontent.com/github-production-release-asset/1/2?signature=fixture"));
        for value in [
            "http://github.com/wudaming00/vocalcode-community/releases/",
            "https://github.com/other/project/releases/",
            "https://github.com/wudaming00/vocalcode-other/releases/",
            "https://github.com/wudaming00/vocalcode/archive/refs/heads/main.zip",
            "https://github.com.evil.example/wudaming00/vocalcode-community/releases/",
            "https://token@release-assets.githubusercontent.com/github-production-release-asset/1",
            "https://release-assets.githubusercontent.com:8080/github-production-release-asset/1",
            "https://release-assets.githubusercontent.com/other/path",
            "https://github.com/wudaming00/vocalcode-community/releases/#fragment",
            "https://github.com/wudaming00/vocalcode/releases/?x=1",
        ] {
            assert!(!trusted_download_location(value), "{value}");
        }
    }

    #[test]
    fn local_features_and_safety_controls_remain_available() {
        for command in [
            "ready",
            "meeting_start",
            "meeting_import",
            "save_dict",
            "purge_data",
        ] {
            assert!(blocked_ipc_for_edition(true, Some(command)).is_none());
        }
        assert!(blocked_ipc_for_edition(true, None).is_none());
    }
}
