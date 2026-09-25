//! Opt-in desktop controls. No transcript, clipboard, model, or settings IPC is
//! exposed here. Native non-activation preserves the real text-field focus.
use crate::dictation_control::{Action, Bridge};
use crate::overlay::{Phase, ScreenRect, Snapshot};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Deserialize;
use std::time::{Duration, Instant};
use tao::event_loop::EventLoopWindowTarget;
use tao::platform::windows::{WindowBuilderExtWindows, WindowExtWindows};
use tao::window::{Window, WindowBuilder};
use windows_sys::Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*};

const HTML: &str = include_str!("control_bar.html");
const SUBCLASS_ID: usize = 0x5643_4241;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Start,
    Stop,
    Cancel,
    History,
    Meetings,
    Rewrite,
    Settings,
    Snooze,
    More,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Ready,
    Scale(u32),
    ReducedMotion(bool),
    Hover(bool),
    Command(u64, u64, Command),
}

pub struct Frame<'a> {
    pub enabled: bool,
    pub edge: &'a str,
    pub language: &'a str,
    pub snapshot: Snapshot,
    pub ready: bool,
    pub level: f32,
}

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum Message {
    #[serde(rename = "ready")]
    Ready {},
    #[serde(rename = "scale")]
    Scale { ratio_milli: u32 },
    #[serde(rename = "motion")]
    Motion { reduced: bool },
    #[serde(rename = "hover")]
    Hover { inside: bool },
    #[serde(rename = "action")]
    Action {
        revision: String,
        view_revision: String,
        action: String,
    },
}
pub fn parse_event(body: &str) -> Option<Event> {
    if body.len() > 256 {
        return None;
    }
    match serde_json::from_str::<Message>(body).ok()? {
        Message::Ready {} => Some(Event::Ready),
        Message::Scale { ratio_milli } => (500..=16000)
            .contains(&ratio_milli)
            .then_some(Event::Scale(ratio_milli)),
        Message::Hover { inside } => Some(Event::Hover(inside)),
        Message::Motion { reduced } => Some(Event::ReducedMotion(reduced)),
        Message::Action {
            revision,
            view_revision,
            action,
        } => {
            let parse_revision = |value: &str| -> Option<u64> {
                (!value.is_empty()
                    && value.len() <= 20
                    && value.bytes().all(|b| b.is_ascii_digit()))
                .then(|| value.parse().ok())
                .flatten()
            };
            let revision = parse_revision(&revision)?;
            let view_revision = parse_revision(&view_revision)?;
            let action = match action.as_str() {
                "start" => Command::Start,
                "stop" => Command::Stop,
                "cancel" => Command::Cancel,
                "history" => Command::History,
                "meetings" => Command::Meetings,
                "rewrite" => Command::Rewrite,
                "settings" => Command::Settings,
                "snooze" => Command::Snooze,
                "more" => Command::More,
                _ => return None,
            };
            Some(Event::Command(revision, view_revision, action))
        }
    }
}

fn active(phase: Phase) -> bool {
    matches!(phase, Phase::Recording | Phase::Transcribing)
}

#[derive(Default)]
struct HoverIntent {
    inside: bool,
    leave_until: Option<Instant>,
}
impl HoverIntent {
    fn update(&mut self, inside: bool, now: Instant) {
        if inside {
            self.leave_until = None;
        } else if self.inside {
            self.leave_until = Some(now + Duration::from_millis(300));
        }
        self.inside = inside;
    }
    fn expanded(&self, now: Instant) -> bool {
        self.inside || self.leave_until.is_some_and(|until| now < until)
    }
}

