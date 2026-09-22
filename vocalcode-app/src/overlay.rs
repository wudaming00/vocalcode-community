//! The on-screen recording indicator.
//!
//! Holding the talk key used to produce no visible feedback at all: the status
//! lived in the settings window, which is normally closed, so the user pressed
//! a key, spoke, and had no way to tell whether anything was listening. Every
//! comparable product solves this the same way, and their agreement is what
//! this follows.
//!
//! # Shape of the thing
//!
//! A small dark capsule near the bottom of the screen showing a live level
//! meter, and nothing else. Notably it does **not** show partial transcript —
//! none of the products surveyed do; live text is consistently treated as a
//! separate opt-in feature rather than a state of the indicator.
//!
//! # Why it is inert
//!
//! The window ignores mouse events entirely. That is not only simplicity: this
//! app types into whatever window has focus, so an overlay that could take
//! focus would send the transcript to itself. Ignoring input makes that
//! impossible rather than merely unlikely.
//!
//! It also has to float above the menu bar and appear on every Space including
//! over full-screen apps, since dictation happens wherever the user already is.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder};
use wry::WebView;

/// Window size in logical pixels. On macOS the window is transparent and larger
/// than the capsule, leaving room for the drop shadow. On Windows WebView2 does
/// not composite a transparent window (it paints white), so there the window is
/// sized to *hug* an opaque dark capsule instead — see the `solid` mode below.
#[cfg(not(windows))]
const WINDOW_W: f64 = 260.0;
#[cfg(not(windows))]
const WINDOW_H: f64 = 64.0;
#[cfg(windows)]
const WINDOW_W: f64 = 172.0;
#[cfg(windows)]
const WINDOW_H: f64 = 34.0;
#[cfg(windows)]
const MINI_WINDOW_W: f64 = 64.0;
#[cfg(windows)]
const MINI_WINDOW_H: f64 = 22.0;
/// Gap between the capsule and the bottom of the work area.
const BOTTOM_MARGIN: f64 = 8.0;

/// What the indicator is currently showing.
///
/// Deliberately smaller than the state machines the surveyed apps use — they
/// carry modes, polish passes and hands-free locks that this does not have.
/// Adding a state it cannot actually reach would just be dead UI.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    /// Nothing happening: the window is hidden entirely.
    Idle = 0,
    /// Capturing audio; the meter is live.
    Recording = 1,
    /// Key released, transcribing. The meter is replaced by a spinner.
    Transcribing = 2,
    /// The user changed the just-inserted phrase; the correction window is
    /// paused until that input is sent or abandoned.
    Learning = 3,
}

impl Phase {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Phase::Recording,
            2 => Phase::Transcribing,
            3 => Phase::Learning,
            _ => Phase::Idle,
        }
    }
}

/// Shared handle the engine thread writes and the UI thread reads.
#[derive(Clone, Default)]
pub struct OverlayState(Arc<AtomicU8>);

impl OverlayState {
    pub fn set(&self, phase: Phase) {
        self.0.store(phase as u8, Ordering::Relaxed);
    }
    fn get(&self) -> Phase {
        Phase::from_u8(self.0.load(Ordering::Relaxed))
    }
}

/// Which indicator the user asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// The full capsule: meter, on-air dot, and a label while transcribing.
    Classic,
    /// A small lozenge with the meter alone.
    Mini,
    /// No indicator at all.
    Off,
}

impl Style {
    /// Parse the config string, falling back to the default rather than
    /// failing: a hand-edited config with a typo should still show *something*
    /// rather than silently leaving the user with no feedback at all.
    pub fn parse(s: &str) -> Self {
        match s {
            "mini" => Style::Mini,
            "off" => Style::Off,
            _ => Style::Classic,
        }
    }
}

#[cfg(windows)]
fn window_logical_size(style: Style) -> (f64, f64) {
    if style == Style::Mini {
        return (MINI_WINDOW_W, MINI_WINDOW_H);
    }

    (WINDOW_W, WINDOW_H)
}

