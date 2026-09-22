//! Compile-time boundary between the existing paid distribution and a local,
//! activation-free community preview. Never infer the edition from user data,
//! an environment variable at runtime, or a WebView message.

pub const ENABLED: bool = cfg!(feature = "community");
pub const LABEL: &str = "Community preview — all local features, no activation";
pub const ACTIVATION_NOTICE: &str = "Community preview needs no purchase or activation.";
pub const UPDATE_NOTICE: &str =
    "Community preview uses manual updates; the paid release channel is disabled.";

pub fn blocked_ipc_reason(message_type: Option<&str>) -> Option<&'static str> {
    blocked_ipc_for_edition(ENABLED, message_type)
}

fn blocked_ipc_for_edition(community: bool, message_type: Option<&str>) -> Option<&'static str> {
    if !community {
        return None;
    }
    match message_type {
        Some("activate" | "buy" | "restore") => Some(ACTIVATION_NOTICE),
        Some("checkupdate" | "update") => Some(UPDATE_NOTICE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn community_refuses_commerce_and_paid_updater_commands() {
        for command in ["activate", "buy", "restore", "checkupdate", "update"] {
            assert!(blocked_ipc_for_edition(true, Some(command)).is_some());
            assert!(blocked_ipc_for_edition(false, Some(command)).is_none());
            assert_eq!(blocked_ipc_reason(Some(command)).is_some(), ENABLED);
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