#[derive(Default)]
struct Visibility {
    enabled: bool,
    snoozed_until: Option<Instant>,
}
impl Visibility {
    fn update_preference(&mut self, requested: bool, phase: Phase) {
        // Defer either change until idle: never remove Stop/Cancel halfway
        // through a recording, nor introduce a new control surface mid-session.
        if !active(phase) {
            if requested && !self.enabled {
                // Explicitly switching the feature back on restores it now.
                self.snoozed_until = None;
            }
            self.enabled = requested;
        }
    }
    fn snoozed(&self, now: Instant) -> bool {
        self.snoozed_until.is_some_and(|until| now < until)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}
impl Bounds {
    fn rect(self) -> ScreenRect {
        ScreenRect {
            left: self.x,
            top: self.y,
            right: self.x.saturating_add(self.width),
            bottom: self.y.saturating_add(self.height),
        }
    }
}

const MOTION_FRAME: Duration = Duration::from_millis(16);
const OPEN_DURATION: Duration = Duration::from_millis(220);
const CLOSE_DURATION: Duration = Duration::from_millis(160);

fn settle_progress(t: f64) -> f64 {
    // Critically damped response: a soft initial acceleration, then a quick
    // settle. No geometric overshoot: the pointer anchor and work area stay safe.
    let t = t.clamp(0.0, 1.0);
    let response = |v: f64| 1.0 - (1.0 + 7.0 * v) * (-7.0 * v).exp();
    response(t) / response(1.0)
}

struct Tween {
    from: Bounds,
    to: Bounds,
    started: Instant,
    duration: Duration,
}
impl Tween {
    fn sample(&self, now: Instant) -> Bounds {
        let t = (now.saturating_duration_since(self.started).as_secs_f64()
            / self.duration.as_secs_f64())
        .clamp(0.0, 1.0);
        let eased = settle_progress(t);
        let lerp =
            |a: i32, b: i32| (f64::from(a) + (f64::from(b) - f64::from(a)) * eased).round() as i32;
        Bounds {
            x: lerp(self.from.x, self.to.x),
            y: lerp(self.from.y, self.to.y),
            width: lerp(self.from.width, self.to.width),
            height: lerp(self.from.height, self.to.height),
        }
    }
}

#[derive(Default)]
struct Motion {
    current: Option<Bounds>,
    tween: Option<Tween>,
    next_frame: Option<Instant>,
}
impl Motion {
    fn layout(&mut self, target: Bounds, now: Instant, animate: bool, opening: bool) -> Bounds {
        if !animate || self.current.is_none() {
            self.current = Some(target);
            self.tween = None;
            self.next_frame = None;
            return target;
        }
        if self.tween.as_ref().map(|t| t.to).or(self.current) != Some(target) {
            // Reverse from the last actually painted geometry, not the old end.
            self.tween = Some(Tween {
                from: self.current.unwrap(),
                to: target,
                started: now,
                duration: if opening {
                    OPEN_DURATION
                } else {
                    CLOSE_DURATION
                },
            });
            self.next_frame = Some(now + MOTION_FRAME);
        }
        if let Some(tween) = &self.tween {
            if now >= tween.started + tween.duration {
                self.current = Some(tween.to);
                self.tween = None;
                self.next_frame = None;
            } else if self.next_frame.is_some_and(|at| now >= at) {
                self.current = Some(tween.sample(now));
                self.next_frame = Some((now + MOTION_FRAME).min(tween.started + tween.duration));
            }
        }
        self.current.unwrap()
    }
    fn moving(&self) -> bool {
        self.tween.is_some()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Surface {
    #[default]
    Compact,
    Quick,
    Menu,
    Status,
}
impl Surface {
    fn size(self) -> (f64, f64) {
        match self {
            Self::Compact => (48.0, 18.0),
            Self::Quick => (160.0, 36.0),
            Self::Menu => (160.0, 200.0),
            Self::Status => (248.0, 40.0),
        }
    }
    fn footer(self) -> f64 {
        match self {
            Self::Compact => 18.0,
            Self::Status => 40.0,
            _ => 36.0,
        }
    }
}

fn paint_shape(height_physical: i32, scale: f64, status: bool) -> (f64, f64) {
    let height = f64::from(height_physical.max(1)) / scale.clamp(0.5, 16.0);
    let menu_reveal = ((height - 36.0) / 164.0).clamp(0.0, 1.0);
    let radius = if status {
        20.0
    } else {
        18.0 - 4.0 * menu_reveal
    };
    (radius.min(height / 2.0), menu_reveal)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InteractionView {
    surface: Surface,
    snapshot: Snapshot,
    ready: bool,
    enabled: bool,
    recovery: bool,
    moving: bool,
}
impl InteractionView {
    fn permits(self, command: Command) -> bool {
        if self.surface == Surface::Compact {
            return false;
        }
        if self.snapshot.phase == Phase::Recording {
            // Stop and Cancel remain reachable even if the preference just changed.
            return matches!(command, Command::Stop | Command::Cancel);
        }
        if active(self.snapshot.phase) || self.moving || !self.enabled {
            return false;
        }
        match command {
            Command::Start => self.ready,
            Command::Stop | Command::Cancel => false,
            Command::More => matches!(self.surface, Surface::Quick | Surface::Menu),
            Command::History => self.surface == Surface::Menu || self.recovery,
            Command::Settings => self.surface == Surface::Menu || !self.ready,
            Command::Meetings | Command::Rewrite | Command::Snooze => self.surface == Surface::Menu,
        }
    }
}

#[derive(Default)]
struct ViewAuthority {
    revision: u64,
    view: Option<InteractionView>,
}
impl ViewAuthority {
    fn update(&mut self, view: Option<InteractionView>) {
        if self.view != view {
            self.revision = self.revision.wrapping_add(1);
            self.view = view;
        }
    }
    fn permits(&self, revision: u64, command: Command) -> bool {
        self.revision == revision && self.view.is_some_and(|v| v.permits(command))
    }
    fn invalidate(&mut self) {
        self.update(None);
    }
}

fn surface_for(hovered: bool, phase: Phase, notice: bool, menu_open: &mut bool) -> Surface {
    if !hovered || notice || active(phase) {
        *menu_open = false;
    }
    if notice {
        Surface::Status
    } else if phase == Phase::Transcribing {
        // Inference has no cancellable action: keep the same tiny indicator.
        Surface::Compact
    } else if *menu_open {
        Surface::Menu
    } else if hovered {
        Surface::Quick
    } else {
        Surface::Compact
    }
}

/// Everything the capsule covers now (`drawn`, which may be the open menu or a
/// frame of its animation) or can grow to without a click: the hover controls
/// and the status message. A notice placed clear of this is not covered by the
/// next hover or error, and moves again only if the menu opens under it.
fn keep_clear_area(work: RECT, scale: f64, edge: &str, drawn: Bounds) -> ScreenRect {
    [Surface::Compact, Surface::Quick, Surface::Status]
        .into_iter()
        .map(|surface| bounds(work, scale, edge, surface))
        .fold(drawn.rect(), |area, surface| area.union(surface.rect()))
}

fn bounds(work: RECT, scale: f64, edge: &str, surface: Surface) -> Bounds {
    let scale = if scale.is_finite() {
        scale.clamp(0.5, 16.0)
    } else {
        1.0
    };
    let available_w = work.right.saturating_sub(work.left).max(1);
    let available_h = work.bottom.saturating_sub(work.top).max(1);
    let (logical_width, logical_height) = surface.size();
    let width = ((logical_width * scale).round() as i32).clamp(1, available_w);
    let height = ((logical_height * scale).round() as i32).clamp(1, available_h);
    let margin = (16.0 * scale).round() as i32;
    let max_x = work.right.saturating_sub(width).max(work.left);
    let max_y = work.bottom.saturating_sub(height).max(work.top);
    // Preserve the tiny peek control under the pointer, NOT a recording button.
    // Hover/tapping the compact capsule must never turn into a Start click.
    let compact_height = ((18.0 * scale).round() as i32).clamp(1, available_h);
    let centre_x = work
        .left
        .saturating_add(available_w.saturating_sub(width) / 2);
    let centre_y = work
        .top
        .saturating_add(available_h.saturating_sub(compact_height) / 2);
    let footer_height = (surface.footer() * scale).round() as i32;
    let lift = if surface != Surface::Compact {
        height
            .saturating_sub(footer_height / 2)
            .saturating_sub(compact_height / 2)
    } else {
        0
    };
    let (x, y) = match edge {
        "left" => (
            work.left.saturating_add(margin),
            centre_y.saturating_sub(lift),
        ),
        "right" => (max_x.saturating_sub(margin), centre_y.saturating_sub(lift)),
        _ => (
            centre_x,
            work.bottom
                .saturating_sub(compact_height)
                .saturating_sub(margin)
                .saturating_sub(lift),
        ),
    };
    Bounds {
        x: x.clamp(work.left, max_x),
        y: y.clamp(work.top, max_y),
        width,
        height,
    }
}

unsafe extern "system" fn non_activating_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    match msg {
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_POINTERACTIVATE => PA_NOACTIVATE as LRESULT,
        WM_NCDESTROY => {
            unsafe {
                RemoveWindowSubclass(hwnd, Some(non_activating_proc), SUBCLASS_ID);
            }
            unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
        }
        _ => unsafe { DefSubclassProc(hwnd, msg, wp, lp) },
    }
}

pub struct ControlBar {
    // WebView must be released before its HWND and context.
    webview: wry::WebView,
    window: Window,
    _context: wry::WebContext,
    ready: bool,
    shown: bool,
    hover: HoverIntent,
    motion: Motion,
    reduced_motion: bool,
    last_surface: Surface,
    menu_open: bool,
    authority: ViewAuthority,
    motion_context: Option<(RECT, u32, String)>,
    visibility: Visibility,
    last_bounds: Option<Bounds>,
    last_snapshot: Option<Snapshot>,
    last_payload: String,
    anchor: HWND,
    error: Option<(&'static str, Instant)>,
    page_ratio: Option<u32>,
    text_scale: f64,
    recovery_until: Option<Instant>,
    /// What a passive notice must leave uncovered; see [`keep_clear_area`].
    keep_clear: Option<ScreenRect>,
}
impl ControlBar {
    pub fn new<T: 'static>(
        target: &EventLoopWindowTarget<T>,
        directory: std::path::PathBuf,
        on_event: impl Fn(Event) + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        let window = WindowBuilder::new()
            .with_title("VocalCode Desktop Controls")
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top(true)
            .with_resizable(false)
            .with_focused(false)
            .with_focusable(false)
            .with_visible(false)
            .with_skip_taskbar(true)
            .with_undecorated_shadow(false)
            .with_inner_size(tao::dpi::LogicalSize::new(48.0, 18.0))
            .build(target)?;
        let hwnd = window.hwnd() as HWND;
        // Tao's skip-taskbar implementation calls ITaskbarList::DeleteTab but
        // retains APPWINDOW. Tool-window style also excludes Alt+Tab and keeps
        // the native show path from resurrecting a separate taskbar item.
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            SetLastError(0);
            let old = SetWindowLongPtrW(
                hwnd,
                GWL_EXSTYLE,
                ((style & !WS_EX_APPWINDOW) | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) as isize,
            );
            if old == 0 && GetLastError() != 0 {
                anyhow::bail!("could not set desktop control window style");
            }
        }
        if unsafe {
            windows_sys::Win32::UI::Shell::SetWindowSubclass(
                hwnd,
                Some(non_activating_proc),
                SUBCLASS_ID,
                0,
            )
        } == 0
        {
            anyhow::bail!("desktop controls could not install non-activation guard");
        }
        let mut context = wry::WebContext::new(Some(directory));
        let document = format!(
            "data:text/html;charset=utf-8;base64,{}",
            STANDARD.encode(HTML)
        );
        // Alpha must be enabled at all three layers: HWND, WebView and HTML.
        // CSS paints slightly inside the region so GDI never clips its AA edge.
        let webview = wry::WebViewBuilder::new_with_web_context(&mut context)
            .with_html(HTML)
            .with_focused(false)
            .with_transparent(true)
            .with_navigation_handler(move |url| url == "about:blank" || url == document)
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
            shown: false,
            hover: HoverIntent::default(),
            motion: Motion::default(),
            reduced_motion: true,
            last_surface: Surface::Compact,
            menu_open: false,
            authority: ViewAuthority::default(),
            motion_context: None,
            visibility: Visibility::default(),
            last_bounds: None,
            last_snapshot: None,
            last_payload: String::new(),
            anchor: std::ptr::null_mut(),
            error: None,
            page_ratio: None,
            text_scale: 1.0,
            recovery_until: None,
            keep_clear: None,
        })
    }

