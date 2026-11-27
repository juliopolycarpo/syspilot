//! The companion's window: plain Win32, drawn with Direct2D (see
//! [`crate::paint`]) on Mica, in the app's theme.
//!
//! It is one window for the whole update: the progress, then, in the same
//! place, a failure with the step it stopped at. A timer in the window's
//! thread ticks the [`Follower`] and redraws when something changed.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use syspilot_proto::update::CompanionTheme;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMSBT_MAINWINDOW, DWMSBT_NONE, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint, PAINTSTRUCT,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::UiaRootObjectId;
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB,
};
use windows::Win32::UI::Shell::{
    ITaskbarList3, TBPF_ERROR, TBPF_NOPROGRESS, TBPF_NORMAL, TaskbarList,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, ChangeWindowMessageFilterEx,
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetCursorPos,
    GetMessageW, GetWindowLongPtrW, IDC_ARROW, IsWindowVisible, KillTimer, LoadCursorW, LoadIconW,
    MSG, MSGFLT_ALLOW, RegisterClassW, RegisterWindowMessageW, SW_HIDE, SW_SHOWNORMAL,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE,
    WM_DPICHANGED, WM_DWMCOLORIZATIONCOLORCHANGED, WM_ERASEBKGND, WM_GETOBJECT, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_PAINT, WM_SETTINGCHANGE, WM_SIZE,
    WM_SYSCOLORCHANGE, WM_THEMECHANGED, WM_TIMER, WNDCLASSW, WS_CAPTION, WS_OVERLAPPED, WS_SYSMENU,
};
use windows::core::{BOOL, PCWSTR, Result, w};

use crate::access::{self, Access, Event, INVOKE_MESSAGE, View};
use crate::follow::{Follower, Tick};
use crate::layout::{Layout, lay_out};
use crate::paint::{Frame, Painter};
use crate::progress::BAR_END;
use crate::screen::{Command, Screen};
use crate::surface::{Choice, ProgressEnd, Surface};
use crate::system;
use crate::theme::{Backdrop, Look, Palette, Scheme, backdrop, wanted_backdrop};
use crate::watch::{Phase, Step};

/// The window's title, as the task dialog had it.
const TITLE: PCWSTR = w!("SysPilot update");
const CLASS: PCWSTR = w!("SysPilotCompanion");
/// The icon resource the build embeds: the app's.
const ICON_RESOURCE: usize = 1;
/// A dialog's frame: a title and Close, nothing to resize or maximise.
const STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0);
const EX_STYLE: WINDOW_EX_STYLE = WINDOW_EX_STYLE(0);
/// The timer that ticks the follower and turns the spinner.
const TICK_TIMER: usize = 1;
/// About 30 frames a second: smooth for a bar and a spinner, light on the
/// processor.
const TICK_MS: u32 = 33;
/// One turn of the spinner, in seconds.
const SPIN_SECONDS: f32 = 1.2;

/// What the window is doing.
enum Mode {
    /// Not shown yet, or hidden.
    Idle,
    /// Following an update, until `end` is set.
    Following { shared: Arc<Mutex<Phase>>, follower: Follower, end: Option<ProgressEnd> },
    /// Telling of a failure, until the user picks.
    Failing { choice: Option<Choice> },
}

/// The taskbar button, which shows the bar too.
struct Taskbar {
    list: ITaskbarList3,
}

impl Taskbar {
    fn new() -> Option<Self> {
        // SAFETY: an in-process class registered by Windows; COM is
        // initialised on this thread.
        let list: ITaskbarList3 =
            unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER) }.ok()?;
        // SAFETY: a live list, initialised once before use.
        unsafe { list.HrInit() }.ok()?;
        Some(Self { list })
    }

    fn show(&self, window: HWND, screen: &Screen) {
        let (state, position) = match screen.bar {
            Some(bar) if bar.failed => (TBPF_ERROR, bar.position),
            Some(bar) => (TBPF_NORMAL, bar.position),
            None => (TBPF_NOPROGRESS, 0),
        };
        // SAFETY: `window` is this live window, whose button the taskbar
        // made.
        _ = unsafe { self.list.SetProgressState(window, state) };
        if state != TBPF_NOPROGRESS {
            // SAFETY: as above.
            _ = unsafe {
                self.list.SetProgressValue(window, u64::from(position), u64::from(BAR_END))
            };
        }
    }
}