#[cfg(not(windows))]
fn window_logical_size(_: Style) -> (f64, f64) {
    (WINDOW_W, WINDOW_H)
}

pub struct Overlay {
    // Field order is intentional: Rust drops struct fields in declaration
    // order, so the WebView must release its browser objects before the native
    // window and the WebContext that owns its profile.
    webview: WebView,
    #[cfg(target_os = "macos")]
    panel: MacPanel,
    window: Window,
    _web_context: wry::WebContext,
    shown: bool,
    last_phase: Phase,
    style: Style,
    /// The style changed and the page has not been told yet.
    style_pending: bool,
    /// The interface language preference, and whether the page has it yet.
    lang: String,
    lang_pending: bool,
}

impl Overlay {
    pub fn new<T>(
        target: &EventLoopWindowTarget<T>,
        data_directory: std::path::PathBuf,
    ) -> anyhow::Result<Self> {
        let window = WindowBuilder::new()
            .with_title("VocalCode Indicator")
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top(true)
            .with_resizable(false)
            .with_focused(false)
            .with_visible(false)
            .with_inner_size(tao::dpi::LogicalSize::new(WINDOW_W, WINDOW_H))
            .build(target)?;

        position_bottom_centre(&window, Style::Classic);

        // macOS composites a transparent WebView2/WKWebView, so the capsule can
        // float with a shadow. Windows WebView2 does not — a transparent window
        // paints white — so there the webview is opaque dark and the page runs
        // in `solid` mode: the capsule fills the (capsule-sized) window, so what
        // shows is the dark pill itself, never a white box around it.
        let mut web_context = wry::WebContext::new(Some(data_directory));
        #[cfg(not(windows))]
        let webview = wry::WebViewBuilder::new_with_web_context(&mut web_context)
            .with_html(include_str!("overlay.html"))
            .with_transparent(true)
            .build(&window)?;
        #[cfg(windows)]
        let webview = wry::WebViewBuilder::new_with_web_context(&mut web_context)
            .with_html(include_str!("overlay.html"))
            .with_background_color((13, 12, 11, 255))
            .build(&window)?;
        #[cfg(windows)]
        let _ = webview.evaluate_script("document.documentElement.classList.add('solid')");

        #[cfg(target_os = "macos")]
        let panel = {
            use wry::WebViewExtMacOS;

            let panel = MacPanel::new(&window)?;
            webview.reparent(panel.as_ns_window())?;
            make_inert_and_floating(&panel);
            // Tao still owns an empty bookkeeping window used by its monitor
            // APIs. Only the native non-activating panel is ever displayed.
            window.set_visible(false);
            panel
        };

        Ok(Self {
            webview,
            #[cfg(target_os = "macos")]
            panel,
            window,
            _web_context: web_context,
            shown: false,
            last_phase: Phase::Idle,
            style: Style::Classic,
            style_pending: false,
            lang: String::new(),
            lang_pending: false,
        })
    }

    /// Apply the user's choice. Safe to call on every tick; the page is only
    /// told when the value actually changed.
    pub fn set_style(&mut self, style: Style) {
        if style == self.style {
            return;
        }
        self.style = style;
        #[cfg(windows)]
        if style != Style::Off {
            let (width, height) = window_logical_size(style);
            self.window
                .set_inner_size(tao::dpi::LogicalSize::new(width, height));
            // A live switch between Classic and Mini does not cross the
            // hidden/visible boundary below, so reposition at the same time as
            // the native resize instead of leaving the smaller capsule offset.
            position_bottom_centre(&self.window, style);
        }
        // Deferred rather than pushed here. At startup this runs a few
        // milliseconds after the webview is built, and `vcStyle` does not exist
        // until the document has loaded — a push now can land on nothing and be
        // lost, leaving a user who chose Mini with the classic capsule until
        // the next restart. Sending it just before the indicator is shown
        // costs one script call per style change and cannot race.
        self.style_pending = true;
        // Switching to Off while the indicator is up must take it down now,
        // not at the end of the current utterance.
        if style == Style::Off && self.shown {
            #[cfg(not(target_os = "macos"))]
            self.window.set_visible(false);
            #[cfg(target_os = "macos")]
            self.panel.set_visible(false);
            self.shown = false;
        }
    }