    /// Only explicit open commands may activate the main window. Recording
    /// actions go through the engine mailbox and never manipulate foreground.
    pub fn event(
        &mut self,
        event: Event,
        snapshot: Snapshot,
        ready: bool,
        recording: bool,
        enabled: bool,
        bridge: &Bridge,
    ) -> Option<&'static str> {
        let now = Instant::now();
        // Acknowledge even a stale/rejected click; otherwise the page could
        // remain optimistically disabled when the authoritative state did not change.
        if matches!(event, Event::Command(..)) {
            self.last_payload.clear();
        }
        match event {
            Event::Ready => {
                self.hide();
                self.ready = true;
                self.page_ratio = None;
                self.last_payload.clear();
            }
            Event::Scale(ratio) => {
                if self.page_ratio != Some(ratio) && (500..=16000).contains(&ratio) {
                    // WebView2 includes Windows text enlargement in its CSS
                    // pixel ratio. HWND DPI alone clipped a 40px bar to 32 CSS
                    // pixels on this machine's 127% accessibility text setting.
                    let dpi = unsafe {
                        windows_sys::Win32::UI::HiDpi::GetDpiForWindow(self.window.hwnd() as HWND)
                    };
                    self.text_scale = additional_text_scale(ratio, dpi);
                    self.page_ratio = Some(ratio);
                    self.last_bounds = None;
                }
            }
            Event::ReducedMotion(reduced) => self.reduced_motion = reduced,
            Event::Hover(inside) if self.shown => self.hover.update(inside, now),
            Event::Command(revision, view_revision, command)
                if self.shown
                    && self.ready
                    && self.last_snapshot == Some(snapshot)
                    && revision == snapshot.revision
                    && (enabled || matches!(command, Command::Stop | Command::Cancel))
                    && self.authority.permits(view_revision, command) =>
            {
                let idle = !active(snapshot.phase)
                    && !recording
                    && !bridge.is_busy()
                    && !self.motion.moving();
                let action = match command {
                    Command::Start if enabled && idle => Some(Action::Start),
                    Command::Stop => Some(Action::Stop),
                    Command::Cancel => Some(Action::Cancel),
                    Command::More if enabled && idle && self.hover.expanded(now) => {
                        self.menu_open = !self.menu_open;
                        self.authority.invalidate();
                        None
                    }
                    Command::History | Command::Meetings | Command::Rewrite | Command::Settings
                        if idle =>
                    {
                        self.hover = HoverIntent::default();
                        self.menu_open = false;
                        self.authority.invalidate();
                        if command == Command::History {
                            self.recovery_until = None;
                        }
                        return Some(match command {
                            Command::History => "history",
                            Command::Meetings => "meetings",
                            Command::Rewrite => "rewrite",
                            _ => "behaviour",
                        });
                    }
                    Command::Snooze if idle => {
                        self.visibility.snoozed_until = Some(now + Duration::from_secs(3600));
                        self.hide();
                        None
                    }
                    _ => None,
                };
                if let Some(action) = action {
                    if let Err(error) = bridge.submit(action, snapshot, ready, recording, now) {
                        self.error = Some((error, now));
                    }
                }
            }
            _ => {}
        }
        None
    }

    /// Returns true only when this surface can replace the passive indicator.
    pub fn tick(&mut self, frame: Frame<'_>, bridge: &Bridge) -> bool {
        let Frame {
            enabled: requested,
            edge,
            language,
            snapshot,
            ready,
            level,
        } = frame;
        let now = Instant::now();
        self.visibility.update_preference(requested, snapshot.phase);
        if !self.ready || self.page_ratio.is_none() || !self.visibility.enabled {
            self.hide();
            return false;
        }
        if snapshot.phase == Phase::Recording
            && self
                .last_snapshot
                .is_some_and(|last| last.phase != Phase::Recording)
        {
            self.recovery_until = None;
        }
        if bridge.take_recent_recovery(now) {
            self.recovery_until = Some(now + Duration::from_secs(12));
        }
        let recovery =
            !active(snapshot.phase) && self.recovery_until.is_some_and(|until| now < until);
        if let Some(error) = bridge.take_error() {
            self.error = Some((error, now));
        }
        if self
            .error
            .is_some_and(|(_, at)| now.duration_since(at) > Duration::from_secs(4))
        {
            self.error = None;
        }
        let foreground = unsafe { GetForegroundWindow() };
        if !self.shown || (!self.hover.expanded(now) && !active(snapshot.phase)) {
            self.anchor = foreground;
        }
        let anchor = if self.anchor.is_null() {
            self.window.hwnd() as HWND
        } else {
            self.anchor
        };
        let monitor = unsafe { MonitorFromWindow(anchor, MONITOR_DEFAULTTONEAREST) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..MONITORINFO::default()
        };
        if monitor.is_null() || unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
            self.hide();
            return false;
        }
        if !active(snapshot.phase)
            && (self.visibility.snoozed(now)
                || foreground_is_fullscreen(foreground, info.rcMonitor))
        {
            self.hide();
            return false;
        }
        let mut dx = 0;
        let mut dy = 0;
        let scale = if unsafe {
            windows_sys::Win32::UI::HiDpi::GetDpiForMonitor(
                monitor,
                windows_sys::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI,
                &mut dx,
                &mut dy,
            )
        } >= 0
            && dx > 0
        {
            f64::from(dx) / 96.0
        } else {
            self.window.scale_factor()
        };
        // Normal recording/processing never opens a second, larger indicator.
        // A wider message is reserved for actionable failure/recovery notices.
        let status = self.error.is_some() || recovery;
        let surface = surface_for(
            self.hover.expanded(now),
            snapshot.phase,
            status,
            &mut self.menu_open,
        );
        let expanded = surface != Surface::Compact;
        let menu = surface == Surface::Menu;
        let effective_scale = scale * self.text_scale;
        let target = bounds(info.rcWork, effective_scale, edge, surface);
        let context = (
            info.rcWork,
            (effective_scale * 1000.0).round() as u32,
            edge.to_string(),
        );
        let same_context = self.motion_context.as_ref().is_some_and(|old| {
            old.0.left == context.0.left
                && old.0.top == context.0.top
                && old.0.right == context.0.right
                && old.0.bottom == context.0.bottom
                && old.1 == context.1
                && old.2 == context.2
        });
        let same_phase = self
            .last_snapshot
            .is_some_and(|last| last.phase == snapshot.phase);
        let animate = self.shown
            && !self.reduced_motion
            && same_context
            && same_phase
            && self.error.is_none()
            && !recovery
            && (self.motion.moving() || self.last_surface != surface);
        let opening = target.height > self.last_bounds.map_or(0, |b| b.height);
        let layout = self.motion.layout(target, now, animate, opening);
        self.keep_clear = Some(keep_clear_area(info.rcWork, effective_scale, edge, layout));
        self.motion_context = Some(context);
        self.last_surface = surface;
        let moving = self.motion.moving();
        let (paint_radius, menu_reveal) = paint_shape(layout.height, effective_scale, status);
        let visual_menu = menu || (!status && moving && menu_reveal > 0.0);
        self.authority.update(Some(InteractionView {
            surface,
            snapshot,
            ready,
            enabled: requested,
            recovery,
            moving,
        }));
        let visual_expanded = expanded || moving;
        let compact = bounds(info.rcWork, effective_scale, edge, Surface::Compact);
        let full = bounds(info.rcWork, effective_scale, edge, Surface::Quick);
        let reveal = if full.width > compact.width {
            (f64::from(layout.width - compact.width) / f64::from(full.width - compact.width))
                .clamp(0.0, 1.0)
        } else {
            f64::from(u8::from(expanded))
        };
        if self.last_bounds != Some(layout) {
            let hwnd = self.window.hwnd() as HWND;
            unsafe {
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    layout.x,
                    layout.y,
                    layout.width,
                    layout.height,
                    SWP_NOACTIVATE,
                );
                let radius = ((paint_radius * 2.0 * effective_scale).round() as i32)
                    .min(layout.height)
                    .min(layout.width);
                let region =
                    CreateRoundRectRgn(0, 0, layout.width + 1, layout.height + 1, radius, radius);
                if !region.is_null() && SetWindowRgn(hwnd, region, 1) == 0 {
                    DeleteObject(region);
                }
            }
            self.last_bounds = Some(layout);
        }
        let phase = match snapshot.phase {
            Phase::Recording => "recording",
            Phase::Transcribing => "processing",
            _ => "idle",
        };
        let level = if level.is_finite() {
            (level.clamp(0., 1.) * 10.).round() as u32
        } else {
            0
        };
        let payload = serde_json::json!({"phase": phase, "revision": snapshot.revision.to_string(),"view_revision":self.authority.revision.to_string(),
            "width_physical":layout.width,"height_physical":layout.height,
            "radius_milli":(paint_radius*1000.0).round() as u32,"menu_reveal_milli":if moving {(menu_reveal*1000.0).round() as u32}else{1000},
            "ready": ready, "busy": bridge.is_busy(), "expanded": visual_expanded, "level": level,
            "moving":moving,"opening":expanded,"menu":visual_menu,"status":status,"reveal_milli":(reveal * 1000.0).round() as u32,
            "language": language, "edge": edge, "error": self.error.map(|(e,_)| e),"recovery":recovery})
        .to_string();
        if self.last_payload != payload {
            if self
                .webview
                .evaluate_script(&format!("window.vocalcodeControl({payload})"))
                .is_err()
            {
                self.hide();
                return false;
            }
            self.last_payload = payload;
        }
        self.last_snapshot = Some(snapshot);
        if !self.shown {
            unsafe {
                ShowWindow(self.window.hwnd() as HWND, SW_SHOWNOACTIVATE);
            }
            self.shown = true;
        }
        true
    }

    /// Actual presentation state survives a temporarily contended config lock.
    pub fn is_shown(&self) -> bool {
        self.shown
    }

    /// The screen area the indicator's notices must not cover, while the
    /// capsule is on screen.
    pub fn keep_clear(&self) -> Option<ScreenRect> {
        self.keep_clear.filter(|_| self.shown)
    }

    /// Brief frame cadence while morphing; the settled idle bar has no timer.
    pub fn next_wake(&self, now: Instant) -> Option<Instant> {
        if !self.shown {
            return None;
        }
        self.motion
            .next_frame
            .into_iter()
            .chain(self.hover.leave_until.filter(|at| *at > now))
            .min()
    }

    fn hide(&mut self) {
        if self.shown {
            unsafe {
                ShowWindow(self.window.hwnd() as HWND, SW_HIDE);
            }
        }
        self.shown = false;
        self.hover = HoverIntent::default();
        self.motion = Motion::default();
        self.menu_open = false;
        self.authority.invalidate();
        self.last_surface = Surface::Compact;
        self.motion_context = None;
        self.last_snapshot = None;
    }

    /// Debug-only fixture hooks. They do not exist in release builds and do
    /// not add any IPC authority to the shipped control document.
    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub fn inspect_native_contract(&self) -> serde_json::Value {
        let hwnd = self.window.hwnd() as HWND;
        let before = unsafe { GetForegroundWindow() };
        let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
        let mut rect = RECT::default();
        unsafe {
            GetClientRect(hwnd, &mut rect);
        }
        let mouse = unsafe { SendMessageW(hwnd, WM_MOUSEACTIVATE, hwnd as usize, 0) };
        let pointer = unsafe { SendMessageW(hwnd, WM_POINTERACTIVATE, hwnd as usize, 0) };
        let view_size = self
            .webview
            .bounds()
            .ok()
            .map(|r| r.size.to_physical::<u32>(self.window.scale_factor()));
        serde_json::json!({"no_activate":style & WS_EX_NOACTIVATE != 0,"menu_open":self.menu_open,
            "not_in_taskbar":style & WS_EX_APPWINDOW == 0 && style & WS_EX_TOOLWINDOW != 0,"visible":unsafe { IsWindowVisible(hwnd) } != 0,
            "client_width":rect.right,"client_height":rect.bottom,"tao_scale":self.window.scale_factor(),
            "webview_width":view_size.map(|s|s.width),"webview_height":view_size.map(|s|s.height),
            "dpi":unsafe {windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd)},
            "mouse_no_activate":mouse == MA_NOACTIVATE as isize,"pointer_no_activate":pointer == PA_NOACTIVATE as isize,
            "focus_preserved":before == unsafe { GetForegroundWindow() }})
    }

    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub fn inspect_hidden_layout(
        &self,
        expanded: bool,
        phase: &str,
        edge: &str,
        language: &str,
        on_result: impl Fn(String) + Send + 'static,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.shown, "layout fixture must remain hidden");
        let short_menu = matches!(phase, "short-menu" | "short-menu-end");
        let motion_ms = phase
            .strip_prefix("opening-")
            .or_else(|| phase.strip_prefix("recording-opening-"))
            .or_else(|| phase.strip_prefix("menu-opening-"))
            .or_else(|| phase.strip_prefix("menu-closing-"))
            .and_then(|v| v.parse::<u64>().ok());
        let menu_motion = phase.starts_with("menu-opening-") || phase.starts_with("menu-closing-");
        let closing = phase.starts_with("menu-closing-");
        let display_phase = if phase.starts_with("recording-opening-") {
            "recording"
        } else if matches!(phase, "recovery" | "menu") || short_menu || motion_ms.is_some() {
            "idle"
        } else {
            phase
        };
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: if short_menu { 120 } else { 1040 },
        };
        let status = phase == "recovery";
        let expanded = expanded && phase != "processing";
        let surface = if !expanded {
            Surface::Compact
        } else if status {
            Surface::Status
        } else if phase == "menu" || short_menu || phase.starts_with("menu-opening-") {
            Surface::Menu
        } else {
            Surface::Quick
        };
        let mut layout = bounds(work, 1.0, edge, surface);
        let mut moving = false;
        let mut reveal = 1000;
        if let Some(ms) = motion_ms {
            let mut motion = Motion::default();
            let now = Instant::now();
            let from = if closing {
                Surface::Menu
            } else if menu_motion {
                Surface::Quick
            } else {
                Surface::Compact
            };
            motion.layout(bounds(work, 1.0, edge, from), now, false, false);
            motion.layout(layout, now, true, !closing);
            layout = motion.layout(layout, now + Duration::from_millis(ms), true, !closing);
            moving = motion.moving();
            reveal = ((layout.width - 48) * 1000 / 112).clamp(0, 1000);
        }
        self.window.set_inner_size(tao::dpi::LogicalSize::new(
            f64::from(layout.width) * self.text_scale,
            f64::from(layout.height) * self.text_scale,
        ));
        let size = self.window.inner_size();
        let (paint_radius, menu_reveal) = paint_shape(layout.height, 1.0, status);
        let payload = serde_json::json!({"phase":display_phase,"expanded":expanded,"revision":"15","view_revision":"1","ready":true,"busy":false,"moving":moving,"reveal_milli":reveal,
            "width_physical":size.width,"height_physical":size.height,
            "radius_milli":(paint_radius*1000.0).round() as u32,"menu_reveal_milli":if moving {(menu_reveal*1000.0).round() as u32}else{1000},
            "level":7,"edge":edge,"language":language,"recovery":phase=="recovery","status":status,"menu":phase=="menu"||short_menu||(menu_motion && layout.height>36)});
        self.webview
            .evaluate_script(&format!("window.vocalcodeControl({payload})"))?;
        if short_menu {
            self.webview.evaluate_script(if phase=="short-menu-end" {
                "document.getElementById('menu').scrollTop=document.getElementById('menu').scrollHeight"
            } else {"document.getElementById('menu').scrollTop=0"})?;
        }
        self.inspect_hidden_sample(on_result)
    }

    /// Render-scale stress is confined to an isolated hidden fixture. It never
    /// changes Windows accessibility settings or the installed app's profile.
    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub fn inspect_hidden_zoom(&self, zoom: f64) -> anyhow::Result<()> {
        anyhow::ensure!(!self.shown && (1.0..=3.0).contains(&zoom));
        self.webview.zoom(zoom)?;
        Ok(())
    }

    /// A second sample after resize delivery distinguishes the settled layout
    /// from WebView2's asynchronous first response to a native size change.
    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub fn inspect_hidden_sample(
        &self,
        on_result: impl Fn(String) + Send + 'static,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.shown);
        self.webview.evaluate_script_with_callback(
            r#"(()=>{const menu=document.getElementById('menu'),clip=menu.getBoundingClientRect(),body=document.body.getBoundingClientRect();return JSON.stringify({width:innerWidth,height:innerHeight,dpr:devicePixelRatio,moving:document.body.dataset.moving==='true',bodyWidth:body.width,bodyHeight:body.height,overflow:document.body.scrollWidth>innerWidth||document.body.scrollHeight>innerHeight,menu:{top:clip.top,bottom:clip.bottom,scrollHeight:menu.scrollHeight,height:menu.clientHeight,scrollTop:menu.scrollTop,opacity:getComputedStyle(menu).opacity},buttons:Array.from(document.querySelectorAll('button')).filter(b=>!b.hidden).map(b=>{const r=b.getBoundingClientRect();return {id:b.id,disabled:b.disabled,name:b.getAttribute('aria-label'),x:r.x,right:r.right,y:r.y,bottom:r.bottom,inMenu:menu.contains(b),inClip:r.top>=clip.top&&r.bottom<=clip.bottom}}),elements:document.querySelectorAll('*').length});})()"#,
            on_result,
        )?;
        Ok(())
    }

    /// Explicit visible compositor fixture. Not reachable from production IPC.
    #[cfg(debug_assertions)]
    #[allow(dead_code)]
    pub fn inspect_composited_window(&self, x: i32, y: i32) -> HWND {
        // Opt-in visual fixture only: no engine, no user profile, no activation.
        let hwnd = self.window.hwnd() as HWND;
        unsafe {
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect);
            let diameter = rect.bottom.min(rect.right);
            let region =
                CreateRoundRectRgn(0, 0, rect.right + 1, rect.bottom + 1, diameter, diameter);
            if !region.is_null() && SetWindowRgn(hwnd, region, 1) == 0 {
                DeleteObject(region);
            }
            SetWindowPos(hwnd, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        hwnd
    }

    /// Integration fixture: real WebView IPC, natively hidden, isolated mailbox.
    #[cfg(debug_assertions)]
    #[allow(dead_code, clippy::too_many_arguments)]
    pub fn inspect_hidden_interaction(
        &mut self,
        snapshot: Snapshot,
        rendered_revision: u64,
        rendered_ready: bool,
        simulated_shown: bool,
        button: &str,
        clicks: usize,
        stale_view: bool,
        collapsed: bool,
        on_result: impl Fn(String) + Send + 'static,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.window.is_visible(),
            "interaction fixture must stay natively hidden"
        );
        anyhow::ensure!(
            self.ready && self.page_ratio.is_some(),
            "wait for the real page handshake"
        );
        anyhow::ensure!(
            ["main", "cancel", "more", "history", "meetings", "rewrite", "settings", "snooze"]
                .contains(&button)
                && (1..=2).contains(&clicks),
            "invalid fixture action"
        );
        self.shown = simulated_shown;
        self.hover.update(true, Instant::now());
        let rendered_menu = button != "more";
        self.menu_open = rendered_menu && !collapsed;
        self.last_snapshot = simulated_shown.then_some(snapshot);
        let view = InteractionView {
            surface: if collapsed || snapshot.phase == Phase::Transcribing {
                Surface::Compact
            } else if snapshot.phase == Phase::Recording {
                Surface::Quick
            } else if self.menu_open {
                Surface::Menu
            } else {
                Surface::Quick
            },
            snapshot,
            ready: rendered_ready,
            enabled: true,
            recovery: false,
            moving: false,
        };
        self.authority.update(Some(view));
        let rendered_view_revision = self.authority.revision;
        if stale_view {
            self.authority.invalidate();
            self.authority.update(Some(view));
        }
        let phase = match snapshot.phase {
            Phase::Recording => "recording",
            Phase::Transcribing => "processing",
            _ => "idle",
        };
        let payload = serde_json::json!({"phase":phase,"expanded":true,"menu":rendered_menu,"revision":rendered_revision.to_string(),"view_revision":rendered_view_revision.to_string(),"ready":rendered_ready,"busy":false,"level":0,"edge":"bottom","language":"en"});
        let button = serde_json::to_string(button)?;
        self.webview.evaluate_script_with_callback(&format!("window.vocalcodeControl({payload}); for(let i=0;i<{clicks};i++)document.getElementById({button}).click(); JSON.stringify({{executed:true}})"),on_result)?;
        Ok(())
    }
}

