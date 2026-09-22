//! Compile-time boundary between the existing paid distribution and a local,
//! activation-free community edition. Never infer the edition from user data,
//! an environment variable at runtime, or a WebView message.

pub const ENABLED: bool = cfg!(feature = "community");
pub const DATA_DIR_NAME: &str = if ENABLED {
    "VocalCode Community"
} else {
    "VocalCode"
};
#[cfg(any(test, windows))]
pub const AUTOSTART_NAME: &str = if ENABLED {
    "VocalCodeCommunity"
} else {
    "VocalCode"
};
#[cfg(any(test, target_os = "macos"))]
pub const BUNDLE_ID: &str = if ENABLED {
    "app.vocalcode.Community"
} else {
    "app.vocalcode.VocalCode"
};
#[cfg(any(test, target_os = "macos"))]
pub const BUNDLE_NAME: &str = if ENABLED {
    "VocalCode Community.app"
} else {
    "VocalCode.app"
};
#[cfg(any(test, target_os = "macos"))]
pub const UPDATE_STEM: &str = if ENABLED {
    ".VocalCodeCommunity-update"
} else {
    ".VocalCode-update"
};
pub const DATA_LOCK_NAME: &str = if ENABLED {
    ".vocalcode-community-data-lifecycle.lock"
} else {
    ".vocalcode-data-lifecycle.lock"
};
pub const TRANSITION_LOCK_NAME: &str = if ENABLED {
    ".vocalcode-community-data-transition.lock"
} else {
    ".vocalcode-data-transition.lock"
};
#[cfg(windows)]
pub const INSTALLER_MUTEX: &str = if ENABLED {
    r"Local\VocalCode.Community.Desktop"
} else {
    r"Local\VocalCode.Desktop"
};
pub const LABEL: &str = "Community — all local features, no activation";
pub const ACTIVATION_NOTICE: &str = "Community edition needs no purchase or activation.";
pub const UPDATE_MANIFEST_URL: &str =
    "https://github.com/wudaming00/vocalcode-community/releases/latest/download/latest.json";

pub fn artifact_url(platform: &str, version: &str) -> Option<String> {
    crate::release_version(version)?;
    let file = match platform {
        "windows" => "VocalCodeCommunitySetup.exe".to_string(),
        "macos" => format!("VocalCodeCommunity-{version}.dmg"),
        _ => return None,
    };
    Some(format!(
        "https://github.com/wudaming00/vocalcode-community/releases/download/v{version}/{file}"
    ))
}

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
            url.path()
                .starts_with("/wudaming00/vocalcode-community/releases/")
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
    fn community_updater_is_available_but_cannot_offer_paid_installers() {
        for command in ["checkupdate", "update"] {
            assert!(blocked_ipc_for_edition(true, Some(command)).is_none());
        }
        assert_eq!(artifact_url("windows", "1.3.1").unwrap(), "https://github.com/wudaming00/vocalcode-community/releases/download/v1.3.1/VocalCodeCommunitySetup.exe");
        assert!(artifact_url("macos", "../evil").is_none());
        assert!(artifact_url("linux", "1.3.1").is_none());
    }

    #[test]
    fn community_identity_is_separate() {
        if ENABLED {
            assert_ne!(DATA_DIR_NAME, "VocalCode");
            assert_ne!(AUTOSTART_NAME, "VocalCode");
            assert_ne!(BUNDLE_ID, "app.vocalcode.VocalCode");
            assert_ne!(BUNDLE_NAME, "VocalCode.app");
            assert_ne!(UPDATE_STEM, ".VocalCode-update");
            assert_ne!(DATA_LOCK_NAME, ".vocalcode-data-lifecycle.lock");
            assert_ne!(TRANSITION_LOCK_NAME, ".vocalcode-data-transition.lock");
        }
    }

    #[test]
    fn github_transport_rejects_untrusted_redirects() {
        assert!(trusted_download_location(UPDATE_MANIFEST_URL));
        assert!(trusted_download_location("https://release-assets.githubusercontent.com/github-production-release-asset/1/2?signature=fixture"));
        for value in [
            "http://github.com/wudaming00/vocalcode-community/releases/",
            "https://github.com/other/project/releases/",
            "https://github.com.evil.example/wudaming00/vocalcode-community/releases/",
            "https://token@release-assets.githubusercontent.com/github-production-release-asset/1",
            "https://release-assets.githubusercontent.com:8080/github-production-release-asset/1",
            "https://release-assets.githubusercontent.com/other/path",
            "https://github.com/wudaming00/vocalcode-community/releases/#fragment",
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
