//! Read-only meeting hints. Never opens a capture stream, invokes a control,
//! reads transcript/editor text, or persists a window title / conference URL.
use crate::ForegroundApplication;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    GoogleMeet,
    Zoom,
    Teams,
    Slack,
    Webex,
}
impl Service {
    pub fn key(self) -> &'static str {
        match self {
            Self::GoogleMeet => "google-meet",
            Self::Zoom => "zoom",
            Self::Teams => "teams",
            Self::Slack => "slack",
            Self::Webex => "webex",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::GoogleMeet => "Google Meet",
            Self::Zoom => "Zoom",
            Self::Teams => "Microsoft Teams",
            Self::Slack => "Slack Huddle",
            Self::Webex => "Webex",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conference {
    pub service: Service,
    /// Ephemeral identifier only, never a full URL (no passwords/query strings).
    pub code: Option<String>,
}
pub fn conference_url(value: &str) -> Option<Conference> {
    if value.len() > 2048 {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str()?;
    let parts: Vec<_> = url.path_segments()?.take(8).collect();
    if host == "meet.google.com" {
        let code = parts.first().filter(|s| is_meet_code(s))?;
        return Some(Conference {
            service: Service::GoogleMeet,
            code: Some(code.to_ascii_lowercase()),
        });
    }
    if host == "zoom.us" || host.ends_with(".zoom.us") {
        let code = parts.windows(2).find_map(|s| {
            (matches!(s[0], "j" | "join")
                && (9..=11).contains(&s[1].len())
                && s[1].bytes().all(|b| b.is_ascii_digit()))
            .then(|| s[1].to_string())
        });
        // /wc/<id>/join is used by the browser client.
        let code = code.or_else(|| {
            parts.windows(2).find_map(|s| {
                (s[0] == "wc"
                    && (9..=11).contains(&s[1].len())
                    && s[1].bytes().all(|b| b.is_ascii_digit()))
                .then(|| s[1].to_string())
            })
        });
        return code.map(|code| Conference {
            service: Service::Zoom,
            code: Some(code),
        });
    }
    if matches!(
        host,
        "teams.microsoft.com" | "teams.live.com" | "teams.cloud.microsoft"
    ) {
        return Some(Conference {
            service: Service::Teams,
            code: None,
        });
    }
    if host.ends_with(".webex.com") {
        return Some(Conference {
            service: Service::Webex,
            code: None,
        });
    }
    None
}
fn is_meet_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    bytes.len() == 12
        && bytes[3] == b'-'
        && bytes[8] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 3 || i == 8 || b.is_ascii_alphabetic())
}
pub fn conference_title(value: &str) -> Option<Conference> {
    let title = value.to_lowercase().replace(['\u{2013}', '\u{2014}'], "-");
    let service = if title.starts_with("meet - ")
        || title.contains("google meet")
        || title.contains("meet.google.com/")
        || title.contains(" - meet - ")
        || title.ends_with(" - meet")
    {
        Service::GoogleMeet
    } else if title.contains("zoom meeting")
        || title.contains("zoom webinar")
        || title.contains("zoom 会议")
    {
        Service::Zoom
    } else if title.contains("teams")
        && ["meeting", "call", "会议", "通话"]
            .iter()
            .any(|s| title.contains(s))
    {
        Service::Teams
    } else if title.contains("webex")
        && ["meeting", "call", "会议", "通话"]
            .iter()
            .any(|s| title.contains(s))
    {
        Service::Webex
    } else {
        return None;
    };
    let code = if service == Service::GoogleMeet {
        title
            .split(|c: char| !c.is_ascii_alphabetic() && c != '-')
            .find(|s| is_meet_code(s))
            .map(str::to_string)
    } else {
        None
    };
    Some(Conference { service, code })
}
pub fn is_browser(app: &str) -> bool {
    matches!(
        app,
        "chrome.exe"
            | "msedge.exe"
            | "firefox.exe"
            | "brave.exe"
            | "vivaldi.exe"
            | "arc.exe"
            | "com.google.chrome"
            | "com.microsoft.edgemac"
            | "org.mozilla.firefox"
            | "com.brave.browser"
            | "company.thebrowser.browser"
    )
}
pub fn native_service(app: &str) -> Option<Service> {
    match app {
        "zoom.exe" | "us.zoom.xos" => Some(Service::Zoom),
        "teams.exe" | "ms-teams.exe" | "com.microsoft.teams" | "com.microsoft.teams2" => {
            Some(Service::Teams)
        }
        "slack.exe" | "com.tinyspeck.slackmacgap" => Some(Service::Slack),
        "webex.exe" | "ciscocollabhost.exe" | "webexmta.exe" => Some(Service::Webex),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CallState {
    #[default]
    Unknown,
    Lobby,
    InCall,
    Ended,
}
/// Only exact call-control labels, with an optional keyboard shortcut suffix.
/// Generic words such as "Leave", "Join", "Close", or "Mute" are not proof.
pub fn call_controls<'a>(names: impl IntoIterator<Item = &'a str>) -> CallState {
    let mut lobby = false;
    let mut ended = false;
    for name in names {
        let name = name.trim().to_lowercase();
        let name = name.split(['(', '（']).next().unwrap_or(&name).trim_end();
        if [
            "leave call",
            "leave meeting",
            "end call",
            "leave huddle",
            "离开通话",
            "离开会议",
            "退出会议",
            "离开此通话",
            "退出通话",
            "退出此次通话",
            "quitter l'appel",
            "quitter la réunion",
            "salir de la llamada",
            "salir de la reunión",
            "anruf verlassen",
            "besprechung verlassen",
            "通話から退出",
            "通話を終了",
            "会議から退出",
            "통화 나가기",
            "통화 종료",
            "회의 나가기",
        ]
        .contains(&name)
        {
            return CallState::InCall;
        }
        lobby |= [
            "join now",
            "ask to join",
            "ready to join?",
            "join meeting",
            "立即加入",
            "申请加入",
            "请求加入",
            "加入会议",
            "今すぐ参加",
            "参加をリクエスト",
            "지금 참여",
            "참여 요청",
        ]
        .contains(&name);
        ended |= [
            "rejoin",
            "rejoin meeting",
            "return to home screen",
            "重新加入",
            "重新加入会议",
            "返回主屏幕",
            "再参加",
            "다시 참여",
        ]
        .contains(&name);
    }
    if ended {
        CallState::Ended
    } else if lobby {
        CallState::Lobby
    } else {
        CallState::Unknown
    }
}

#[derive(Debug, Clone)]
pub struct MeetingWindow {
    pub application: ForegroundApplication,
    pub window_id: u64,
    pub foreground: bool,
    pub microphone_active: bool,
    pub conference: Option<Conference>,
    pub call_state: CallState,
}

/// Intended for a single isolated worker, never the UI / dictation threads.
pub fn observe() -> Vec<MeetingWindow> {
    #[cfg(windows)]
    {
        native::observe()
    }
    #[cfg(not(windows))]
    {
        crate::foreground_application()
            .map(|application| MeetingWindow {
                conference: conference_title(&application.window_title),
                application,
                window_id: 0,
                foreground: true,
                microphone_active: false,
                call_state: CallState::Unknown,
            })
            .into_iter()
            .collect()
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{
        collections::HashSet,
        time::{Duration, Instant},
    };
    use windows::{
        core::Interface,
        Win32::{
            Foundation::HWND,
            Media::Audio::*,
            System::{Com::*, Variant::VARIANT},
            UI::Accessibility::*,
        },
    };
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        },
        UI::WindowsAndMessaging::*,
    };

    fn app_name(pid: u32) -> Option<String> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let mut path = [0_u16; 1024];
        let mut count = path.len() as u32;
        let ok =
            unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut count) } != 0;
        unsafe {
            CloseHandle(handle);
        }
        if !ok || count as usize > path.len() {
            return None;
        }
        let path = String::from_utf16_lossy(&path[..count as usize]);
        path.rsplit(['\\', '/']).next().map(str::to_ascii_lowercase)
    }
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }

    /// Capture-session *metadata*, on all active capture endpoints (including
    /// non-default microphones). No IAudioClient / Initialize / Start calls.
    fn microphone_apps(deadline: Instant) -> HashSet<String> {
        let mut result = HashSet::new();
        let Ok(enumerator) = (unsafe {
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        }) else {
            return result;
        };
        let Ok(devices) = (unsafe { enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) })
        else {
            return result;
        };
        for index in 0..unsafe { devices.GetCount() }.unwrap_or(0).min(16) {
            if Instant::now() >= deadline {
                break;
            }
            let Ok(device) = (unsafe { devices.Item(index) }) else {
                continue;
            };
            let Ok(manager) =
                (unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) })
            else {
                continue;
            };
            let Ok(sessions) = (unsafe { manager.GetSessionEnumerator() }) else {
                continue;
            };
            for index in 0..unsafe { sessions.GetCount() }.unwrap_or(0).min(128) {
                if Instant::now() >= deadline {
                    break;
                }
                let Ok(session) = (unsafe { sessions.GetSession(index) }) else {
                    continue;
                };
                if unsafe { session.GetState() }.ok() != Some(AudioSessionStateActive) {
                    continue;
                }
                let Ok(session) = session.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let Ok(pid) = (unsafe { session.GetProcessId() }) else {
                    continue;
                };
                if pid == 0 || pid == std::process::id() {
                    continue;
                }
                if let Some(app) =
                    app_name(pid).filter(|app| is_browser(app) || native_service(app).is_some())
                {
                    result.insert(app);
                }
            }
        }
        result
    }
    struct WindowList {
        handles: Vec<usize>,
        visited: usize,
    }
    unsafe extern "system" fn collect(
        hwnd: windows_sys::Win32::Foundation::HWND,
        context: isize,
    ) -> i32 {
        // EnumWindows invokes this synchronously while `list` remains alive.
        let list = unsafe { &mut *(context as *mut WindowList) };
        list.visited += 1;
        if list.visited > 512 {
            return 0;
        }
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        if pid != 0 && pid != std::process::id() {
            list.handles.push(hwnd as usize);
        }
        1
    }
    pub(super) fn observe() -> Vec<MeetingWindow> {
        if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
            return Vec::new();
        }
        let _com = Com;
        let deadline = Instant::now() + Duration::from_millis(850);
        let microphones = microphone_apps(deadline);
        let mut list = WindowList {
            handles: Vec::new(),
            visited: 0,
        };
        let foreground = unsafe { GetForegroundWindow() } as usize;
        unsafe {
            EnumWindows(Some(collect), &mut list as *mut _ as isize);
        }
        list.handles.sort_by_key(|h| *h != foreground);
        let automation: Option<IUIAutomation> =
            unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }.ok();
        if let Some(automation) = &automation {
            if let Ok(timed) = automation.cast::<IUIAutomation2>() {
                unsafe {
                    let _ = timed.SetConnectionTimeout(250);
                    let _ = timed.SetTransactionTimeout(250);
                    let _ = timed.SetAutoSetFocus(false);
                }
            }
        }
        let mut results = Vec::new();
        let mut scans = 0;
        for handle in list.handles {
            if results.len() >= 16 {
                break;
            }
            let hwnd = handle as windows_sys::Win32::Foundation::HWND;
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, &mut pid);
            }
            let Some(app) = app_name(pid) else {
                continue;
            };
            let browser = is_browser(&app);
            let service = native_service(&app);
            if !browser && service.is_none() {
                continue;
            }
            let mut title = [0u16; 512];
            let count = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
            let title = String::from_utf16_lossy(&title[..count.max(0) as usize]);
            let mut conference = conference_title(&title);
            if !browser {
                conference = service.map(|service| Conference {
                    service,
                    code: None,
                });
            }
            let microphone_active = microphones.contains(&app);
            // Only inspect supported meeting windows, or a supported browser
            // actively holding a mic. Browser/editor page bodies are never read.
            if conference.is_none() && !microphone_active {
                continue;
            }
            let mut call_state = CallState::Unknown;
            let mut background_tabs = Vec::new();
            if scans < 4 && Instant::now() < deadline {
                if let Some(automation) = &automation {
                    scans += 1;
                    let (state, document, tabs) =
                        inspect_controls(automation, HWND(hwnd), browser, deadline);
                    call_state = state;
                    background_tabs = tabs;
                    if let Some(document) = document {
                        conference = Some(document);
                    }
                }
            }
            // A native app's home/settings screen is not a meeting just because
            // its executable is running. Unknown controls need title evidence.
            if !browser
                && call_state == CallState::Unknown
                && !microphone_active
                && !["meeting", "call", "huddle", "会议", "通话"]
                    .iter()
                    .any(|s| title.to_lowercase().contains(s))
            {
                continue;
            }
            let window = MeetingWindow {
                application: ForegroundApplication {
                    process_id: pid,
                    display_name: app.trim_end_matches(".exe").to_string(),
                    app_id: app,
                    window_title: title,
                },
                window_id: handle as u64,
                foreground: handle == foreground,
                microphone_active,
                conference,
                call_state,
            };
            // A background tab's label is weaker than its in-call UI. Keep it
            // observable without activating the tab, never treat it as proof.
            if browser && microphone_active {
                for tab in background_tabs.into_iter().take(4) {
                    if window.conference.as_ref() == Some(&tab) || tab.code.is_none() {
                        continue;
                    }
                    let mut background = window.clone();
                    background.conference = Some(tab);
                    background.call_state = CallState::Unknown;
                    background.foreground = false;
                    results.push(background);
                }
            }
            // Foreground/in-call observation takes precedence over weak tab hints.
            if results.len() >= 16 {
                results.truncate(15);
            }
            results.push(window);
        }
        results
    }
    fn inspect_controls(
        automation: &IUIAutomation,
        hwnd: HWND,
        browser: bool,
        deadline: Instant,
    ) -> (CallState, Option<Conference>, Vec<Conference>) {
        let result = (|| -> windows::core::Result<_> {
            unsafe {
                let root = automation.ElementFromHandle(hwnd)?;
                let button = automation.CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &VARIANT::from(UIA_ButtonControlTypeId.0),
                )?;
                let document = automation.CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &VARIANT::from(UIA_DocumentControlTypeId.0),
                )?;
                let tab = automation.CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &VARIANT::from(UIA_TabItemControlTypeId.0),
                )?;
                let condition = automation.CreateOrCondition(&button, &document)?;
                let condition = automation.CreateOrCondition(&condition, &tab)?;
                let cache = automation.CreateCacheRequest()?;
                cache.AddProperty(UIA_NamePropertyId)?;
                cache.AddProperty(UIA_ControlTypePropertyId)?;
                cache.AddProperty(UIA_IsOffscreenPropertyId)?;
                cache.SetTreeScope(TreeScope_Element)?;
                // Do not materialize every matching descendant and only then
                // truncate: a large browser/Slack tree could allocate far more
                // than our nominal limit. Walk a filtered tree incrementally.
                let walker = automation.CreateTreeWalker(&condition)?;
                let mut pending = Vec::new();
                if let Ok(first) = walker.GetFirstChildElementBuildCache(&root, &cache) {
                    pending.push((first, 0_u8));
                }
                let mut names = Vec::new();
                let mut conference = None;
                let mut tabs = Vec::new();
                for _ in 0..128 {
                    if Instant::now() >= deadline {
                        break;
                    }
                    let Some((element, depth)) = pending.pop() else {
                        break;
                    };
                    if let Ok(sibling) = walker.GetNextSiblingElementBuildCache(&element, &cache) {
                        pending.push((sibling, depth));
                    }
                    let kind = element.CachedControlType()?;
                    if depth < 12 && kind == UIA_DocumentControlTypeId {
                        if let Ok(child) = walker.GetFirstChildElementBuildCache(&element, &cache) {
                            pending.push((child, depth + 1));
                        }
                    }
                    if element
                        .CachedIsOffscreen()
                        .map(|v| v.as_bool())
                        .unwrap_or(true)
                    {
                        continue;
                    }
                    match kind {
                        kind if kind == UIA_ButtonControlTypeId => {
                            let name = element.CachedName()?;
                            if name.len() <= 160 {
                                names.push(name.to_string());
                            }
                        }
                        kind if kind == UIA_DocumentControlTypeId
                            && browser
                            && conference.is_none() =>
                        {
                            if let Ok(value) = element
                                .GetCurrentPatternAs::<IUIAutomationValuePattern>(
                                    UIA_ValuePatternId,
                                )
                                .and_then(|p| p.CurrentValue())
                            {
                                if value.len() <= 2048 {
                                    conference = conference_url(&value.to_string());
                                }
                            }
                        }
                        kind if kind == UIA_TabItemControlTypeId && browser && tabs.len() < 4 => {
                            let name = element.CachedName()?;
                            if name.len() <= 256 {
                                if let Some(tab) =
                                    conference_title(&name.to_string()).filter(|c| c.code.is_some())
                                {
                                    tabs.push(tab);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok((
                    call_controls(names.iter().map(String::as_str)),
                    conference,
                    tabs,
                ))
            }
        })();
        result.unwrap_or((CallState::Unknown, None, Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn urls_are_scoped_and_credentials_never_become_identity() {
        let a = conference_url("https://meet.google.com/abc-defg-hij?authuser=private").unwrap();
        assert_eq!(a.code.as_deref(), Some("abc-defg-hij"));
        assert_eq!(
            conference_url("https://us02web.zoom.us/j/12345678901?pwd=secret")
                .unwrap()
                .code
                .as_deref(),
            Some("12345678901")
        );
        assert!(conference_url("https://meet.google.com.attacker.test/abc-defg-hij").is_none());
        assert!(conference_url("https://meet.google.com@attacker.test/abc-defg-hij").is_none());
        assert!(conference_url("https://meet.google.com/").is_none());
        assert!(conference_url("http://meet.google.com/abc-defg-hij").is_none());
        assert!(conference_url("https://zoom.us/pricing").is_none());
    }
    #[test]
    fn only_unambiguous_controls_confirm_a_call() {
        assert_eq!(call_controls(["Leave", "Mute", "Join"]), CallState::Unknown);
        assert_eq!(
            call_controls(["Ask to join", "Mute microphone"]),
            CallState::Lobby
        );
        assert_eq!(
            call_controls(["Leave call (Ctrl+Shift+B)"]),
            CallState::InCall
        );
        assert_eq!(call_controls(["退出通话"]), CallState::InCall);
        assert_eq!(
            call_controls(["退出通话（Ctrl+Shift+B）"]),
            CallState::InCall
        );
        assert_eq!(
            call_controls(["通話を終了（Ctrl+Shift+B）"]),
            CallState::InCall
        );
        assert_eq!(
            call_controls(["회의 나가기 (Ctrl+Shift+B)"]),
            CallState::InCall
        );
        assert_eq!(call_controls(["今すぐ参加"]), CallState::Lobby);
        assert_eq!(call_controls(["다시 참여"]), CallState::Ended);
        assert_eq!(call_controls(["Rejoin"]), CallState::Ended);
        assert_eq!(
            call_controls(["Leave call documentation"]),
            CallState::Unknown
        );
    }
    #[test]
    fn allowlists_and_titles_do_not_match_unrelated_apps() {
        assert!(!is_browser("fake-chrome.exe"));
        assert!(native_service("zoom-helper-malware.exe").is_none());
        assert!(conference_title("Meet the team - Chrome").is_none());
        assert_eq!(
            conference_title("Meet - abc-defg-hij - Chrome")
                .unwrap()
                .code
                .as_deref(),
            Some("abc-defg-hij")
        );
    }
}