fn additional_text_scale(ratio_milli: u32, hwnd_dpi: u32) -> f64 {
    crate::overlay::windows_text_scale(ratio_milli, hwnd_dpi)
}

fn foreground_is_fullscreen(hwnd: HWND, monitor: RECT) -> bool {
    if hwnd.is_null() {
        return false;
    }
    let mut pid = 0;
    let mut rect = RECT::default();
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == std::process::id() || IsIconic(hwnd) != 0 || GetWindowRect(hwnd, &mut rect) == 0 {
            return false;
        }
    }
    // Maximized windows have a taskbar-sized gap. Only a monitor-covering
    // foreground window hides the idle bar, never active recording controls.
    rect.left <= monitor.left
        && rect.top <= monitor.top
        && rect.right >= monitor.right
        && rect.bottom >= monitor.bottom
}

#[cfg(test)]
mod tests {
    use super::*;
    fn interaction_view(surface: Surface) -> InteractionView {
        InteractionView {
            surface,
            snapshot: Snapshot {
                phase: Phase::Idle,
                revision: 1,
            },
            ready: true,
            enabled: true,
            recovery: false,
            moving: false,
        }
    }
    #[test]
    fn menu_border_morphs_continuously_without_a_corner_jump() {
        for scale in [1.0_f64, 1.25, 1.27, 1.5, 2.0, 3.81] {
            let (radius, reveal) = paint_shape((18.0 * scale).round() as i32, scale, false);
            assert!((radius - 9.0).abs() < 0.3 && reveal == 0.0);
            let mut previous = 18.0;
            for height in 36..=200 {
                let (radius, reveal) =
                    paint_shape((f64::from(height) * scale).round() as i32, scale, false);
                assert!((14.0..=18.0).contains(&radius));
                assert!((0.0..=1.0).contains(&reveal));
                assert!((previous - radius).abs() < 0.3);
                previous = radius;
            }
            assert_eq!(
                paint_shape((40.0 * scale).round() as i32, scale, true).1,
                ((40.0 * scale).round() / scale - 36.0) / 164.0
            );
        }
    }
    #[test]
    fn native_visibility_not_a_stale_dom_decides_which_buttons_have_authority() {
        let quick = interaction_view(Surface::Quick);
        assert!(quick.permits(Command::Start));
        assert!(quick.permits(Command::More));
        for command in [
            Command::History,
            Command::Meetings,
            Command::Rewrite,
            Command::Settings,
            Command::Snooze,
            Command::Stop,
            Command::Cancel,
        ] {
            assert!(!quick.permits(command));
        }
        for command in [
            Command::Start,
            Command::More,
            Command::History,
            Command::Meetings,
            Command::Rewrite,
            Command::Settings,
            Command::Snooze,
        ] {
            assert!(!interaction_view(Surface::Compact).permits(command));
            assert!(interaction_view(Surface::Menu).permits(command));
        }
        let recovery = InteractionView {
            recovery: true,
            ..interaction_view(Surface::Status)
        };
        assert!(recovery.permits(Command::History));
        assert!(!recovery.permits(Command::Meetings));
    }
    #[test]
    fn closing_and_reopening_rejects_previous_presentation_even_with_same_engine_revision() {
        let view = interaction_view(Surface::Menu);
        let mut authority = ViewAuthority::default();
        authority.update(Some(view));
        let previous = authority.revision;
        assert!(authority.permits(previous, Command::Meetings));
        authority.invalidate();
        assert!(!authority.permits(previous, Command::Meetings));
        authority.update(Some(view));
        assert!(!authority.permits(previous, Command::Meetings));
        assert!(authority.permits(authority.revision, Command::Meetings));
    }
    #[test]
    fn ordinary_meter_refreshes_do_not_invalidate_clicks_but_moving_geometry_does() {
        let view = interaction_view(Surface::Quick);
        let mut authority = ViewAuthority::default();
        authority.update(Some(view));
        let stable = authority.revision;
        for _ in 0..1000 {
            authority.update(Some(view));
        }
        assert_eq!(authority.revision, stable);
        authority.update(Some(InteractionView {
            moving: true,
            ..view
        }));
        assert!(!authority.permits(stable, Command::Start));
        assert!(!authority.permits(authority.revision, Command::Start));
    }
    #[test]
    fn status_controls_allow_stop_with_preference_off_and_settings_before_ready() {
        let recording = InteractionView {
            snapshot: Snapshot {
                phase: Phase::Recording,
                revision: 2,
            },
            enabled: false,
            moving: true,
            ..interaction_view(Surface::Status)
        };
        assert!(recording.permits(Command::Stop));
        assert!(recording.permits(Command::Cancel));
        assert!(!recording.permits(Command::Start));
        let unready = InteractionView {
            ready: false,
            ..interaction_view(Surface::Quick)
        };
        assert!(unready.permits(Command::Settings));
        assert!(!unready.permits(Command::Start));
    }
    #[test]
    fn presentation_revision_is_required_typed_bounded_and_lossless() {
        let json = r#"{"type":"action","revision":"1","view_revision":"18446744073709551615","action":"more"}"#;
        assert_eq!(
            parse_event(json),
            Some(Event::Command(1, u64::MAX, Command::More))
        );
        for invalid in ["", "-1", "1.5", "18446744073709551616", "not-a-number"] {
            assert!(parse_event(&format!(
                r#"{{"type":"action","revision":"1","view_revision":"{invalid}","action":"more"}}"#
            ))
            .is_none());
        }
        assert!(parse_event(r#"{"type":"action","revision":"1","action":"more"}"#).is_none());
        assert!(parse_event(
            r#"{"type":"action","revision":"1","view_revision":7,"action":"more"}"#
        )
        .is_none());
    }
    #[test]
    fn soft_settle_has_no_overshoot_and_accelerates_without_an_initial_jump() {
        assert_eq!(settle_progress(0.0), 0.0);
        assert_eq!(settle_progress(1.0), 1.0);
        assert_eq!(settle_progress(-1.0), 0.0);
        assert_eq!(settle_progress(2.0), 1.0);
        assert!(settle_progress(16.0 / 220.0) < 0.1);
        let mut previous = 0.0;
        for frame in 0..=100 {
            let p = settle_progress(f64::from(frame) / 100.0);
            assert!((0.0..=1.0).contains(&p) && p >= previous);
            previous = p;
        }
    }
    #[test]
    fn recording_and_processing_stay_tiny_without_hover() {
        for phase in [Phase::Idle, Phase::Recording, Phase::Transcribing] {
            let mut menu = true;
            assert_eq!(
                surface_for(false, phase, false, &mut menu),
                Surface::Compact
            );
            assert!(!menu);
        }
        let mut menu = true;
        assert_eq!(
            surface_for(true, Phase::Recording, false, &mut menu),
            Surface::Quick
        );
        assert!(!menu);
        assert_eq!(
            surface_for(true, Phase::Transcribing, false, &mut menu),
            Surface::Compact
        );
        assert_eq!(
            surface_for(false, Phase::Idle, true, &mut menu),
            Surface::Status
        );
    }

    #[test]
    fn compact_recording_rejects_actions_until_controls_are_visible() {
        let compact = InteractionView {
            snapshot: Snapshot {
                phase: Phase::Recording,
                revision: 42,
            },
            ..interaction_view(Surface::Compact)
        };
        for action in [
            Command::Start,
            Command::Stop,
            Command::Cancel,
            Command::More,
        ] {
            assert!(!compact.permits(action));
        }
        let quick = InteractionView {
            surface: Surface::Quick,
            ..compact
        };
        assert!(quick.permits(Command::Stop));
        assert!(quick.permits(Command::Cancel));
        assert!(!quick.permits(Command::Start));
    }

    #[test]
    fn secondary_menu_requires_explicit_intent_and_resets_after_leaving_or_recording() {
        let mut menu = false;
        assert_eq!(
            surface_for(true, Phase::Idle, false, &mut menu),
            Surface::Quick
        );
        menu = true; // Only the explicit More command changes this to true.
        assert_eq!(
            surface_for(true, Phase::Idle, false, &mut menu),
            Surface::Menu
        );
        assert_eq!(
            surface_for(false, Phase::Idle, false, &mut menu),
            Surface::Compact
        );
        assert!(!menu);
        assert_eq!(
            surface_for(true, Phase::Idle, false, &mut menu),
            Surface::Quick
        );
        menu = true;
        assert_eq!(
            surface_for(true, Phase::Idle, true, &mut menu),
            Surface::Status
        );
        assert!(!menu);
        assert_eq!(
            surface_for(true, Phase::Idle, false, &mut menu),
            Surface::Quick
        );
    }
    #[test]
    fn bottom_expansion_is_centred_including_animation_on_scaled_offset_monitors() {
        for work in [
            RECT {
                left: 0,
                top: 0,
                right: 5120,
                bottom: 1400,
            },
            RECT {
                left: -1921,
                top: -400,
                right: -1,
                bottom: 1040,
            },
        ] {
            for scale in [1.0, 1.27, 1.5, 2.54] {
                let compact = bounds(work, scale, "bottom", Surface::Compact);
                for surface in [Surface::Quick, Surface::Menu, Surface::Status] {
                    let expanded = bounds(work, scale, "bottom", surface);
                    assert!((expanded.x * 2 + expanded.width - work.left - work.right).abs() <= 1);
                    let now = Instant::now();
                    let tween = Tween {
                        from: compact,
                        to: expanded,
                        started: now,
                        duration: OPEN_DURATION,
                    };
                    for ms in (0..=180).step_by(12) {
                        let b = tween.sample(now + Duration::from_millis(ms));
                        assert!((b.x * 2 + b.width - work.left - work.right).abs() <= 2);
                    }
                }
            }
        }
    }
    fn motion_bounds() -> (Bounds, Bounds) {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        (
            bounds(work, 1.0, "bottom", Surface::Compact),
            bounds(work, 1.0, "bottom", Surface::Quick),
        )
    }
    #[test]
    fn motion_is_short_monotonic_and_has_no_idle_frame_timer() {
        let (small, large) = motion_bounds();
        let now = Instant::now();
        let mut motion = Motion::default();
        assert_eq!(motion.layout(small, now, false, false), small);
        assert_eq!(motion.layout(large, now, true, true), small);
        assert_eq!(motion.next_frame, Some(now + MOTION_FRAME));
        let mut previous = small;
        for ms in (16..180).step_by(16) {
            let frame = motion.layout(large, now + Duration::from_millis(ms), true, true);
            assert!(frame.width >= previous.width && frame.width <= large.width);
            assert!(frame.height >= previous.height && frame.height <= large.height);
            previous = frame;
        }
        assert_eq!(motion.layout(large, now + OPEN_DURATION, true, true), large);
        assert!(!motion.moving());
        assert!(motion.next_frame.is_none());
    }
    #[test]
    fn motion_reverses_from_the_painted_frame_and_coalesces_resize_events() {
        let (small, large) = motion_bounds();
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.layout(small, now, false, false);
        motion.layout(large, now, true, true);
        assert_eq!(
            motion.layout(large, now + Duration::from_millis(1), true, true),
            small
        );
        let middle = motion.layout(large, now + Duration::from_millis(64), true, true);
        assert!(middle.width > small.width && middle.width < large.width);
        assert_eq!(
            motion.layout(small, now + Duration::from_millis(65), true, false),
            middle
        );
        assert_eq!(
            motion.layout(small, now + Duration::from_millis(66), true, false),
            middle
        );
        assert_eq!(
            motion.layout(
                small,
                now + Duration::from_millis(65) + CLOSE_DURATION,
                true,
                false
            ),
            small
        );
        assert!(motion.next_frame.is_none());
    }
    #[test]
    fn reduced_motion_or_phase_change_snaps_without_a_delayed_stop_button() {
        let (small, large) = motion_bounds();
        let now = Instant::now();
        let mut motion = Motion::default();
        motion.layout(small, now, false, false);
        motion.layout(large, now, true, true);
        assert_eq!(
            motion.layout(large, now + Duration::from_millis(10), false, true),
            large
        );
        assert!(!motion.moving());
        assert!(motion.next_frame.is_none());
        assert_eq!(
            parse_event(r#"{"type":"motion","reduced":true}"#),
            Some(Event::ReducedMotion(true))
        );
        assert!(parse_event(r#"{"type":"motion","reduced":"true"}"#).is_none());
    }
    #[test]
    fn snooze_expires_and_explicit_reenable_restores_idle_controls() {
        let now = Instant::now();
        let mut v = Visibility::default();
        v.update_preference(true, Phase::Idle);
        v.snoozed_until = Some(now + Duration::from_secs(3600));
        v.update_preference(true, Phase::Idle);
        assert!(v.snoozed(now));
        assert!(!v.snoozed(now + Duration::from_secs(3600)));
        v.update_preference(false, Phase::Idle);
        assert!(!v.enabled);
        v.update_preference(true, Phase::Idle);
        assert!(v.enabled);
        assert!(!v.snoozed(now));
    }
    #[test]
    fn changing_visibility_mid_session_never_removes_stop_or_introduces_a_new_surface() {
        for phase in [Phase::Recording, Phase::Transcribing] {
            let mut v = Visibility::default();
            v.update_preference(true, phase);
            assert!(!v.enabled);
            v.update_preference(true, Phase::Idle);
            v.update_preference(false, phase);
            assert!(v.enabled);
            v.update_preference(false, Phase::Idle);
            assert!(!v.enabled);
        }
    }
    #[test]
    fn ipc_is_small_typed_and_has_no_general_settings_authority() {
        assert_eq!(parse_event(r#"{"type":"ready"}"#), Some(Event::Ready));
        assert_eq!(
            parse_event(
                r#"{"type":"action","revision":"9007199254740993","view_revision":"7","action":"stop"}"#
            ),
            Some(Event::Command(9007199254740993, 7, Command::Stop))
        );
        for input in [
            r#"{"type":"save"}"#,
            r#"{"type":"ready","text":"x"}"#,
            r#"{"type":"action","revision":1,"action":"stop"}"#,
            r#"{"type":"action","revision":"1","action":"start_meeting"}"#,
            r#"{"type":"action","revision":"-1","action":"start"}"#,
            r#"{"type":"action","revision":"18446744073709551616","action":"start"}"#,
        ] {
            assert!(parse_event(input).is_none(), "{input}");
        }
        assert!(parse_event(&" ".repeat(257)).is_none());
        assert_eq!(
            parse_event(r#"{"type":"scale","ratio_milli":1270}"#),
            Some(Event::Scale(1270))
        );
        assert!(parse_event(r#"{"type":"scale","ratio_milli":0}"#).is_none());
    }
    #[test]
    fn text_enlargement_is_additional_to_monitor_dpi_not_a_replacement() {
        assert!((additional_text_scale(1270, 96) - 1.27).abs() < 0.001);
        assert!((additional_text_scale(2540, 192) - 1.27).abs() < 0.001);
        assert_eq!(additional_text_scale(2000, 192), 1.0);
        assert_eq!(additional_text_scale(1270, 192), 1.0);
    }
    #[test]
    fn all_edges_fit_work_area_on_negative_coordinate_and_scaled_monitors() {
        for work in [
            RECT {
                left: -2560,
                top: -900,
                right: 0,
                bottom: 480,
            },
            RECT {
                left: 0,
                top: 48,
                right: 1920,
                bottom: 1040,
            },
        ] {
            for scale in [1., 1.25, 1.5, 2., 3.] {
                for edge in ["left", "right", "bottom"] {
                    for surface in [
                        Surface::Compact,
                        Surface::Quick,
                        Surface::Menu,
                        Surface::Status,
                    ] {
                        let b = bounds(work, scale, edge, surface);
                        assert!(
                            b.x >= work.left
                                && b.y >= work.top
                                && b.x + b.width <= work.right
                                && b.y + b.height <= work.bottom
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn tiny_work_areas_and_invalid_scales_stay_bounded() {
        let work = RECT {
            left: -5,
            top: 10,
            right: 3,
            bottom: 13,
        };
        for scale in [f64::NAN, f64::INFINITY, -9., 0., 100.] {
            let b = bounds(work, scale, "left", Surface::Menu);
            assert_eq!(
                b,
                Bounds {
                    x: -5,
                    y: 10,
                    width: 8,
                    height: 3
                }
            );
        }
    }
    #[test]
    fn hover_expansion_preserves_the_peek_anchor_without_exposing_start_under_it() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        for edge in ["bottom", "left", "right"] {
            let a = bounds(work, 1.5, edge, Surface::Compact);
            let b = bounds(work, 1.5, edge, Surface::Quick);
            if edge == "right" {
                assert_eq!(a.x + a.width, b.x + b.width);
            } else if edge == "bottom" {
                assert_eq!(a.x + a.width / 2, b.x + b.width / 2);
            } else {
                assert_eq!(a.x, b.x);
            }
            assert!((a.y + a.height / 2 - (b.y + b.height - 27)).abs() <= 1);
        }
    }
    #[test]
    fn idle_capsule_is_tiny_but_active_controls_stay_usable() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let compact = bounds(work, 1.0, "bottom", Surface::Compact);
        let quick = bounds(work, 1.0, "bottom", Surface::Quick);
        let controls = bounds(work, 1.0, "bottom", Surface::Status);
        let menu = bounds(work, 1.0, "bottom", Surface::Menu);
        assert_eq!((compact.width, compact.height), (48, 18));
        assert_eq!((quick.width, quick.height), (160, 36));
        assert_eq!((controls.width, controls.height), (248, 40));
        assert_eq!((menu.width, menu.height), (160, 200));
    }
    /// The indicator yields to the capsule, but a notice still shows beside
    /// it, at the same bottom-centre spot. It must sit above every surface the
    /// capsule shows without a click, and above the menu while that is open.
    #[test]
    fn a_notice_beside_the_bottom_capsule_never_covers_it() {
        use crate::overlay::{origin_clear_of, NOTICE_MAX_W, NOTICE_MIN_W, WINDOW_H};
        for work in [
            RECT {
                left: 0,
                top: 48,
                right: 1920,
                bottom: 1040,
            },
            RECT {
                left: -2560,
                top: -900,
                right: 0,
                bottom: 480,
            },
        ] {
            let screen = ScreenRect {
                left: work.left,
                top: work.top,
                right: work.right,
                bottom: work.bottom,
            };
            for scale in [1.0_f64, 1.25, 1.5, 2.0, 3.0] {
                let margin = (8.0 * scale).round() as i32;
                let height = (WINDOW_H * scale).round() as i32;
                for drawn in [
                    Surface::Compact,
                    Surface::Quick,
                    Surface::Menu,
                    Surface::Status,
                ] {
                    let drawn = bounds(work, scale, "bottom", drawn);
                    let keep = keep_clear_area(work, scale, "bottom", drawn);
                    for logical_width in [NOTICE_MIN_W, 300.0, NOTICE_MAX_W] {
                        let width = (logical_width * scale).round() as i32;
                        let (x, y) = origin_clear_of(screen, width, height, margin, Some(keep));
                        let notice = Bounds {
                            x,
                            y,
                            width,
                            height,
                        }
                        .rect();
                        assert!(y >= work.top && x >= work.left);
                        assert!(!notice.intersects(drawn.rect()), "{scale} {drawn:?}");
                        for surface in [Surface::Compact, Surface::Quick, Surface::Status] {
                            let surface = bounds(work, scale, "bottom", surface).rect();
                            assert!(!notice.intersects(surface), "{scale} {surface:?}");
                        }
                        // Lifted only as far as it must be: one margin above.
                        assert_eq!(notice.bottom + margin, keep.top);
                    }
                }
            }
        }
    }
    #[test]
    fn a_notice_keeps_its_usual_spot_unless_the_capsule_is_under_it() {
        use crate::overlay::origin_clear_of;
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let screen = ScreenRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let usual = origin_clear_of(screen, 300, 34, 8, None);
        assert_eq!(usual, (810, 998));
        for edge in ["left", "right"] {
            let drawn = bounds(work, 1.0, edge, Surface::Menu);
            let keep = keep_clear_area(work, 1.0, edge, drawn);
            assert_eq!(origin_clear_of(screen, 300, 34, 8, Some(keep)), usual);
        }
        // A screen too short for both keeps the notice on it.
        let short = ScreenRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 60,
        };
        let keep = ScreenRect {
            left: 0,
            top: 10,
            right: 1920,
            bottom: 60,
        };
        assert_eq!(origin_clear_of(short, 300, 34, 8, Some(keep)), (810, 0));
    }
    #[test]
    fn leave_grace_does_not_flicker_or_extend_on_repeated_leave_messages() {
        let now = Instant::now();
        let mut hover = HoverIntent::default();
        assert!(!hover.expanded(now));
        hover.update(true, now);
        hover.update(false, now);
        assert!(hover.expanded(now + Duration::from_millis(299)));
        hover.update(false, now + Duration::from_millis(200));
        assert!(!hover.expanded(now + Duration::from_millis(300)));
        hover.update(true, now + Duration::from_millis(250));
        assert!(hover.expanded(now + Duration::from_secs(2)));
    }
    #[test]
    fn menu_navigation_has_no_implicit_recording_authority() {
        for (action, command) in [
            ("meetings", Command::Meetings),
            ("rewrite", Command::Rewrite),
        ] {
            assert_eq!(
                parse_event(&format!(
                    r#"{{"type":"action","revision":"1","view_revision":"7","action":"{action}"}}"#
                )),
                Some(Event::Command(1, 7, command))
            );
        }
        assert!(
            parse_event(r#"{"type":"action","revision":"1","action":"start_meeting"}"#).is_none()
        );
    }
    #[test]
    fn local_surface_has_no_remote_assets_or_editable_text_fields() {
        assert!(HTML.contains("connect-src 'none'"));
        assert!(!HTML.contains("<input"));
        assert!(!HTML.contains("<textarea"));
        assert!(HTML.contains("prefers-reduced-motion"));
    }
}
