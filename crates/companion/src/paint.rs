//! Draws a [`Screen`] laid out by [`crate::layout`] with Direct2D and
//! DirectWrite, both part of Windows. Nothing here decides what goes
//! where; it only draws.

use std::f32::consts::PI;

use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F,
    D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_OPEN, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL,
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES, D2D1_CAP_STYLE_ROUND,
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_LINE_JOIN_ROUND, D2D1_PRESENT_OPTIONS_NONE,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES,
    D2D1_SWEEP_DIRECTION_CLOCKWISE, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory,
    ID2D1Bitmap, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_METRICS,
    DWRITE_WORD_WRAPPING_EMERGENCY_BREAK, DWriteCreateFactory, IDWriteFactory,
    IDWriteFontCollection, IDWriteTextFormat, IDWriteTextLayout,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeMedianCut,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::LoadIconWithScaleDown;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetClientRect};
use windows::core::{BOOL, HSTRING, PCWSTR, Result, w};
use windows_numerics::Vector2;

use crate::layout::{BADGE, BAR, Layout, MARK, Measure, Rect, Style};
use crate::progress::BAR_END;
use crate::screen::{Badge, Mark, Screen};
use crate::theme::{Backdrop, Color, Palette};

/// The icon resource the build embeds: the app's.
const ICON_RESOURCE: usize = 1;
/// Corner radius of the card and of a button, after Fluent's
/// `OverlayCornerRadius` and `ControlCornerRadius`.
const CARD_RADIUS: f32 = 8.0;
const BUTTON_RADIUS: f32 = 4.0;
/// How far the focus ring sits outside a button.
const FOCUS_GAP: f32 = 3.0;

/// One frame: what to draw and in which state the buttons are.
pub struct Frame<'a> {
    /// What the window shows.
    pub screen: &'a Screen,
    /// Where.
    pub layout: &'a Layout,
    /// In which colours.
    pub palette: &'a Palette,
    /// On what.
    pub backdrop: Backdrop,
    /// The button under the pointer.
    pub hover: Option<usize>,
    /// The button held down.
    pub pressed: Option<usize>,
    /// The button with a visible keyboard focus.
    pub focus: Option<usize>,
    /// The turn of the running step's spinner, from 0 to 1.
    pub spin: f32,
}

/// The fonts of Fluent's type ramp, as Windows has them.
struct Fonts {
    headline: IDWriteTextFormat,
    body: IDWriteTextFormat,
    caption: IDWriteTextFormat,
}

/// What lives as long as one render target: it must be made again when
/// Direct2D loses its device or the window changes DPI.
struct Target {
    target: ID2D1HwndRenderTarget,
    brush: ID2D1SolidColorBrush,
    icon: Option<ID2D1Bitmap>,
    dpi: u32,
}

/// See the module documentation.
pub struct Painter {
    factory: ID2D1Factory,
    write: IDWriteFactory,
    imaging: IWICImagingFactory,
    fonts: Fonts,
    round: ID2D1StrokeStyle,
    target: Option<Target>,
}

fn d2d_color(color: Color) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: color.r, g: color.g, b: color.b, a: color.a }
}

fn d2d_rect(rect: &Rect) -> D2D_RECT_F {
    D2D_RECT_F { left: rect.x, top: rect.y, right: rect.right(), bottom: rect.bottom() }
}

fn point(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// Whether Windows has the font family `name`.
fn has_family(collection: &IDWriteFontCollection, name: &str) -> bool {
    let (mut index, mut exists) = (0, BOOL(0));
    // SAFETY: `name` lives through the call; the out-pointers are valid.
    let found = unsafe { collection.FindFamilyName(&HSTRING::from(name), &mut index, &mut exists) };
    found.is_ok() && exists.as_bool()
}

impl Fonts {
    /// Segoe UI Variable where Windows has it (Windows 11), Segoe UI where
    /// it does not.
    fn new(write: &IDWriteFactory) -> Result<Self> {
        let mut collection = None;
        // SAFETY: `collection` is a valid out-pointer.
        unsafe { write.GetSystemFontCollection(&mut collection, false) }?;
        let variable = collection
            .as_ref()
            .is_some_and(|collection| has_family(collection, "Segoe UI Variable Text"));
        let family = |optical: &str| {
            if variable { format!("Segoe UI Variable {optical}") } else { "Segoe UI".to_owned() }
        };
        let format = |name: String, weight: DWRITE_FONT_WEIGHT, size: f32| {
            // SAFETY: the family and locale names live through the call; no
            // custom collection is passed.
            let format = unsafe {
                write.CreateTextFormat(
                    &HSTRING::from(name),
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("en-us"),
                )
            }?;
            // A path with no spaces, like Windows Installer's log, still
            // wraps at the card's edge.
            // SAFETY: a valid wrapping mode on a live format.
            unsafe { format.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK) }?;
            Ok::<_, windows::core::Error>(format)
        };
        Ok(Self {
            headline: format(family("Display"), DWRITE_FONT_WEIGHT_SEMI_BOLD, 20.0)?,
            body: format(family("Text"), DWRITE_FONT_WEIGHT_NORMAL, 14.0)?,
            caption: format(family("Small"), DWRITE_FONT_WEIGHT_NORMAL, 12.0)?,
        })
    }

    fn of(&self, style: Style) -> &IDWriteTextFormat {
        match style {
            Style::Headline => &self.headline,
            Style::Body => &self.body,
            Style::Caption => &self.caption,
        }
    }
}

