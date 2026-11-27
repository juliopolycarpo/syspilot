//! What screen readers get from the drawn window: UI Automation elements
//! for the headline, the steps, the reason, the bar, the footer and the
//! buttons, and an announcement when the update moves on or stops.
//!
//! The task dialog gave this for free. Here a provider answers from the
//! window's [`View`], which the window keeps up to date; the providers
//! ask to be called on the window's own thread, so they never race it.

use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayPutElement};
use windows::Win32::System::Variant::{VARIANT, VT_I4};
use windows::Win32::UI::Accessibility::{
    HeadingLevel1, IInvokeProvider_Impl, IRangeValueProvider_Impl, IRawElementProviderFragment,
    IRawElementProviderFragment_Impl, IRawElementProviderFragmentRoot,
    IRawElementProviderFragmentRoot_Impl, IRawElementProviderSimple,
    IRawElementProviderSimple_Impl, NavigateDirection, NavigateDirection_FirstChild,
    NavigateDirection_LastChild, NavigateDirection_NextSibling, NavigateDirection_Parent,
    NavigateDirection_PreviousSibling, NotificationKind_ActionCompleted,
    NotificationProcessing_ImportantMostRecent, ProviderOptions,
    ProviderOptions_ServerSideProvider, ProviderOptions_UseComThreading,
    StructureChangeType_ChildrenInvalidated, UIA_AutomationFocusChangedEventId,
    UIA_AutomationIdPropertyId, UIA_ButtonControlTypeId, UIA_ControlTypePropertyId,
    UIA_HasKeyboardFocusPropertyId, UIA_HeadingLevelPropertyId, UIA_InvokePatternId,
    UIA_IsEnabledPropertyId, UIA_IsKeyboardFocusablePropertyId, UIA_NamePropertyId, UIA_PATTERN_ID,
    UIA_PROPERTY_ID, UIA_ProgressBarControlTypeId, UIA_RangeValuePatternId,
    UIA_RangeValueValuePropertyId, UIA_TextControlTypeId, UiaAppendRuntimeId,
    UiaClientsAreListening, UiaDisconnectProvider, UiaHostProviderFromHwnd,
    UiaRaiseAutomationEvent, UiaRaiseAutomationPropertyChangedEvent, UiaRaiseNotificationEvent,
    UiaRaiseStructureChangedEvent, UiaRect, UiaReturnRawElementProvider,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::{BOOL, BSTR, Error, IUnknown, Interface, Result};

use crate::layout::{Layout, Rect};
use crate::progress::BAR_END;
use crate::screen::{Bar, Command, Mark, Screen};

/// What the window posts itself when a screen reader presses a button;
/// `wparam` is the button's index.
pub const INVOKE_MESSAGE: u32 = WM_APP + 1;

/// What is on show, for the providers.
pub struct View {
    /// The window.
    pub window: HWND,
    /// What it shows.
    pub screen: Screen,
    /// Where.
    pub layout: Layout,
    /// The button the keyboard is on.
    pub focus: Option<usize>,
}

/// The view and the one root element, shared by the window and every
/// provider it handed out.
#[derive(Default)]
pub struct Shared {
    view: RefCell<Option<View>>,
    root: RefCell<Option<IRawElementProviderSimple>>,
}

/// A part of the window a screen reader can land on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The window itself.
    Root,
    /// The headline.
    Headline,
    /// A step.
    Row(usize),
    /// The reason for a failure.
    Reason,
    /// The bar.
    Bar,
    /// The footer.
    Note,
    /// A button.
    Button(usize),
}

/// The parts on show, in reading order.
pub fn parts(screen: &Screen) -> Vec<Part> {
    let mut parts = vec![Part::Headline];
    parts.extend((0..screen.rows.len()).map(Part::Row));
    parts.extend(screen.reason.as_ref().map(|_| Part::Reason));
    parts.extend(screen.bar.map(|_| Part::Bar));
    parts.extend(screen.note.map(|_| Part::Note));
    parts.extend((0..screen.buttons.len()).map(Part::Button));
    parts
}