/// Everything the window procedure works with.
struct State {
    theme: CompanionTheme,
    painter: Painter,
    palette: Palette,
    backdrop: Backdrop,
    /// Whether Windows animates; the spinner stands still when it does not.
    animate: bool,
    /// What is on show; `None` before anything was.
    screen: Option<Screen>,
    layout: Layout,
    hover: Option<usize>,
    pressed: Option<usize>,
    /// The button the keyboard is on, and whether its ring shows: only
    /// once the keyboard was used, as in any Windows dialog.
    focus: usize,
    focus_visible: bool,
    tracking_mouse: bool,
    mode: Mode,
    started: Instant,
    taskbar: Option<Taskbar>,
    /// The message the taskbar sends once the window's button exists.
    taskbar_created: u32,
    /// What screen readers are given.
    access: Access,
    /// What to tell them once the state is free again: raising an event
    /// can call back into the providers.
    pending: Vec<Event>,
}

/// See the module documentation.
pub struct Window {
    handle: HWND,
    /// Boxed, so that its address, which the window keeps, never moves.
    state: Box<RefCell<State>>,
}

fn dpi_of(window: HWND) -> u32 {
    // SAFETY: `window` is a live window.
    match unsafe { GetDpiForWindow(window) } {
        0 => 96,
        dpi => dpi,
    }
}

/// The window's outer size for content `width` × `height` DIPs at `dpi`.
fn outer_size(width: f32, height: f32, dpi: u32) -> (i32, i32) {
    let scale = dpi as f32 / 96.0;
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: (width * scale).ceil() as i32,
        bottom: (height * scale).ceil() as i32,
    };
    // SAFETY: `rect` is a valid in/out rectangle.
    _ = unsafe { AdjustWindowRectExForDpi(&mut rect, STYLE, false, EX_STYLE, dpi) };
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// Where a window of `layout` opens: in the middle of the work area of the
/// screen the pointer is on, where the user just clicked Update now.
fn placement(layout: &Layout) -> (i32, i32, i32, i32) {
    let mut cursor = POINT::default();
    // SAFETY: `cursor` is a valid out-pointer.
    _ = unsafe { GetCursorPos(&mut cursor) };
    // SAFETY: always returns a monitor with this flag.
    let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: `info` is sized for the call.
    let known = unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool();
    let (mut dpi_x, mut dpi_y) = (96, 96);
    // SAFETY: valid out-pointers.
    _ = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    let (width, height) = outer_size(layout.width, layout.height, dpi_x);
    if !known {
        return (CW_USEDEFAULT, CW_USEDEFAULT, width, height);
    }
    let work = info.rcWork;
    let x = work.left + (work.right - work.left - width) / 2;
    let y = work.top + (work.bottom - work.top - height) / 2;
    (x, y, width, height)
}

impl State {
    /// Applies the look Windows has now: colours, backdrop, title bar.
    fn apply_look(&mut self, window: HWND) {
        let look = system::look(self.theme);
        self.palette = Palette::new(look);
        self.animate = system::animations_on();
        let dark = BOOL::from(matches!(look, Look::Fluent(Scheme::Dark, _)));
        // SAFETY: a BOOL, sized as the attribute takes, on a live window.
        _ = unsafe {
            DwmSetWindowAttribute(
                window,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                (&raw const dark).cast::<c_void>(),
                size_of::<BOOL>() as u32,
            )
        };
        let wanted = wanted_backdrop(system::build(), matches!(look, Look::Contrast(_)));
        let accepted = wanted == Backdrop::Mica && set_mica(window, true);
        if !accepted {
            set_mica(window, false);
        }
        self.backdrop = backdrop(wanted, accepted);
        invalidate(window);
    }

