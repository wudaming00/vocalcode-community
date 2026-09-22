//! Isolated, non-activating reminder surface. Its IPC cannot start a recording,
//! save settings, touch the clipboard or perform any general settings operation.
use crate::meeting_reminder::{
    Action, ActionResult, MeetingCandidate, Prompt, ReminderGate, Strength,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use tao::{
    event_loop::EventLoopWindowTarget,
    window::{Window, WindowBuilder},
};
use vocalcode_meeting::auto_end::{Action as EndAction, Notice as EndNotice};

const HTML: &str = include_str!("meeting_prompt.html");
fn document_url() -> String {
    // WebView2's NavigateToString includes the charset in NavigationStarting.
    format!(
        "data:text/html;charset=utf-8;base64,{}",
        STANDARD.encode(HTML)
    )
}
fn allowed_navigation(url: &str, document: &str) -> bool {
    url == "about:blank" || url == document
}
#[derive(Debug, Clone, Copy)]
pub enum Event {
    Ready,
    Action(u64, Action),
    AutoEnd(u64, EndAction),
}

pub fn parse_event(body: &str) -> Option<Event> {
    if body.len() > 256 {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    if value["type"] == "ready" {
        return Some(Event::Ready);
    }
    if value["type"] != "reminder_action" && value["type"] != "auto_end_action" {
        return None;
    }
    let id = value["id"]
        .as_u64()
        .filter(|id| *id > 0 && *id < (1 << 53))?;
    if value["type"] == "auto_end_action" {
        return Some(Event::AutoEnd(
            id,
            match value["action"].as_str()? {
                "visible" => EndAction::Visible,
                "continue" => EndAction::Continue,
                "disable" => EndAction::Disable,
                "stop" => EndAction::Stop,
                _ => return None,
            },
        ));
    }
    let action = match value["action"].as_str()? {
        "review" => Action::Review,
        "dismiss" => Action::Dismiss,
        "snooze" => Action::Snooze,
        _ => return None,
    };
    Some(Event::Action(id, action))
}

/// Called only for a user's explicit Review action, after hiding the popup.
/// This shared boundary is also exercised by the native UI integration test.
pub fn open_review(
    window: &Window,
    webview: &wry::WebView,
    candidate: Option<MeetingCandidate>,
) -> wry::Result<()> {
    let payload = serde_json::json!({"suggested_title": candidate.map(|c| c.suggested_title).unwrap_or_default()});
    webview.evaluate_script(&format!("window.vocalcodeOpenMeetingReview({payload});"))?;
    window.set_visible(true);
    window.set_minimized(false);
    #[cfg(windows)]
    {
        use tao::platform::windows::WindowExtWindows;
        if unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(window.hwnd() as _)
        } == 0
        {
            window.request_user_attention(Some(tao::window::UserAttentionType::Informational));
        }
    }
    window.set_focus();
    Ok(())
}

pub struct MeetingPrompt {
    webview: wry::WebView,
    window: Window,
    _context: wry::WebContext,
    ready: bool,
    shown: Option<u64>,
    auto_end: bool,
}
impl MeetingPrompt {
    pub fn new<T: 'static>(
        target: &EventLoopWindowTarget<T>,
        directory: std::path::PathBuf,
        on_event: impl Fn(Event) + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        let builder = WindowBuilder::new()
            .with_title("VocalCode Meeting Reminder")
            .with_decorations(false)
            .with_focused(false)
            .with_visible(false)
            .with_always_on_top(true)
            .with_resizable(false)
            .with_inner_size(tao::dpi::LogicalSize::new(392., 300.));
        #[cfg(windows)]
        let builder = {
            use tao::platform::windows::WindowBuilderExtWindows;
            builder
                .with_skip_taskbar(true)
                .with_undecorated_shadow(false)
        };
        let window = builder.build(target)?;
        let mut context = wry::WebContext::new(Some(directory));
        let document = document_url();
        let webview = wry::WebViewBuilder::new_with_web_context(&mut context)
            .with_html(HTML)
            .with_background_color((14, 15, 17, 255))
            .with_navigation_handler(move |url| allowed_navigation(&url, &document))
            .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
            .with_ipc_handler(move |request| {
                if let Some(event) = parse_event(request.body()) {
                    on_event(event);
                }
            })
            .build(&window)?;
        Ok(Self {
            webview,
            window,
            _context: context,
            ready: false,
            shown: None,
            auto_end: false,
        })
    }
    pub fn window_id(&self) -> tao::window::WindowId {
        self.window.id()
    }
    pub fn apply_action(
        &mut self,
        gate: &mut ReminderGate,
        id: u64,
        action: Action,
        now_ms: u64,
    ) -> ActionResult {
        if self.auto_end {
            return ActionResult::Ignored;
        }
        let result = gate.visible_action(self.shown, id, action, now_ms);
        if result != ActionResult::Ignored {
            // Hide before raising the main window: hiding the active popup
            // afterwards can hand foreground ownership back to another app.
            self.hide();
        }
        result
    }
    pub fn dismiss(&mut self, gate: &mut ReminderGate, now_ms: u64) {
        if let Some(id) = self.shown {
            self.apply_action(gate, id, Action::Dismiss, now_ms);
        } else {
            self.hide();
        }
    }
    pub fn ready(&mut self) {
        // A reloaded document must not orphan a previously visible native HWND.
        self.hide();
        self.ready = true;
    }
    pub fn sync(&mut self, prompt: Option<&Prompt>, language: &str) {
        if self.auto_end {
            self.hide();
        }
        let Some(prompt) = prompt.filter(|_| self.ready) else {
            self.hide();
            return;
        };
        if self.shown == Some(prompt.id) {
            return;
        }
        let payload = serde_json::json!({"id":prompt.id,"app":prompt.candidate.app_name,"title":prompt.candidate.suggested_title,
            "kind":if prompt.candidate.strength == Strength::Confirmed {"confirmed"} else if prompt.candidate.strength == Strength::Calendar {"calendar"} else {"possible"},"language":language});
        if self
            .webview
            .evaluate_script(&format!("window.showReminder({payload});"))
            .is_err()
        {
            return;
        }
        self.show_without_activation();
        self.shown = Some(prompt.id);
    }

    pub fn sync_auto_end(&mut self, notice: &EndNotice, language: &str) {
        if !self.ready {
            return;
        }
        let same = self.auto_end && self.shown == Some(notice.id);
        if !same {
            self.hide();
        }
        let payload = serde_json::json!({"id": notice.id, "minutes": notice.idle_minutes,
            "seconds": notice.remaining_seconds, "language": language});
        if self
            .webview
            .evaluate_script(&format!("window.showAutoEnd({payload});"))
            .is_err()
        {
            return;
        }
        if !same {
            self.show_without_activation();
        }
        self.auto_end = true;
        self.shown = Some(notice.id);
    }

    pub fn accepts_auto_end(&self, id: u64) -> bool {
        self.auto_end && self.shown == Some(id)
    }

    pub fn auto_end_id(&self) -> Option<u64> {
        self.shown.filter(|_| self.auto_end)
    }
    pub fn hide(&mut self) {
        self.auto_end = false;
        #[cfg(windows)]
        {
            use tao::platform::windows::WindowExtWindows;
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                IsWindowVisible, ShowWindow, SW_HIDE,
            };
            // Showing uses native SW_SHOWNOACTIVATE, bypassing Tao's cached
            // VISIBLE flag. Tao's set_visible(false) is therefore a no-op.
            // Use the same native layer for hiding, even after shown was reset.
            // Avoid repeatedly calling SW_HIDE on an already hidden window.
            let hwnd = self.window.hwnd() as _;
            unsafe {
                if IsWindowVisible(hwnd) != 0 {
                    ShowWindow(hwnd, SW_HIDE);
                }
            }
        }
        #[cfg(not(windows))]
        self.window.set_visible(false);
        if self.shown.take().is_some() {
            let _ = self.webview.evaluate_script("window.hideReminder()");
        }
    }
    /// UI-only smoke inspection, excluded from shipped release builds.
    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub fn inspect_layout(&self, report: impl Fn(String) + Send + 'static) {
        let _ = self.webview.evaluate_script_with_callback(
            r#"(() => {
                const main = document.querySelector('main');
                const visible = [...document.querySelectorAll('button,h1,#privacy,#context')];
                return {language: document.documentElement.lang, width: innerWidth, height: innerHeight,
                    overflow: main.scrollHeight > main.clientHeight + 2 || visible.some(e => {
                        const r = e.getBoundingClientRect();
                        return r.left < 0 || r.top < 0 || r.right > innerWidth + 1 || r.bottom > innerHeight + 1 || e.scrollWidth > e.clientWidth + 1;
                    })};
            })()"#,
            report,
        );
    }
    /// Read the OS state, not Tao's cached visibility, in the native smoke test.
    #[cfg(all(debug_assertions, windows))]
    #[allow(dead_code)]
    pub fn inspect_native_visibility(&self) -> bool {
        use tao::platform::windows::WindowExtWindows;
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(self.window.hwnd() as _)
                != 0
        }
    }
    #[cfg(all(debug_assertions, windows))]
    #[allow(dead_code)]
    pub fn inspect_native_foreground(&self) -> bool {
        use tao::platform::windows::WindowExtWindows;
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow()
                == self.window.hwnd() as _
        }
    }
    /// Exercise the embedded page's real handlers with synthetic reminder data.
    /// This is deliberately absent from shipped builds and the IPC allowlist.
    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub fn exercise_smoke_action(&self, action: &str) -> anyhow::Result<()> {
        let script = match action {
            "close" => "document.getElementById('close').click()",
            "dismiss" => "document.getElementById('dismiss').click()",
            "snooze" => "document.getElementById('snooze').click()",
            "review" => "document.getElementById('review').click()",
            "escape" => "document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape'}))",
            // Start the real production timeout even if the pointer happens to
            // be over this synthetic card. Do not shorten or replace the timer.
            "timeout" => "document.dispatchEvent(new Event('mouseleave'))",
            _ => anyhow::bail!("unknown reminder smoke action"),
        };
        self.webview.evaluate_script(script)?;
        Ok(())
    }
    fn show_without_activation(&self) {
        #[cfg(windows)]
        {
            use tao::platform::windows::WindowExtWindows;
            use windows_sys::Win32::{
                Graphics::Gdi::{
                    CreateRoundRectRgn, DeleteObject, GetMonitorInfoW, MonitorFromWindow,
                    SetWindowRgn, MONITORINFO, MONITOR_DEFAULTTONEAREST,
                },
                UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
            };
            let hwnd = self.window.hwnd() as _;
            let foreground = unsafe { GetForegroundWindow() };
            let scale = (unsafe { GetDpiForWindow(foreground) } as f64 / 96.).max(1.);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if unsafe {
                GetMonitorInfoW(
                    MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST),
                    &mut info,
                )
            } != 0
            {
                let margin = (16. * scale) as i32;
                let width = ((392. * scale) as i32)
                    .min((info.rcWork.right - info.rcWork.left - 2 * margin).max(240));
                let height = ((300. * scale) as i32)
                    .min((info.rcWork.bottom - info.rcWork.top - 2 * margin).max(180));
                unsafe {
                    SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        info.rcWork.right - width - margin,
                        info.rcWork.bottom - height - margin,
                        width,
                        height,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                    let region = CreateRoundRectRgn(
                        0,
                        0,
                        width + 1,
                        height + 1,
                        (24. * scale) as i32,
                        (24. * scale) as i32,
                    );
                    if !region.is_null() && SetWindowRgn(hwnd, region, 1) == 0 {
                        DeleteObject(region as _);
                    }
                    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
            } else {
                unsafe {
                    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
            }
        }
        #[cfg(not(windows))]
        {
            if let Some(monitor) = self
                .window
                .current_monitor()
                .or_else(|| self.window.primary_monitor())
            {
                let scale = monitor.scale_factor();
                let origin = monitor.position().to_logical::<f64>(scale);
                let size = monitor.size().to_logical::<f64>(scale);
                self.window
                    .set_outer_position(tao::dpi::LogicalPosition::new(
                        origin.x + (size.width - 408.).max(0.),
                        origin.y + (size.height - 340.).max(0.),
                    ));
            }
            self.window.set_visible(true);
            // No set_focus: the meeting window keeps keyboard ownership.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_document_navigation_matches_webview2_without_allowing_arbitrary_data() {
        let document = document_url();
        assert!(document.starts_with("data:text/html;charset=utf-8;base64,"));
        assert!(allowed_navigation(&document, &document));
        assert!(allowed_navigation("about:blank", &document));
        for url in [
            "https://example.com",
            "javascript:alert(1)",
            "data:text/html,other",
        ] {
            assert!(!allowed_navigation(url, &document));
        }
        assert!(!allowed_navigation(
            &(document.clone() + "#other"),
            &document
        ));
    }
    #[test]
    fn ipc_is_an_allowlist_with_bound_tokens_and_no_recording_authority() {
        assert!(matches!(
            parse_event(r#"{"type":"reminder_action","id":1,"action":"review"}"#),
            Some(Event::Action(1, Action::Review))
        ));
        for input in [
            r#"{"type":"meeting_start"}"#,
            r#"{"type":"save"}"#,
            r#"{"type":"reminder_action","id":0,"action":"review"}"#,
            r#"{"type":"reminder_action","id":1,"action":"start"}"#,
        ] {
            assert!(parse_event(input).is_none());
        }
        assert!(parse_event(&" ".repeat(257)).is_none());
    }

    #[test]
    fn auto_end_ipc_cannot_start_capture_or_use_invalid_tokens() {
        assert!(matches!(
            parse_event(r#"{"type":"auto_end_action","id":9,"action":"continue"}"#),
            Some(Event::AutoEnd(9, EndAction::Continue))
        ));
        assert!(matches!(
            parse_event(r#"{"type":"auto_end_action","id":9,"action":"visible"}"#),
            Some(Event::AutoEnd(9, EndAction::Visible))
        ));
        for body in [
            r#"{"type":"auto_end_action","id":9,"action":"start"}"#,
            r#"{"type":"auto_end_action","id":0,"action":"stop"}"#,
            r#"{"type":"auto_end_action","id":9007199254740992,"action":"stop"}"#,
            r#"{"type":"reminder_action","id":9,"action":"stop"}"#,
        ] {
            assert!(parse_event(body).is_none());
        }
    }
}