/// Where `part` is drawn.
fn rect_of(layout: &Layout, part: Part) -> Option<Rect> {
    match part {
        Part::Root => None,
        Part::Headline => Some(layout.headline),
        Part::Row(index) => layout.rows.get(index).map(|&(mark, text)| Rect {
            width: text.right() - mark.x,
            x: mark.x,
            ..text
        }),
        Part::Reason => layout.reason,
        Part::Bar => layout.bar,
        Part::Note => layout.note,
        Part::Button(index) => layout.buttons.get(index).copied(),
    }
}

/// The part at `(x, y)`, in the window's content.
pub fn part_at(screen: &Screen, layout: &Layout, x: f32, y: f32) -> Option<Part> {
    parts(screen)
        .into_iter()
        .find(|&part| rect_of(layout, part).is_some_and(|rect| rect.contains(x, y)))
}

/// What a screen reader reads for `part`.
pub fn name(screen: &Screen, part: Part) -> String {
    match part {
        Part::Root => String::new(),
        Part::Headline => screen.headline.clone(),
        Part::Row(index) => screen.rows.get(index).map_or_else(String::new, |row| {
            let state = match row.mark {
                Mark::Done => "done",
                Mark::Current => "in progress",
                Mark::Pending => "not started",
                Mark::Failed => "stopped here",
            };
            format!("{}, {state}", row.text)
        }),
        Part::Reason => screen.reason.clone().unwrap_or_default(),
        Part::Bar => "Update progress".to_owned(),
        Part::Note => screen.note.unwrap_or_default().to_owned(),
        Part::Button(index) => {
            screen.buttons.get(index).map_or_else(String::new, |button| button.label.to_owned())
        }
    }
}

/// What a screen reader is told when `after` replaces `before`: how the
/// update ended, or the step it moved on to.
pub fn announcement(before: Option<&Screen>, after: &Screen) -> Option<String> {
    if let Some(reason) = &after.reason {
        let new = before.is_none_or(|before| before.reason.as_ref() != Some(reason));
        return new.then(|| format!("{}. {reason}", after.headline));
    }
    let step = after.current().map(|row| row.text);
    match before {
        None => Some(match step {
            Some(step) => format!("{}. {step}", after.headline),
            None => after.headline.clone(),
        }),
        Some(before) if before.headline != after.headline => Some(after.headline.clone()),
        Some(before) if before.current().map(|row| row.text) != step => step.map(str::to_owned),
        Some(_) => None,
    }
}

/// The bar in whole percent, which is as often as its value is reported.
pub fn percent(screen: &Screen) -> Option<i32> {
    screen.bar.map(|bar| (bar.position * 100 / BAR_END) as i32)
}

/// Something to tell UI Automation once the window is not busy.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Read this out.
    Announce(String),
    /// The parts changed.
    Structure,
    /// The bar moved, in percent.
    Progress(i32, i32),
    /// The keyboard moved to this button.
    Focus(usize),
}

fn runtime_number(part: Part) -> i32 {
    match part {
        Part::Root => 0,
        Part::Headline => 1,
        Part::Reason => 2,
        Part::Bar => 3,
        Part::Note => 4,
        Part::Row(index) => 10 + index as i32,
        Part::Button(index) => 20 + index as i32,
    }
}

/// The COM object, alone in its module: the code `implement` writes casts
/// its tables in a way the workspace lints refuse.
mod provider {
    #![expect(trivial_casts, reason = "in the COM tables the implement macro writes")]

    use std::rc::Rc;

    use windows::Win32::UI::Accessibility::{
        IInvokeProvider, IRangeValueProvider, IRawElementProviderFragment,
        IRawElementProviderFragmentRoot, IRawElementProviderSimple,
    };
    use windows::core::implement;

    use super::{Part, Shared};

    /// A provider for one part. One type for all, with the patterns each
    /// part has offered through `GetPatternProvider`.
    #[implement(
        IRawElementProviderSimple,
        IRawElementProviderFragment,
        IRawElementProviderFragmentRoot,
        IInvokeProvider,
        IRangeValueProvider
    )]
    pub struct Element {
        pub shared: Rc<Shared>,
        pub part: Part,
    }
}

use provider::{Element, Element_Impl};

/// No element: a null pointer and success, as UI Automation reads it.
fn none<T>() -> Result<T> {
    Err(Error::empty())
}