    /// Tell the indicator which language to label itself in.
    ///
    /// Passed through as the raw preference, "auto" included: resolving it
    /// needs the browser locale, which only the page can see.
    pub fn set_lang(&mut self, pref: &str) {
        if pref == self.lang {
            return;
        }
        self.lang = pref.to_string();
        // Deferred for the same reason as the style: sent just before the
        // indicator is shown, when the document is certainly parsed.
        self.lang_pending = true;
    }

    /// Push the current phase and microphone level into the page.
    ///
    /// Called from the UI event loop's tick. The level is only a target: the
    /// page animates continuously on its own, so a coarse update rate here
    /// still yields a smooth meter.
    pub fn tick(&mut self, state: &OverlayState, level: f32) {
        let phase = state.get();

        if phase != self.last_phase {
            let name = match phase {
                Phase::Idle => "idle",
                Phase::Recording => "recording",
                Phase::Transcribing => "transcribing",
                Phase::Learning => "learning",
            };
            let _ = self
                .webview
                .evaluate_script(&format!("window.vcPhase && vcPhase('{name}')"));
            self.last_phase = phase;
        }

        // Hidden while idle rather than left on screen as a resting sliver.
        // A permanent mark is a real cost on a small laptop display, and this
        // indicator has no hover affordance that would justify one.
        let want = phase != Phase::Idle && self.style != Style::Off;
        if want != self.shown {
            if want {
                if self.lang_pending {
                    // Config files are user-editable. Encode the value as a JS
                    // string literal instead of trying to sanitize individual
                    // quote characters before evaluating it in the WebView.
                    let _ = self
                        .webview
                        .evaluate_script(&overlay_language_script(&self.lang));
                    self.lang_pending = false;
                }
                if self.style_pending {
                    let name = match self.style {
                        Style::Mini => "mini",
                        _ => "classic",
                    };
                    let _ = self
                        .webview
                        .evaluate_script(&format!("window.vcStyle && vcStyle('{name}')"));
                    self.style_pending = false;
                }
                position_bottom_centre(&self.window, self.style);
                #[cfg(target_os = "macos")]
                self.panel.sync_frame_from(&self.window);
            }
            #[cfg(not(target_os = "macos"))]
            self.window.set_visible(want);
            #[cfg(target_os = "macos")]
            self.panel.set_visible(want);
            #[cfg(target_os = "macos")]
            if want {
                order_front_without_activating(&self.panel);
            }
            if want {
                // A hidden WKWebView is suspended while VocalCode is not the
                // active application. The phase script above can therefore be
                // accepted without producing a frame, leaving the newly shown
                // window in the transparent idle state. Re-apply the phase
                // after ordering the native window, when WebKit is visible and
                // must render it.
                let name = match phase {
                    Phase::Recording => "recording",
                    Phase::Transcribing => "transcribing",
                    Phase::Learning => "learning",
                    Phase::Idle => "idle",
                };
                let _ = self
                    .webview
                    .evaluate_script(&format!("window.vcPhase && vcPhase('{name}')"));
            }
            self.shown = want;
        }

        if phase == Phase::Recording {
            let _ = self
                .webview
                .evaluate_script(&format!("window.vcLevel && vcLevel({level:.3})"));
        }
    }
}

/// Re-order an already visible indicator on every show.
///
/// `set_visible(true)` maps to AppKit's ordinary ordering path. On macOS 26 a
/// non-activating, all-Spaces transparent window can remain ordered behind the
/// current Space even though `isVisible` is true. `orderFrontRegardless` is the
/// native operation intended for passive status windows: it raises the window
/// without activating VocalCode or taking the editor's keyboard focus.
#[cfg(target_os = "macos")]
fn order_front_without_activating(panel: &MacPanel) {
    if panel.0.is_null() {
        log::warn!("no NSPanel while showing the recording indicator");
        return;
    }
    unsafe {
        let _: () = objc2::msg_send![panel.0, orderFrontRegardless];
    }
}