impl Painter {
    /// Makes the factories and fonts. Needs COM on this thread.
    pub fn new() -> Result<Self> {
        // SAFETY: no options; single-threaded, as only the window's thread
        // draws.
        let factory: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }?;
        // SAFETY: the shared factory, from the system's dwrite.dll.
        let write: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }?;
        // SAFETY: an in-process class registered by Windows; COM is
        // initialised on this thread by the caller.
        let imaging: IWICImagingFactory =
            unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }?;
        let round_properties = D2D1_STROKE_STYLE_PROPERTIES {
            startCap: D2D1_CAP_STYLE_ROUND,
            endCap: D2D1_CAP_STYLE_ROUND,
            lineJoin: D2D1_LINE_JOIN_ROUND,
            ..Default::default()
        };
        // SAFETY: `round_properties` is valid for the call; no dashes.
        let round = unsafe { factory.CreateStrokeStyle(&round_properties, None) }?;
        let fonts = Fonts::new(&write)?;
        Ok(Self { factory, write, imaging, fonts, round, target: None })
    }

    /// A text layout of `text` in `style`, wrapped at `width`.
    fn text_layout(&self, text: &str, style: Style, width: f32) -> Result<IDWriteTextLayout> {
        // SAFETY: the text and format live through the call.
        unsafe { self.write.CreateTextLayout(&utf16(text), self.fonts.of(style), width, f32::MAX) }
    }

    /// The window's size or DPI changed: the target follows on the next
    /// frame.
    pub fn resize(&mut self, window: HWND) {
        let Some(target) = &self.target else { return };
        // SAFETY: `window` is the live window this target draws.
        if unsafe { GetDpiForWindow(window) } != target.dpi {
            // The icon is made for one size in pixels: make it all again.
            self.target = None;
            return;
        }
        let size = client_size(window);
        // SAFETY: `size` is valid for the call; the target is live.
        if unsafe { target.target.Resize(&size) }.is_err() {
            self.target = None;
        }
    }

    fn make_target(&self, window: HWND) -> Result<Target> {
        // SAFETY: `window` is a live window.
        let dpi = unsafe { GetDpiForWindow(window) };
        let properties = D2D1_RENDER_TARGET_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                // Premultiplied, so a transparent clear lets Mica through.
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: dpi as f32,
            dpiY: dpi as f32,
            ..Default::default()
        };
        let window_properties = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd: window,
            pixelSize: client_size(window),
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        // SAFETY: both property structures are valid for the call.
        let target =
            unsafe { self.factory.CreateHwndRenderTarget(&properties, &window_properties) }?;
        // SAFETY: a valid colour; no brush properties.
        let brush = unsafe { target.CreateSolidColorBrush(&d2d_color(Color::TRANSPARENT), None) }?;
        // Transparent pixels take no ClearType; grayscale also matches the
        // app's text.
        // SAFETY: a valid mode on a live target.
        unsafe { target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE) };
        let icon = self.icon(&target, dpi).ok();
        Ok(Target { target, brush, icon, dpi })
    }

    /// The app's icon, at the badge's size in pixels at `dpi`.
    fn icon(&self, target: &ID2D1HwndRenderTarget, dpi: u32) -> Result<ID2D1Bitmap> {
        let pixels = (BADGE * dpi as f32 / 96.0).round() as i32;
        // SAFETY: a null name asks for this executable's module.
        let module = unsafe { GetModuleHandleW(None) }?;
        // SAFETY: the resource ID is passed in a name's place, as Windows
        // takes it; the icon is destroyed below.
        let icon = unsafe {
            LoadIconWithScaleDown(
                Some(module.into()),
                PCWSTR(ICON_RESOURCE as *const u16),
                pixels,
                pixels,
            )
        }?;
        // SAFETY: `icon` is the live icon loaded above.
        let bitmap = unsafe { self.imaging.CreateBitmapFromHICON(icon) };
        // SAFETY: WIC copied the icon's pixels; it is no longer needed.
        _ = unsafe { DestroyIcon(icon) };
        // SAFETY: a live factory.
        let converter = unsafe { self.imaging.CreateFormatConverter() }?;
        // SAFETY: the source bitmap is live and the format GUID static.
        unsafe {
            converter.Initialize(
                &bitmap?,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeMedianCut,
            )
        }?;
        let properties = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: dpi as f32,
            dpiY: dpi as f32,
        };
        // SAFETY: the converter is a live bitmap source; `properties` is
        // valid for the call.
        unsafe { target.CreateBitmapFromWicBitmap(&converter, Some(&properties)) }
    }

    /// Draws `frame` into `window`.
    pub fn paint(&mut self, window: HWND, frame: &Frame<'_>) -> Result<()> {
        if self.target.is_none() {
            self.target = Some(self.make_target(window)?);
        }
        let Some(target) = &self.target else { return Ok(()) };
        let canvas = Canvas { target: &target.target, brush: &target.brush, painter: self };
        // SAFETY: a live target, drawn from this thread only.
        unsafe { target.target.BeginDraw() };
        canvas.frame(frame, target.icon.as_ref());
        // SAFETY: matches the BeginDraw above.
        let drawn = unsafe { target.target.EndDraw(None, None) };
        match drawn {
            // The device went away, e.g. a driver update: draw again with a
            // new target.
            Err(error) if error.code() == D2DERR_RECREATE_TARGET => {
                self.target = None;
                Ok(())
            }
            other => other,
        }
    }
}