fn element(shared: &Rc<Shared>, part: Part) -> IRawElementProviderFragment {
    Element { shared: Rc::clone(shared), part }.into()
}

fn simple(shared: &Rc<Shared>, part: Part) -> IRawElementProviderSimple {
    Element { shared: Rc::clone(shared), part }.into()
}

impl Element {
    fn with_view<T>(&self, read: impl FnOnce(&View) -> T) -> Option<T> {
        let view = self.shared.view.try_borrow().ok()?;
        view.as_ref().map(read)
    }

    fn root(&self) -> Result<IRawElementProviderFragment> {
        let root = self.shared.root.try_borrow().ok().and_then(|root| root.clone());
        match root {
            Some(root) => root.cast(),
            None => Ok(element(&self.shared, Part::Root)),
        }
    }
}

impl IRawElementProviderSimple_Impl for Element_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        // Called on the window's thread, as COM's single-threaded
        // apartment would.
        Ok(ProviderOptions_ServerSideProvider | ProviderOptions_UseComThreading)
    }

    fn GetPatternProvider(&self, pattern: UIA_PATTERN_ID) -> Result<IUnknown> {
        let has = match self.part {
            Part::Button(_) => pattern == UIA_InvokePatternId,
            Part::Bar => pattern == UIA_RangeValuePatternId,
            _ => false,
        };
        if !has {
            return none();
        }
        let provider: IRawElementProviderSimple =
            Element { shared: Rc::clone(&self.shared), part: self.part }.into();
        provider.cast()
    }

    #[expect(non_upper_case_globals, reason = "Windows names its property IDs in mixed case")]
    fn GetPropertyValue(&self, property: UIA_PROPERTY_ID) -> Result<VARIANT> {
        let part = self.part;
        let value = self.with_view(|view| match property {
            UIA_NamePropertyId if part != Part::Root => {
                VARIANT::from(name(&view.screen, part).as_str())
            }
            UIA_ControlTypePropertyId => match part {
                Part::Root => VARIANT::default(),
                Part::Bar => VARIANT::from(UIA_ProgressBarControlTypeId.0),
                Part::Button(_) => VARIANT::from(UIA_ButtonControlTypeId.0),
                _ => VARIANT::from(UIA_TextControlTypeId.0),
            },
            UIA_HeadingLevelPropertyId if part == Part::Headline => VARIANT::from(HeadingLevel1.0),
            UIA_AutomationIdPropertyId => match part {
                Part::Button(index) => {
                    match view.screen.buttons.get(index).map(|button| button.command) {
                        Some(Command::OpenApp) => VARIANT::from("OpenApp"),
                        Some(Command::Close) => VARIANT::from("Close"),
                        None => VARIANT::default(),
                    }
                }
                Part::Bar => VARIANT::from("Progress"),
                _ => VARIANT::default(),
            },
            UIA_IsKeyboardFocusablePropertyId if matches!(part, Part::Button(_)) => {
                VARIANT::from(true)
            }
            UIA_HasKeyboardFocusPropertyId if matches!(part, Part::Button(_)) => {
                VARIANT::from(Some(part) == view.focus.map(Part::Button))
            }
            UIA_IsEnabledPropertyId if part != Part::Root => VARIANT::from(true),
            _ => VARIANT::default(),
        });
        Ok(value.unwrap_or_default())
    }

    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        match (self.part, self.with_view(|view| view.window)) {
            // SAFETY: the window is live while its view is set.
            (Part::Root, Some(window)) => unsafe { UiaHostProviderFromHwnd(window) },
            _ => none(),
        }
    }
}