    fn show_screen(&mut self, window: HWND, screen: Screen) {
        if self.screen.as_ref() == Some(&screen) {
            return;
        }
        let new_layout = lay_out(&screen, &self.painter);
        if new_layout.height != self.layout.height {
            let (width, height) = outer_size(new_layout.width, new_layout.height, dpi_of(window));
            // SAFETY: a live window; it keeps its place and stacking.
            _ = unsafe {
                SetWindowPos(
                    window,
                    None,
                    0,
                    0,
                    width,
                    height,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
            // The WM_SIZE this sends finds the state busy: resize here.
            self.painter.resize(window);
        }
        if self.screen.as_ref().map(|shown| &shown.buttons) != Some(&screen.buttons) {
            self.focus = screen.default_button().unwrap_or(0);
            self.hover = None;
            self.pressed = None;
        }
        if let Some(taskbar) = &self.taskbar {
            taskbar.show(window, &screen);
        }
        let before = self.screen.as_ref();
        if before.map(access::parts) != Some(access::parts(&screen)) {
            self.pending.push(Event::Structure);
        }
        if let (Some(old), Some(new)) = (before.and_then(access::percent), access::percent(&screen))
            && old != new
        {
            self.pending.push(Event::Progress(old, new));
        }
        if let Some(text) = access::announcement(before, &screen) {
            self.pending.push(Event::Announce(text));
        }
        self.access.update(View {
            window,
            screen: screen.clone(),
            layout: new_layout.clone(),
            focus: (!screen.buttons.is_empty()).then_some(self.focus),
        });
        self.layout = new_layout;
        self.screen = Some(screen);
        invalidate(window);
    }

    fn tick(&mut self, window: HWND) {
        let Mode::Following { shared, follower, end } = &mut self.mode else { return };
        let phase = shared.lock().unwrap_or_else(PoisonError::into_inner).clone();
        match follower.tick(&phase, Instant::now()) {
            Tick::Show(screen) => {
                self.show_screen(window, screen);
                // The spinner turns on every frame.
                if self.animate
                    && self.screen.as_ref().is_some_and(|screen| screen.current().is_some())
                {
                    invalidate(window);
                }
            }
            Tick::End(phase) => *end = Some(ProgressEnd::Ended(phase)),
        }
    }

    fn paint(&mut self, window: HWND) {
        let Some(screen) = &self.screen else { return };
        let spin = if self.animate {
            (self.started.elapsed().as_secs_f32() / SPIN_SECONDS).fract()
        } else {
            0.0
        };
        let frame = Frame {
            screen,
            layout: &self.layout,
            palette: &self.palette,
            backdrop: self.backdrop,
            hover: self.hover,
            pressed: self.pressed,
            focus: self.focus_visible.then_some(self.focus),
            spin,
        };
        _ = self.painter.paint(window, &frame);
    }

    /// The user pressed the button at `index`.
    fn press(&mut self, index: usize) {
        let command = self
            .screen
            .as_ref()
            .and_then(|screen| screen.buttons.get(index))
            .map(|button| button.command);
        match command {
            Some(Command::OpenApp) => {
                if let Mode::Failing { choice } = &mut self.mode {
                    *choice = Some(Choice::OpenApp);
                }
            }
            Some(Command::Close) => self.close(),
            None => {}
        }
    }

    /// The window is closed: by its button, Esc, Alt+F4 or the title bar.
    fn close(&mut self) {
        match &mut self.mode {
            Mode::Following { end, .. } => *end = Some(ProgressEnd::Closed),
            Mode::Failing { choice } => *choice = Some(Choice::Close),
            Mode::Idle => {}
        }
    }

    fn button_count(&self) -> usize {
        self.screen.as_ref().map_or(0, |screen| screen.buttons.len())
    }

    fn key(&mut self, window: HWND, key: u16) -> bool {
        let count = self.button_count();
        match key {
            _ if key == VK_ESCAPE.0 => self.close(),
            _ if key == VK_RETURN.0 => {
                let target = if self.focus_visible {
                    Some(self.focus)
                } else {
                    self.screen.as_ref().and_then(Screen::default_button)
                };
                if let Some(index) = target {
                    self.press(index);
                }
            }
            _ if key == VK_SPACE.0 && self.focus_visible => self.press(self.focus),
            _ if (key == VK_TAB.0 || key == VK_LEFT.0 || key == VK_RIGHT.0) && count > 0 => {
                // SAFETY: reads the key state of this thread's input.
                let shift = unsafe { GetKeyState(i32::from(VK_SHIFT.0)) } < 0;
                let back = key == VK_LEFT.0 || (key == VK_TAB.0 && shift);
                if self.focus_visible {
                    self.focus = if back {
                        (self.focus + count - 1) % count
                    } else {
                        (self.focus + 1) % count
                    };
                }
                self.focus_visible = true;
                self.access.focus(Some(self.focus));
                self.pending.push(Event::Focus(self.focus));
                invalidate(window);
            }
            _ => return false,
        }
        true
    }

    fn mouse(&mut self, window: HWND, message: u32, lparam: LPARAM) {
        let scale = 96.0 / dpi_of(window) as f32;
        let x = f32::from(lparam.0 as i16) * scale;
        let y = f32::from((lparam.0 >> 16) as i16) * scale;
        let at = self.layout.button_at(x, y);
        match message {
            WM_MOUSEMOVE => {
                if !self.tracking_mouse {
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: window,
                        dwHoverTime: 0,
                    };
                    // SAFETY: `track` is sized and names this live window.
                    self.tracking_mouse = unsafe { TrackMouseEvent(&mut track) }.is_ok();
                }
                if at != self.hover {
                    self.hover = at;
                    invalidate(window);
                }
            }
            WM_LBUTTONDOWN => {
                self.pressed = at;
                // SAFETY: this window takes the mouse until the button is up.
                unsafe { SetCapture(window) };
                invalidate(window);
            }
            WM_LBUTTONUP => {
                // SAFETY: releases the capture taken on the button's press.
                _ = unsafe { ReleaseCapture() };
                let pressed = self.pressed.take();
                invalidate(window);
                if let Some(index) = pressed.filter(|&index| Some(index) == at) {
                    self.press(index);
                }
            }
            _ => {}
        }
    }

    fn handle(
        &mut self,
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        match message {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                // SAFETY: a live window, inside WM_PAINT.
                unsafe { BeginPaint(window, &mut paint) };
                self.paint(window);
                // SAFETY: matches BeginPaint.
                _ = unsafe { EndPaint(window, &paint) };
            }
            // Everything is drawn in WM_PAINT; erasing would flash.
            WM_ERASEBKGND => return Some(LRESULT(1)),
            WM_SIZE => {
                self.painter.resize(window);
                invalidate(window);
            }
            WM_DPICHANGED => {
                // SAFETY: Windows passes the suggested rectangle with this
                // message.
                let suggested = unsafe { *(lparam.0 as *const RECT) };
                let dpi = u32::from(wparam.0 as u16);
                let (width, height) = outer_size(self.layout.width, self.layout.height, dpi);
                // SAFETY: a live window, moved where Windows suggests.
                _ = unsafe {
                    SetWindowPos(
                        window,
                        None,
                        suggested.left,
                        suggested.top,
                        width,
                        height,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    )
                };
                self.painter.resize(window);
                invalidate(window);
            }
            WM_TIMER if wparam.0 == TICK_TIMER => self.tick(window),
            WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP => self.mouse(window, message, lparam),
            WM_MOUSELEAVE => {
                self.tracking_mouse = false;
                if self.hover.take().is_some() {
                    invalidate(window);
                }
            }
            WM_KEYDOWN => {
                if !self.key(window, wparam.0 as u16) {
                    return None;
                }
            }
            WM_CLOSE => self.close(),
            INVOKE_MESSAGE => self.press(wparam.0),
            WM_SETTINGCHANGE
            | WM_THEMECHANGED
            | WM_SYSCOLORCHANGE
            | WM_DWMCOLORIZATIONCOLORCHANGED => {
                self.apply_look(window);
                return None;
            }
            _ if message == self.taskbar_created => {
                self.taskbar = Taskbar::new();
                if let (Some(taskbar), Some(screen)) = (&self.taskbar, &self.screen) {
                    taskbar.show(window, screen);
                }
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

fn invalidate(window: HWND) {
    // SAFETY: a live window; the whole client area is drawn again.
    _ = unsafe { InvalidateRect(Some(window), None, false) };
}

/// Asks for Mica, or takes it back, and says whether Windows took it.
fn set_mica(window: HWND, on: bool) -> bool {
    // The whole window is frame, so that Mica shows behind the content
    // where it is cleared to transparent.
    let margin = if on { -1 } else { 0 };
    let margins = MARGINS {
        cxLeftWidth: margin,
        cxRightWidth: margin,
        cyTopHeight: margin,
        cyBottomHeight: margin,
    };
    // SAFETY: valid margins on a live window.
    let extended = unsafe { DwmExtendFrameIntoClientArea(window, &margins) };
    let kind = if on { DWMSBT_MAINWINDOW } else { DWMSBT_NONE };
    // SAFETY: the backdrop type, sized as the attribute takes.
    let set = unsafe {
        DwmSetWindowAttribute(
            window,
            DWMWA_SYSTEMBACKDROP_TYPE,
            (&raw const kind).cast::<c_void>(),
            size_of_val(&kind) as u32,
        )
    };
    extended.is_ok() && set.is_ok()
}

/// Tells screen readers what the state set aside, once it is free.
fn flush(state: &RefCell<State>) {
    let taken = state
        .try_borrow_mut()
        .ok()
        .map(|mut state| (std::mem::take(&mut state.pending), state.access.clone()));
    if let Some((events, access)) = taken {
        access.raise(events);
    }
}

unsafe extern "system" fn window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        // SAFETY: for WM_NCCREATE, `lparam` points at the CREATESTRUCTW,
        // whose parameter is the state `Window::open` passed.
        let created = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        // SAFETY: stores the state's address with the live window.
        unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, created.lpCreateParams as isize) };
    }
    // SAFETY: reads back what WM_NCCREATE stored, or 0 before it.
    let state = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const RefCell<State>;
    // SAFETY: when set, it points at the boxed state, which `Window`
    // keeps alive until after the window is destroyed.
    let state = unsafe { state.as_ref() };
    // A screen reader asks for the window's elements; answered without
    // holding the state, as the answer can call the providers.
    if message == WM_GETOBJECT
        && lparam.0 as i32 == UiaRootObjectId
        && let Some(access) =
            state.and_then(|state| state.try_borrow().ok()).map(|state| state.access.clone())
    {
        return access.answer(window, wparam, lparam);
    }
    let handled = state
        .and_then(|state| state.try_borrow_mut().ok())
        .and_then(|mut state| state.handle(window, message, wparam, lparam));
    if let Some(state) = state {
        flush(state);
    }
    match handled {
        Some(result) => result,
        // A close that arrives while the state is busy must not destroy
        // the window behind its owner's back.
        None if message == WM_CLOSE => LRESULT(0),
        // SAFETY: the default handling, for a live window.
        None => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

impl Window {
    /// Makes the window, hidden, in the app's `theme`.
    pub fn open(theme: CompanionTheme) -> Result<Self> {
        // SAFETY: COM for this thread, once, for Direct2D's helpers and the
        // taskbar; a second call is harmless.
        _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let painter = Painter::new()?;
        let look = system::look(theme);
        let state = Box::new(RefCell::new(State {
            theme,
            painter,
            palette: Palette::new(look),
            backdrop: Backdrop::Solid,
            animate: system::animations_on(),
            screen: None,
            // A first layout, for the window's first size.
            layout: Layout::default(),
            hover: None,
            pressed: None,
            focus: 0,
            focus_visible: false,
            tracking_mouse: false,
            mode: Mode::Idle,
            started: Instant::now(),
            taskbar: None,
            // SAFETY: a NUL-terminated literal.
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated")) },
            access: Access::new(),
            pending: Vec::new(),
        }));
        let first = lay_out(&Screen::working(None, Step::Preparing, 0), &state.borrow().painter);
        let (x, y, width, height) = placement(&first);
        state.borrow_mut().layout = first;

        // SAFETY: a null name asks for this executable's module.
        let module = unsafe { GetModuleHandleW(None) }?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_procedure),
            hInstance: module.into(),
            // SAFETY: the resource ID in a name's place, as Windows takes it.
            hIcon: unsafe { LoadIconW(Some(module.into()), PCWSTR(ICON_RESOURCE as *const u16)) }
                .unwrap_or_default(),
            // SAFETY: a system cursor.
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        // SAFETY: `class` is valid; registering twice fails harmlessly.
        unsafe { RegisterClassW(&class) };
        let state_pointer: *const RefCell<State> = &*state;
        // SAFETY: the class is registered; the state outlives the window,
        // as `Window` destroys the window before dropping it.
        let handle = unsafe {
            CreateWindowExW(
                EX_STYLE,
                CLASS,
                TITLE,
                STYLE,
                x,
                y,
                width,
                height,
                None,
                None,
                Some(module.into()),
                Some(state_pointer.cast::<c_void>()),
            )
        }?;
        // An elevated window hears from the unelevated taskbar only when
        // it lets that message through.
        let taskbar_created = state.borrow().taskbar_created;
        // SAFETY: a live window and a registered message.
        _ = unsafe { ChangeWindowMessageFilterEx(handle, taskbar_created, MSGFLT_ALLOW, None) };
        state.borrow_mut().apply_look(handle);
        let window = Self { handle, state };
        // On failure the window is dropped, and destroyed, unseen.
        window.state.borrow_mut().painter.prepare(handle)?;
        Ok(window)
    }

    fn show(&self) {
        // SAFETY: a live window.
        if !unsafe { IsWindowVisible(self.handle) }.as_bool() {
            // SAFETY: as above.
            _ = unsafe { ShowWindow(self.handle, SW_SHOWNORMAL) };
            // SAFETY: as above; the app that started this had the
            // foreground, so Windows lets it take it.
            _ = unsafe { SetForegroundWindow(self.handle) };
        }
    }

    /// Runs the window's messages until `done` says so.
    fn run_until(&self, done: impl Fn(&State) -> bool) {
        loop {
            if done(&self.state.borrow()) {
                return;
            }
            let mut message = MSG::default();
            // SAFETY: a valid out-pointer; every window of this thread.
            let got = unsafe { GetMessageW(&mut message, None, 0, 0) };
            if got.0 <= 0 {
                return;
            }
            // SAFETY: a message just taken from the queue.
            _ = unsafe { TranslateMessage(&message) };
            // SAFETY: as above.
            unsafe { DispatchMessageW(&message) };
        }
    }
}

impl Surface for Window {
    fn follow(&mut self, shared: &Arc<Mutex<Phase>>, follower: Follower) -> ProgressEnd {
        {
            let mut state = self.state.borrow_mut();
            state.mode = Mode::Following { shared: Arc::clone(shared), follower, end: None };
            state.tick(self.handle);
        }
        flush(&self.state);
        self.show();
        // SAFETY: a live window; the timer is killed below.
        unsafe { SetTimer(Some(self.handle), TICK_TIMER, TICK_MS, None) };
        self.run_until(|state| matches!(state.mode, Mode::Following { end: Some(_), .. }));
        // SAFETY: the timer set above.
        _ = unsafe { KillTimer(Some(self.handle), TICK_TIMER) };
        let mut state = self.state.borrow_mut();
        match std::mem::replace(&mut state.mode, Mode::Idle) {
            Mode::Following { end: Some(end), .. } => end,
            // The message loop ended without an answer: Windows is shutting
            // the session down.
            _ => ProgressEnd::Closed,
        }
    }

    fn fail(&mut self, headline: &str, reason: &str, can_open: bool) -> Choice {
        {
            let mut state = self.state.borrow_mut();
            let screen = Screen::failed(headline, reason, can_open, state.screen.as_ref());
            state.mode = Mode::Failing { choice: None };
            state.show_screen(self.handle, screen);
        }
        flush(&self.state);
        self.show();
        self.run_until(|state| matches!(state.mode, Mode::Failing { choice: Some(_) }));
        let mut state = self.state.borrow_mut();
        match std::mem::replace(&mut state.mode, Mode::Idle) {
            Mode::Failing { choice: Some(choice) } => choice,
            _ => Choice::Close,
        }
    }

    fn hide(&mut self) {
        // SAFETY: a live window.
        _ = unsafe { ShowWindow(self.handle, SW_HIDE) };
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let access = self.state.borrow().access.clone();
        access.disconnect(self.handle);
        // SAFETY: the window this owns; its state is dropped after it.
        _ = unsafe { DestroyWindow(self.handle) };
    }
}