fn overlay_language_script(lang: &str) -> String {
    let lang = serde_json::to_string(lang).expect("serializing a Rust string cannot fail");
    format!("window.vcLang && vcLang({lang})")
}

/// Centre the window horizontally and sit it just above the bottom of the
/// **work area** — the screen minus the Dock and menu bar.
///
/// Using the full monitor bounds instead puts the indicator underneath the
/// Dock, where it both looks wrong and covers something the user clicks.
///
/// The screen this lands on is the one the user is *working* on, not the one
/// the indicator happens to already be on. The distinction is the whole bug:
/// asking `current_monitor()` means asking the overlay where it is, so it
/// positions relative to itself and can never move. It was placed once at
/// creation and stayed there for the life of the process, which on a
/// multi-monitor desk means the only signal that a recording is live sits on a
/// screen the user is not looking at.
fn position_bottom_centre(window: &Window, style: Style) {
    #[cfg(target_os = "macos")]
    if position_on_focused_screen(window) {
        return;
    }

    #[cfg(target_os = "windows")]
    if position_on_windows_work_area(window, style) {
        return;
    }

    let monitor = window
        .current_monitor()
        .or_else(|| window.primary_monitor());
    let Some(m) = monitor else { return };

    let scale = m.scale_factor();
    let pos = m.position().to_logical::<f64>(scale);
    let size = m.size().to_logical::<f64>(scale);

    // How much of the bottom of the screen the Dock occupies. tao has no
    // work-area API, so on macOS this comes from NSScreen; elsewhere the
    // full height is used, which is correct for a bottom-docked taskbar only
    // by accident and is refined per-platform when those are supported.
    let bottom_inset = bottom_inset_of_current_screen(window);

    let (window_width, window_height) = window_logical_size(style);
    window.set_outer_position(tao::dpi::LogicalPosition::new(
        pos.x + (size.width - window_width) / 2.0,
        pos.y + size.height - bottom_inset - window_height - BOTTOM_MARGIN,
    ));
}