impl IRawElementProviderFragment_Impl for Element_Impl {
    #[expect(non_upper_case_globals, reason = "Windows names its directions in mixed case")]
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        let Some(parts) = self.with_view(|view| parts(&view.screen)) else { return none() };
        let target = match (self.part, direction) {
            (Part::Root, NavigateDirection_FirstChild) => parts.first().copied(),
            (Part::Root, NavigateDirection_LastChild) => parts.last().copied(),
            (Part::Root, _) => None,
            (_, NavigateDirection_Parent) => return self.root(),
            (part, NavigateDirection_NextSibling) => parts
                .iter()
                .position(|&other| other == part)
                .and_then(|at| parts.get(at + 1))
                .copied(),
            (part, NavigateDirection_PreviousSibling) => parts
                .iter()
                .position(|&other| other == part)
                .and_then(|at| at.checked_sub(1))
                .and_then(|at| parts.get(at))
                .copied(),
            _ => None,
        };
        match target {
            Some(part) => Ok(element(&self.shared, part)),
            None => none(),
        }
    }

    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        // The root's identity is its window's.
        if self.part == Part::Root {
            return Ok(std::ptr::null_mut());
        }
        // SAFETY: a new vector of two 32-bit integers, handed to the caller.
        let array = unsafe { SafeArrayCreateVector(VT_I4, 0, 2) };
        if array.is_null() {
            return Err(Error::from_thread());
        }
        for (index, value) in [(0, UiaAppendRuntimeId as i32), (1, runtime_number(self.part))] {
            // SAFETY: `array` holds two integers; `index` is 0 or 1 and
            // `value` lives through the call, which copies it.
            unsafe { SafeArrayPutElement(array, &index, (&raw const value).cast::<c_void>()) }?;
        }
        Ok(array)
    }

    fn BoundingRectangle(&self) -> Result<UiaRect> {
        let part = self.part;
        let found =
            self.with_view(|view| rect_of(&view.layout, part).map(|rect| (view.window, rect)));
        let Some(Some((window, rect))) = found else { return Ok(UiaRect::default()) };
        let mut origin = POINT::default();
        // SAFETY: a live window and a valid point.
        _ = unsafe { ClientToScreen(window, &mut origin) };
        // SAFETY: a live window.
        let scale = f64::from(unsafe { GetDpiForWindow(window) }.max(96)) / 96.0;
        Ok(UiaRect {
            left: f64::from(origin.x) + f64::from(rect.x) * scale,
            top: f64::from(origin.y) + f64::from(rect.y) * scale,
            width: f64::from(rect.width) * scale,
            height: f64::from(rect.height) * scale,
        })
    }

    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }

    fn SetFocus(&self) -> Result<()> {
        Ok(())
    }

    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        self.root()?.cast()
    }
}

impl IRawElementProviderFragmentRoot_Impl for Element_Impl {
    fn ElementProviderFromPoint(&self, x: f64, y: f64) -> Result<IRawElementProviderFragment> {
        let found = self.with_view(|view| {
            let mut point = POINT { x: x as i32, y: y as i32 };
            // SAFETY: a live window and a valid point.
            _ = unsafe { ScreenToClient(view.window, &mut point) };
            // SAFETY: a live window.
            let scale = 96.0 / unsafe { GetDpiForWindow(view.window) }.max(96) as f32;
            part_at(&view.screen, &view.layout, point.x as f32 * scale, point.y as f32 * scale)
        });
        match found.flatten() {
            Some(part) => Ok(element(&self.shared, part)),
            None => self.root(),
        }
    }

    fn GetFocus(&self) -> Result<IRawElementProviderFragment> {
        match self.with_view(|view| view.focus).flatten() {
            Some(index) => Ok(element(&self.shared, Part::Button(index))),
            None => none(),
        }
    }
}

impl IInvokeProvider_Impl for Element_Impl {
    fn Invoke(&self) -> Result<()> {
        let (Part::Button(index), Some(window)) = (self.part, self.with_view(|view| view.window))
        else {
            return Err(Error::from_hresult(windows::Win32::Foundation::E_FAIL));
        };
        // Pressed by the window itself, outside this call, as a click is.
        // SAFETY: a live window; the message carries only an index.
        unsafe { PostMessageW(Some(window), INVOKE_MESSAGE, WPARAM(index), LPARAM(0)) }
    }
}

impl IRangeValueProvider_Impl for Element_Impl {
    fn SetValue(&self, _value: f64) -> Result<()> {
        Err(Error::from_hresult(windows::Win32::Foundation::E_ACCESSDENIED))
    }

    fn Value(&self) -> Result<f64> {
        Ok(self.with_view(|view| percent(&view.screen)).flatten().map_or(0.0, f64::from))
    }