impl Measure for Painter {
    fn measure(&self, text: &str, style: Style, width: f32) -> (f32, f32) {
        let mut metrics = DWRITE_TEXT_METRICS::default();
        let measured = self.text_layout(text, style, width).and_then(|layout| {
            // SAFETY: `metrics` is a valid out-pointer.
            unsafe { layout.GetMetrics(&mut metrics) }
        });
        match measured {
            Ok(()) => (metrics.widthIncludingTrailingWhitespace, metrics.height),
            // Text that cannot be measured is drawn on one line's room.
            Err(_) => (width, 20.0),
        }
    }
}

fn client_size(window: HWND) -> D2D_SIZE_U {
    let mut client = RECT::default();
    // SAFETY: `window` is a live window and `client` a valid out-pointer.
    _ = unsafe { GetClientRect(window, &mut client) };
    D2D_SIZE_U {
        width: u32::try_from(client.right - client.left).unwrap_or(0),
        height: u32::try_from(client.bottom - client.top).unwrap_or(0),
    }
}

/// A frame being drawn.
struct Canvas<'a> {
    target: &'a ID2D1HwndRenderTarget,
    brush: &'a ID2D1SolidColorBrush,
    painter: &'a Painter,
}

impl Canvas<'_> {
    fn color(&self, color: Color) -> &ID2D1SolidColorBrush {
        // SAFETY: a valid colour on a live brush.
        unsafe { self.brush.SetColor(&d2d_color(color)) };
        self.brush
    }

    fn clear(&self, color: Color) {
        // SAFETY: a valid colour on a target between BeginDraw and EndDraw.
        unsafe { self.target.Clear(Some(&d2d_color(color))) };
    }

    fn fill_rounded(&self, rect: &Rect, radius: f32, color: Color) {
        let rounded = D2D1_ROUNDED_RECT { rect: d2d_rect(rect), radiusX: radius, radiusY: radius };
        // SAFETY: `rounded` is valid for the call; the brush is live.
        unsafe { self.target.FillRoundedRectangle(&rounded, self.color(color)) };
    }

    fn stroke_rounded(&self, rect: &Rect, radius: f32, width: f32, color: Color) {
        // The stroke is centred on the edge: inset by half its width so
        // that it stays inside `rect`, on whole pixels.
        let inset = Rect {
            x: rect.x + width / 2.0,
            y: rect.y + width / 2.0,
            width: rect.width - width,
            height: rect.height - width,
        };
        let rounded =
            D2D1_ROUNDED_RECT { rect: d2d_rect(&inset), radiusX: radius, radiusY: radius };
        // SAFETY: `rounded` is valid for the call; brush and style are live.
        unsafe { self.target.DrawRoundedRectangle(&rounded, self.color(color), width, None) };
    }

    fn line(&self, from: Vector2, to: Vector2, width: f32, color: Color) {
        // SAFETY: brush and stroke style are live.
        unsafe { self.target.DrawLine(from, to, self.color(color), width, &self.painter.round) };
    }

    fn circle(&self, center: Vector2, radius: f32, stroke: Option<f32>, color: Color) {
        let ellipse = D2D1_ELLIPSE { point: center, radiusX: radius, radiusY: radius };
        match stroke {
            // SAFETY: `ellipse` is valid for the call; the brush is live.
            Some(width) => unsafe {
                self.target.DrawEllipse(&ellipse, self.color(color), width, None);
            },
            // SAFETY: as above.
            None => unsafe { self.target.FillEllipse(&ellipse, self.color(color)) },
        }
    }

    /// An arc of the circle around `center`, from `start` for `sweep`
    /// turns, clockwise from twelve o'clock.
    fn arc(&self, center: Vector2, radius: f32, start: f32, sweep: f32, width: f32, color: Color) {
        let at = |turn: f32| {
            let angle = turn * 2.0 * PI;
            point(center.X + radius * angle.sin(), center.Y - radius * angle.cos())
        };
        // SAFETY: a live factory.
        let Ok(path) = (unsafe { self.painter.factory.CreatePathGeometry() }) else { return };
        // SAFETY: a new, empty path.
        let Ok(sink) = (unsafe { path.Open() }) else { return };
        // SAFETY: the sink is open; one figure is begun and ended.
        unsafe { sink.BeginFigure(at(start), D2D1_FIGURE_BEGIN_HOLLOW) };
        let segment = D2D1_ARC_SEGMENT {
            point: at(start + sweep),
            size: D2D_SIZE_F { width: radius, height: radius },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: if sweep > 0.5 { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
        };
        // SAFETY: `segment` is valid for the call, inside the open figure.
        unsafe { sink.AddArc(&segment) };
        // SAFETY: ends the figure begun above.
        unsafe { sink.EndFigure(D2D1_FIGURE_END_OPEN) };
        // SAFETY: the figure is complete.
        if unsafe { sink.Close() }.is_err() {
            return;
        }
        // SAFETY: a closed path; brush and stroke style are live.
        unsafe { self.target.DrawGeometry(&path, self.color(color), width, &self.painter.round) };
    }

    fn text(&self, text: &str, style: Style, rect: &Rect, color: Color) {
        let Ok(layout) = self.painter.text_layout(text, style, rect.width) else { return };
        // SAFETY: a live layout and brush.
        unsafe {
            self.target.DrawTextLayout(
                point(rect.x, rect.y),
                &layout,
                self.color(color),
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
    }

    /// `text` in the middle of `rect`, on one line.
    fn centred_text(&self, text: &str, rect: &Rect, color: Color) {
        let Ok(layout) = self.painter.text_layout(text, Style::Body, rect.width) else { return };
        // SAFETY: valid alignments on a live layout.
        _ = unsafe { layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER) };
        // SAFETY: as above.
        _ = unsafe { layout.SetMaxHeight(rect.height) };
        // SAFETY: as above.
        _ = unsafe { layout.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER) };
        // SAFETY: a live layout and brush.
        unsafe {
            self.target.DrawTextLayout(
                point(rect.x, rect.y),
                &layout,
                self.color(color),
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
    }

    fn frame(&self, frame: &Frame<'_>, icon: Option<&ID2D1Bitmap>) {
        let Frame { screen, layout, palette, .. } = frame;
        self.clear(match frame.backdrop {
            Backdrop::Mica => Color::TRANSPARENT,
            Backdrop::Solid => palette.background,
        });
        self.fill_rounded(&layout.card, CARD_RADIUS, palette.card);
        self.stroke_rounded(&layout.card, CARD_RADIUS, 1.0, palette.card_stroke);
        self.badge(screen.badge, &layout.badge, palette, icon);
        self.text(&screen.headline, Style::Headline, &layout.headline, palette.text);
        for (row, (mark, text)) in screen.rows.iter().zip(&layout.rows) {
            self.mark(row.mark, mark, palette, frame.spin);
            let color =
                if row.mark == Mark::Pending { palette.text_secondary } else { palette.text };
            self.text(row.text, Style::Body, text, color);
        }
        if let (Some(reason), Some(rect)) = (&screen.reason, &layout.reason) {
            self.text(reason, Style::Body, rect, palette.text);
        }
        if let (Some(bar), Some(rect)) = (screen.bar, &layout.bar) {
            // Fluent's bar: a hairline track and a thicker, rounded fill.
            let middle = rect.y + BAR / 2.0;
            self.line(
                point(rect.x, middle),
                point(rect.right(), middle),
                1.0,
                palette.strong_stroke,
            );
            let filled = rect.width * bar.position.min(BAR_END) as f32 / BAR_END as f32;
            let fill = Rect { width: filled.max(BAR), ..*rect };
            let color = if bar.failed { palette.critical } else { palette.accent };
            self.fill_rounded(&fill, BAR / 2.0, color);
        }
        if let (Some(note), Some(rect)) = (screen.note, &layout.note) {
            self.text(note, Style::Caption, rect, palette.text_secondary);
        }
        for (index, (button, rect)) in screen.buttons.iter().zip(&layout.buttons).enumerate() {
            let (rest, hover, pressed) = if button.default {
                (palette.accent, palette.accent_hover, palette.accent_pressed)
            } else {
                (palette.button, palette.button_hover, palette.button_pressed)
            };
            let fill = if frame.pressed == Some(index) {
                pressed
            } else if frame.hover == Some(index) {
                hover
            } else {
                rest
            };
            self.fill_rounded(rect, BUTTON_RADIUS, fill);
            if !button.default {
                self.stroke_rounded(rect, BUTTON_RADIUS, 1.0, palette.button_stroke);
            }
            self.centred_text(button.label, rect, palette.text_on(fill));
            if frame.focus == Some(index) {
                let ring = Rect {
                    x: rect.x - FOCUS_GAP,
                    y: rect.y - FOCUS_GAP,
                    width: rect.width + 2.0 * FOCUS_GAP,
                    height: rect.height + 2.0 * FOCUS_GAP,
                };
                self.stroke_rounded(&ring, BUTTON_RADIUS + FOCUS_GAP, 2.0, palette.focus);
            }
        }
    }

    fn badge(&self, badge: Badge, rect: &Rect, palette: &Palette, icon: Option<&ID2D1Bitmap>) {
        let center = point(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
        match (badge, icon) {
            (Badge::App, Some(icon)) => {
                // SAFETY: a live bitmap made for this target.
                unsafe {
                    self.target.DrawBitmap(
                        icon,
                        Some(&d2d_rect(rect)),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        None,
                    );
                }
            }
            // No icon to show: the accent's ring, so the headline still has
            // something at its side.
            (Badge::App, None) => self.circle(center, 13.0, Some(2.0), palette.accent),
            (Badge::Error, _) => {
                self.circle(center, 14.0, None, palette.critical);
                let arm = 5.0;
                let cross = |dx: f32| {
                    self.line(
                        point(center.X - dx, center.Y - arm),
                        point(center.X + dx, center.Y + arm),
                        2.0,
                        palette.on_critical,
                    );
                };
                cross(arm);
                cross(-arm);
            }
        }
    }

    fn mark(&self, mark: Mark, rect: &Rect, palette: &Palette, spin: f32) {
        let (x, y) = (rect.x, rect.y);
        let center = point(x + MARK / 2.0, y + MARK / 2.0);
        match mark {
            Mark::Done => {
                let (a, b, c) =
                    (point(x + 3.5, y + 8.5), point(x + 6.5, y + 11.5), point(x + 12.5, y + 4.5));
                self.line(a, b, 1.5, palette.accent);
                self.line(b, c, 1.5, palette.accent);
            }
            Mark::Current => {
                // A quarter of a ring, turning; the hairline ring under it
                // shows where it runs.
                self.circle(center, 6.0, Some(1.0), palette.strong_stroke.faded(0.5));
                self.arc(center, 6.0, spin, 0.3, 2.0, palette.accent);
            }
            Mark::Pending => self.circle(center, 6.0, Some(1.0), palette.strong_stroke),
            Mark::Failed => {
                let (near, far) = (4.5, 11.5);
                self.line(
                    point(x + near, y + near),
                    point(x + far, y + far),
                    1.5,
                    palette.critical,
                );
                self.line(
                    point(x + far, y + near),
                    point(x + near, y + far),
                    1.5,
                    palette.critical,
                );
            }
        }
    }
}