// Declared here rather than pulling in objc2-app-kit for a handful of getters.
// `Encode` is what lets msg_send! return them: the runtime needs the type
// encoding to know how the struct comes back.
#[cfg(target_os = "macos")]
mod cocoa {
    use objc2::encode::{Encode, Encoding};

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct NSPoint {
        pub x: f64,
        pub y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct NSSize {
        pub width: f64,
        pub height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct NSRect {
        pub origin: NSPoint,
        pub size: NSSize,
    }

    unsafe impl Encode for NSPoint {
        const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
    }
    unsafe impl Encode for NSSize {
        const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
    }
    unsafe impl Encode for NSRect {
        const ENCODING: Encoding =
            Encoding::Struct("CGRect", &[NSPoint::ENCODING, NSSize::ENCODING]);
    }
}

#[cfg(target_os = "macos")]
struct MacPanel(*mut objc2::runtime::AnyObject);

#[cfg(target_os = "macos")]
impl MacPanel {
    fn new(window: &Window) -> anyhow::Result<Self> {
        use cocoa::NSRect;
        use objc2::runtime::AnyObject;
        use tao::platform::macos::WindowExtMacOS;

        const NONACTIVATING_PANEL: u64 = 1 << 7;
        const BACKING_STORE_BUFFERED: u64 = 2;

        let source = window.ns_window() as *mut AnyObject;
        if source.is_null() {
            anyhow::bail!("Tao did not provide an NSWindow for the indicator");
        }
        unsafe {
            let frame: NSRect = objc2::msg_send![source, frame];
            let class = objc2::class!(NSPanel);
            let allocated: *mut AnyObject = objc2::msg_send![class, alloc];
            if allocated.is_null() {
                anyhow::bail!("could not allocate the recording NSPanel");
            }
            let panel: *mut AnyObject = objc2::msg_send![
                allocated,
                initWithContentRect: frame,
                styleMask: NONACTIVATING_PANEL,
                backing: BACKING_STORE_BUFFERED,
                defer: false
            ];
            if panel.is_null() {
                anyhow::bail!("could not initialise the recording NSPanel");
            }
            let _: () = objc2::msg_send![panel, setFloatingPanel: true];
            let _: () = objc2::msg_send![panel, setBecomesKeyOnlyIfNeeded: true];
            // `close` must only order the panel out. This object is retained by
            // `alloc` above and released exactly once by `Drop` below.
            let _: () = objc2::msg_send![panel, setReleasedWhenClosed: false];
            let _: () = objc2::msg_send![panel, setOpaque: false];
            let color_class = objc2::class!(NSColor);
            let clear: *mut AnyObject = objc2::msg_send![color_class, clearColor];
            let _: () = objc2::msg_send![panel, setBackgroundColor: clear];
            let _: () = objc2::msg_send![panel, setHasShadow: false];
            Ok(Self(panel))
        }
    }

    fn as_ns_window(&self) -> *mut objc2_app_kit::NSWindow {
        self.0.cast()
    }

    fn sync_frame_from(&self, window: &Window) {
        use cocoa::NSRect;
        use objc2::runtime::AnyObject;
        use tao::platform::macos::WindowExtMacOS;

        let source = window.ns_window() as *mut AnyObject;
        if source.is_null() {
            return;
        }
        unsafe {
            let frame: NSRect = objc2::msg_send![source, frame];
            let _: () = objc2::msg_send![self.0, setFrame: frame, display: false];
        }
    }

    fn set_visible(&self, visible: bool) {
        unsafe {
            let _: () = objc2::msg_send![self.0, setIsVisible: visible];
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for MacPanel {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _: () = objc2::msg_send![self.0, close];
                let _: () = objc2::msg_send![self.0, release];
            }
        }
    }
}

/// Put the indicator on the screen that currently owns the keyboard, and report
/// whether that succeeded.
///
/// `NSScreen.mainScreen` is defined by Apple as the screen containing the window
/// with keyboard focus, which is exactly the question being asked — where is the
/// user typing, and therefore where will the dictated text land. The overlay
/// never takes focus (`with_focused(false)`, non-activating), so showing it
/// cannot move the answer.
///
/// `visibleFrame` already excludes the Dock and the menu bar, so this also
/// replaces the separate inset calculation the tao path still needs.
///
/// **The coordinate systems are opposite and this is the only place it shows.**
/// Cocoa measures y upward from the bottom-left of the primary screen; tao
/// measures it downward from that screen's top-left. On a single display the
/// conversion still has to be right, but it cannot be *wrong* in a way anyone
/// notices — the error is proportional to the offset between screens, and with
/// one screen that offset is zero. So a single-display check proves nothing
/// about the multi-display case here.
///
/// Returns false rather than guessing if AppKit will not answer, leaving the
/// caller's monitor-based path to do what it did before.
#[cfg(target_os = "macos")]
fn position_on_focused_screen(window: &Window) -> bool {
    use cocoa::NSRect;
    use objc2::runtime::AnyObject;

    unsafe {
        let class = objc2::class!(NSScreen);
        let focused: *mut AnyObject = objc2::msg_send![class, mainScreen];
        if focused.is_null() {
            return false;
        }
        // `screens[0]` is the screen with the menu bar, and Cocoa's global
        // origin is its bottom-left corner. Its height is therefore the only
        // number needed to flip into tao's top-left space, whatever the other
        // displays are doing.
        let screens: *mut AnyObject = objc2::msg_send![class, screens];
        if screens.is_null() {
            return false;
        }
        let count: usize = objc2::msg_send![screens, count];
        if count == 0 {
            return false;
        }
        let primary: *mut AnyObject = objc2::msg_send![screens, objectAtIndex: 0usize];
        if primary.is_null() {
            return false;
        }
        let primary_frame: NSRect = objc2::msg_send![primary, frame];
        let work: NSRect = objc2::msg_send![focused, visibleFrame];

        let x = work.origin.x + (work.size.width - WINDOW_W) / 2.0;
        // `work.origin.y` is the top of the Dock in Cocoa terms; the window sits
        // BOTTOM_MARGIN above it, and flipping measures its top edge down from
        // the primary screen's top.
        let y = primary_frame.size.height - work.origin.y - BOTTOM_MARGIN - WINDOW_H;
        window.set_outer_position(tao::dpi::LogicalPosition::new(x, y));
        true
    }
}

/// Height of the Dock (or whatever else the system reserves) at the bottom of
/// the screen the window is on, in logical pixels.
///
/// Only reached when [`position_on_focused_screen`] declined to answer.
#[cfg(target_os = "macos")]
fn bottom_inset_of_current_screen(window: &Window) -> f64 {
    use cocoa::NSRect;
    use objc2::runtime::AnyObject;
    use tao::platform::macos::WindowExtMacOS;

    let ns_window = window.ns_window() as *mut AnyObject;
    if ns_window.is_null() {
        return 0.0;
    }
    unsafe {
        let screen: *mut AnyObject = objc2::msg_send![ns_window, screen];
        if screen.is_null() {
            return 0.0;
        }
        let frame: NSRect = objc2::msg_send![screen, frame];
        let visible: NSRect = objc2::msg_send![screen, visibleFrame];
        // Cocoa's origin is bottom-left, so the gap between the two origins is
        // exactly what is reserved at the bottom.
        (visible.origin.y - frame.origin.y).max(0.0)
    }
}

#[cfg(not(target_os = "macos"))]
fn bottom_inset_of_current_screen(_window: &Window) -> f64 {
    0.0
}

/// Raise the window above the menu bar, make it click-through, and let it
/// follow the user onto other Spaces and full-screen apps.
///
/// `tao`'s own always-on-top only reaches the floating window level, which is
/// still below the menu bar, and it has no API for either click-through or the
/// collection behaviour — so this goes through the underlying `NSWindow`.
#[cfg(target_os = "macos")]
fn make_inert_and_floating(panel: &MacPanel) {
    // Apple's full-screen Space compositor sits above floating and status-bar
    // windows. A passive cross-application overlay needs the screen-saver
    // level to remain visible over another app's full-screen content.
    const SCREEN_SAVER_WINDOW_LEVEL: i64 = 1000;
    // NSWindowCollectionBehaviorCanJoinAllSpaces | FullScreenAuxiliary |
    // Stationary | CanJoinAllApplications. The last flag is the macOS 13+
    // authority for a system overlay to join another application's full-screen
    // Space; the older FullScreenAuxiliary flag alone only associates windows
    // with their own application's full-screen window.
    const CAN_JOIN_ALL_SPACES: u64 = 1 << 0;
    const STATIONARY: u64 = 1 << 4;
    const FULL_SCREEN_AUXILIARY: u64 = 1 << 8;
    const CAN_JOIN_ALL_APPLICATIONS: u64 = 1 << 18;

    let ns_window = panel.0;
    if ns_window.is_null() {
        log::warn!("no NSPanel for the indicator; it may sit below the menu bar");
        return;
    }
    unsafe {
        let _: () = objc2::msg_send![ns_window, setLevel: SCREEN_SAVER_WINDOW_LEVEL];
        // The load-bearing line: without it the overlay can take focus, and
        // this app types into whatever is focused — the transcript would go
        // into the indicator instead of the user's editor.
        let _: () = objc2::msg_send![ns_window, setIgnoresMouseEvents: true];
        let _: () = objc2::msg_send![
            ns_window,
            setCollectionBehavior: CAN_JOIN_ALL_SPACES
                | STATIONARY
                | FULL_SCREEN_AUXILIARY
                | CAN_JOIN_ALL_APPLICATIONS
        ];
        // Hidden from Mission Control and the window cycle: it is a readout,
        // not a window anyone would want to switch to.
        let _: () = objc2::msg_send![ns_window, setHidesOnDeactivate: false];
        // No native window shadow — the page draws its own.
        //
        // macOS derives a transparent window's shadow from a snapshot of its
        // alpha, and does not recompute it when the content changes. The capsule
        // animates in (scale .96 → 1, translateY 6px → 0, fading up), so the
        // snapshot caught it mid-entrance and then stayed: a second, larger,
        // slightly offset rounded shape sitting behind the capsule with its lower
        // half showing — visible ever since the window became genuinely
        // transparent, because before that the opaque white background hid it.
        //
        // Invalidating the shadow on every show would also work and would still
        // be a race against the animation. Since `.pill` already carries a
        // box-shadow, the native one is duplicate work that can only ever be out
        // of date, so it goes.
        let _: () = objc2::msg_send![ns_window, setHasShadow: false];
    }
}

/// Put the indicator inside the work area of the monitor that owns the
/// foreground window.
///
/// `MonitorHandle::size()` is the full display rectangle on Windows, including
/// the taskbar.  A bottom-docked taskbar can therefore cover this entire 34 px
/// indicator.  Win32's `rcWork` is the authoritative rectangle after taskbars
/// and other app bars have been removed, and is already expressed in physical
/// desktop coordinates.  Keep the whole calculation in those coordinates so
/// mixed-DPI displays do not combine logical sizes with physical offsets.
#[cfg(target_os = "windows")]
fn position_on_windows_work_area(window: &Window, style: Style) -> bool {
    use std::mem::size_of;

    use tao::platform::windows::WindowExtWindows as _;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    let overlay_hwnd = window.hwnd() as HWND;
    let foreground = unsafe { GetForegroundWindow() };
    let anchor = if foreground.is_null() {
        overlay_hwnd
    } else {
        foreground
    };
    let monitor = unsafe { MonitorFromWindow(anchor, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return false;
    }

    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..MONITORINFO::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        log::warn!("could not read the Windows work area for the recording indicator");
        return false;
    }

    let mut dpi_x = 0;
    let mut dpi_y = 0;
    let dpi_result =
        unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    let scale = if dpi_result < 0 || dpi_x == 0 {
        window.scale_factor()
    } else {
        f64::from(dpi_x) / 96.0
    };
    // DWM can add an invisible outer border even to an undecorated WebView2
    // window.  Its live outer size is therefore the final authority; the
    // target-monitor DPI estimate protects the first move between monitors
    // whose scales differ.
    let measured = window.outer_size();
    let (window_width, window_height) = window_logical_size(style);
    let width = ((window_width * scale).round().max(1.0) as i32)
        .max(i32::try_from(measured.width).unwrap_or(i32::MAX));
    let height = ((window_height * scale).round().max(1.0) as i32)
        .max(i32::try_from(measured.height).unwrap_or(i32::MAX));
    let margin = (BOTTOM_MARGIN * scale).round().max(0.0) as i32;
    let (x, y) = bottom_centre_in_work_area(
        info.rcWork.left,
        info.rcWork.top,
        info.rcWork.right,
        info.rcWork.bottom,
        width,
        height,
        margin,
    );
    window.set_outer_position(tao::dpi::PhysicalPosition::new(x, y));
    true
}

/// Return a position whose complete window rectangle remains inside `rcWork`.
/// Saturating arithmetic also leaves a deterministic fallback for pathological
/// work areas smaller than the indicator instead of wrapping off-screen.
#[cfg(target_os = "windows")]
fn bottom_centre_in_work_area(
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    width: i32,
    height: i32,
    margin: i32,
) -> (i32, i32) {
    let width = width.max(1);
    let height = height.max(1);
    let work_width = right.saturating_sub(left).max(0);
    let max_x = right.saturating_sub(width).max(left);
    let max_y = bottom.saturating_sub(height).max(top);
    let centred_x = left.saturating_add(work_width.saturating_sub(width).max(0) / 2);
    let desired_y = bottom.saturating_sub(height).saturating_sub(margin.max(0));
    (centred_x.clamp(left, max_x), desired_y.clamp(top, max_y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vocalcode_core::Config;

    /// The three values the picker can write must all survive the round trip
    /// from the page, through the config file, back to a `Style`.
    #[test]
    fn every_offered_style_parses() {
        assert!(matches!(Style::parse("classic"), Style::Classic));
        assert!(matches!(Style::parse("mini"), Style::Mini));
        assert!(matches!(Style::parse("off"), Style::Off));
    }

    /// A config that predates the setting, or one hand-edited to nonsense, must
    /// leave the user with a working indicator rather than none — silence here
    /// is indistinguishable from the app being broken.
    #[test]
    fn anything_else_falls_back_to_the_full_indicator() {
        assert!(matches!(Style::parse(""), Style::Classic));
        assert!(matches!(Style::parse("Mini"), Style::Classic));
        assert!(matches!(Style::parse("disabled"), Style::Classic));
    }

    /// The default must be the visible one: a new user who has never opened
    /// Behaviour still needs to see that the app is listening.
    #[test]
    fn the_default_config_shows_the_indicator() {
        assert!(matches!(
            Style::parse(&Config::default().overlay_style),
            Style::Classic
        ));
    }

    #[test]
    fn language_value_is_encoded_as_json_before_script_evaluation() {
        let script = overlay_language_script("en');globalThis.injected=true;//\n\\");
        assert_eq!(
            script,
            "window.vcLang && vcLang(\"en');globalThis.injected=true;//\\n\\\\\")"
        );
        assert_eq!(script.matches("vcLang(").count(), 1);
    }

    #[test]
    fn meter_interpolates_at_the_display_refresh_rate() {
        let html = include_str!("overlay.html");
        assert!(html.contains("requestAnimationFrame(renderMeter)"));
        assert!(html.contains("Math.exp(-dt / tau)"));
        assert!(html.contains("runMeter(recording)"));
        assert!(!html.contains("shown += (target - shown) * (target > shown"));
    }

    #[test]
    fn correction_learning_has_a_distinct_indicator_phase() {
        assert!(matches!(Phase::from_u8(3), Phase::Learning));
        let html = include_str!("overlay.html");
        assert!(html.contains("phase === \"learning\""));
        assert!(html.contains("class=\"learnmark hide\""));
        assert!(html.contains("Learning…"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_mini_style_shrinks_both_native_and_web_content() {
        assert_eq!(window_logical_size(Style::Classic), (172.0, 34.0));
        assert_eq!(window_logical_size(Style::Mini), (64.0, 22.0));
        let html = include_str!("overlay.html");
        assert!(html.contains("html.solid.mini .pill"));
        assert!(html.contains("box-sizing:border-box"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_indicator_stays_above_a_bottom_taskbar() {
        let (x, y) = bottom_centre_in_work_area(0, 0, 5120, 1392, 188, 43, 9);
        assert_eq!((x, y), (2466, 1340));
        assert!(y + 43 <= 1392);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_indicator_uses_the_remaining_area_beside_a_side_taskbar() {
        let (x, y) = bottom_centre_in_work_area(80, 0, 1920, 1080, 172, 34, 8);
        assert_eq!((x, y), (914, 1038));
        assert!(x >= 80);
        assert!(x + 172 <= 1920);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_indicator_handles_negative_monitor_coordinates() {
        let (x, y) = bottom_centre_in_work_area(-1920, -120, 0, 920, 172, 34, 8);
        assert_eq!((x, y), (-1046, 878));
        assert!(x >= -1920 && x + 172 <= 0);
        assert!(y >= -120 && y + 34 <= 920);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_indicator_clamps_pathological_work_areas_without_overflow() {
        assert_eq!(
            bottom_centre_in_work_area(i32::MAX - 10, i32::MAX - 5, i32::MAX, i32::MAX, 172, 34, 8,),
            (i32::MAX - 10, i32::MAX - 5)
        );
    }
}