    fn IsReadOnly(&self) -> Result<BOOL> {
        Ok(BOOL(1))
    }

    fn Maximum(&self) -> Result<f64> {
        Ok(100.0)
    }

    fn Minimum(&self) -> Result<f64> {
        Ok(0.0)
    }

    fn LargeChange(&self) -> Result<f64> {
        Ok(0.0)
    }

    fn SmallChange(&self) -> Result<f64> {
        Ok(0.0)
    }
}

/// The window's side of it.
#[derive(Clone)]
pub struct Access {
    shared: Rc<Shared>,
}

impl Access {
    /// Nothing on show yet.
    pub fn new() -> Self {
        Self { shared: Rc::new(Shared::default()) }
    }

    /// The window now shows `view`.
    pub fn update(&self, view: View) {
        if let Ok(mut shown) = self.shared.view.try_borrow_mut() {
            *shown = Some(view);
        }
    }

    /// Only the bar moved, to `bar`.
    pub fn move_bar(&self, bar: Option<Bar>) {
        if let Ok(mut shown) = self.shared.view.try_borrow_mut()
            && let Some(view) = shown.as_mut()
        {
            view.screen.bar = bar;
        }
    }

    /// The keyboard is now on `focus`.
    pub fn focus(&self, focus: Option<usize>) {
        if let Ok(mut shown) = self.shared.view.try_borrow_mut()
            && let Some(view) = shown.as_mut()
        {
            view.focus = focus;
        }
    }

    fn root(&self) -> IRawElementProviderSimple {
        let mut root = self.shared.root.borrow_mut();
        root.get_or_insert_with(|| {
            Element { shared: Rc::clone(&self.shared), part: Part::Root }.into()
        })
        .clone()
    }

    /// Answers WM_GETOBJECT.
    pub fn answer(
        &self,
        window: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        let root = self.root();
        // SAFETY: the window's own message, answered with its root.
        unsafe { UiaReturnRawElementProvider(window, wparam, lparam, &root) }
    }

    /// Tells UI Automation of `events`, when anyone listens.
    pub fn raise(&self, events: Vec<Event>) {
        // SAFETY: only reads whether a client is connected.
        if events.is_empty() || !unsafe { UiaClientsAreListening() }.as_bool() {
            return;
        }
        let root = self.root();
        for event in events {
            match event {
                Event::Announce(text) => {
                    // SAFETY: a live provider and two strings for the call.
                    _ = unsafe {
                        UiaRaiseNotificationEvent(
                            &root,
                            NotificationKind_ActionCompleted,
                            NotificationProcessing_ImportantMostRecent,
                            &BSTR::from(text),
                            &BSTR::from("SysPilotUpdate"),
                        )
                    };
                }
                Event::Structure => {
                    // SAFETY: the root, whose runtime ID is its window's.
                    _ = unsafe {
                        UiaRaiseStructureChangedEvent(
                            &root,
                            StructureChangeType_ChildrenInvalidated,
                            std::ptr::null_mut(),
                            0,
                        )
                    };
                }
                Event::Progress(old, new) => {
                    let bar = simple(&self.shared, Part::Bar);
                    // SAFETY: a live provider and two values for the call.
                    _ = unsafe {
                        UiaRaiseAutomationPropertyChangedEvent(
                            &bar,
                            UIA_RangeValueValuePropertyId,
                            &VARIANT::from(f64::from(old)),
                            &VARIANT::from(f64::from(new)),
                        )
                    };
                }
                Event::Focus(index) => {
                    let button = simple(&self.shared, Part::Button(index));
                    // SAFETY: a live provider.
                    _ = unsafe {
                        UiaRaiseAutomationEvent(&button, UIA_AutomationFocusChangedEventId)
                    };
                }
            }
        }
    }

    /// The window is going: providers still held by clients answer
    /// nothing from now on.
    pub fn disconnect(&self, window: HWND) {
        if let Ok(mut view) = self.shared.view.try_borrow_mut() {
            *view = None;
        }
        let root = self.shared.root.borrow_mut().take();
        // SAFETY: tells UI Automation the window has no provider any more.
        unsafe {
            UiaReturnRawElementProvider(
                window,
                WPARAM(0),
                LPARAM(0),
                None::<&IRawElementProviderSimple>,
            )
        };
        if let Some(root) = root {
            // SAFETY: the provider this window handed out.
            _ = unsafe { UiaDisconnectProvider(&root) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Measure, Style, lay_out};
    use crate::watch::Step;

    /// Every character 7 wide, every line 20 tall, as in the layout tests.
    struct GridMeasure;

    impl Measure for GridMeasure {
        fn measure(&self, text: &str, _style: Style, width: f32) -> (f32, f32) {
            let full = text.chars().count() as f32 * 7.0;
            (full.min(width), (full / width).ceil().max(1.0) * 20.0)
        }
    }

    #[test]
    fn parts_are_read_in_the_order_they_are_drawn() {
        let screen = Screen::working(None, Step::Installing, 400);
        assert_eq!(
            parts(&screen),
            [
                Part::Headline,
                Part::Row(0),
                Part::Row(1),
                Part::Row(2),
                Part::Bar,
                Part::Note,
                Part::Button(0)
            ]
        );
        let failed = Screen::failed("h", "r", true, None);
        assert_eq!(
            parts(&failed),
            [Part::Headline, Part::Reason, Part::Button(0), Part::Button(1)]
        );
    }

    #[test]
    fn a_step_is_read_with_where_it_stands() {
        let screen = Screen::working(None, Step::Installing, 400);
        assert_eq!(name(&screen, Part::Row(0)), "Download and check the new version, done");
        assert_eq!(name(&screen, Part::Row(1)), "Install it, in progress");
        assert_eq!(name(&screen, Part::Row(2)), "Start SysPilot again, not started");
        let failed = Screen::failed("h", "r", false, Some(&screen));
        assert_eq!(name(&failed, Part::Row(1)), "Install it, stopped here");
        assert_eq!(name(&failed, Part::Button(0)), "Close");
    }

    #[test]
    fn the_first_screen_says_what_runs_and_each_new_step_is_told() {
        let preparing = Screen::working(Some("0.3.1"), Step::Preparing, 100);
        assert_eq!(
            announcement(None, &preparing).as_deref(),
            Some("Updating SysPilot to 0.3.1. Download and check the new version")
        );
        let moved = Screen::working(Some("0.3.1"), Step::Preparing, 200);
        assert_eq!(announcement(Some(&preparing), &moved), None, "(bar moved): expected silence");
        let installing = Screen::working(Some("0.3.1"), Step::Installing, 400);
        assert_eq!(announcement(Some(&moved), &installing).as_deref(), Some("Install it"));
        let done = Screen::done(Some("0.3.1"));
        assert_eq!(
            announcement(Some(&installing), &done).as_deref(),
            Some("SysPilot 0.3.1 is installed")
        );
    }

    #[test]
    fn a_failure_is_told_with_its_reason_once() {
        let before = Screen::working(None, Step::Installing, 400);
        let failed = Screen::failed("The update did not finish", "Because.", true, Some(&before));
        assert_eq!(
            announcement(Some(&before), &failed).as_deref(),
            Some("The update did not finish. Because.")
        );
        assert_eq!(announcement(Some(&failed), &failed), None, "(same failure): expected silence");
    }

    #[test]
    fn the_bar_is_read_in_whole_percent() {
        assert_eq!(percent(&Screen::working(None, Step::Installing, 612)), Some(61));
        assert_eq!(percent(&Screen::done(None)), Some(100));
        assert_eq!(percent(&Screen::failed("h", "r", false, None)), None);
    }

    #[test]
    fn a_point_finds_the_part_drawn_there() {
        let screen = Screen::failed("h", "r", true, None);
        let layout = lay_out(&screen, &GridMeasure);
        let close = layout.buttons[1];
        assert_eq!(part_at(&screen, &layout, close.x + 2.0, close.y + 2.0), Some(Part::Button(1)));
        let headline = layout.headline;
        assert_eq!(
            part_at(&screen, &layout, headline.x + 1.0, headline.y + 1.0),
            Some(Part::Headline)
        );
        assert_eq!(
            part_at(&screen, &layout, 1.0, 1.0),
            None,
            "(window's corner): expected nothing"
        );
    }
}
