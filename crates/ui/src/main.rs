//! SysPilot desktop app.
#![windows_subsystem = "windows"]

use syspilot_proto::{Client, Instance};
use windows_reactor::*;

/// Toolkit spike: one screen that connects to the service and lists tweaks.
struct Spike {
    lines: Vec<(String, String)>,
}

impl Component for Spike {
    type Input = Instance;
    type Message = ();

    fn create(instance: &Instance, _context: &ComponentContext<Self>) -> Self {
        let lines = match Client::connect(instance).and_then(|mut client| client.catalog()) {
            Ok(catalog) => {
                catalog.tweaks.into_iter().map(|tweak| (tweak.id, tweak.title)).collect()
            }
            Err(error) => vec![("error".to_owned(), error.to_string())],
        };
        Self { lines }
    }

    fn view(&self, _input: &Instance, context: &mut ViewContext<Self>) -> View {
        context.window_title("SysPilot");
        context.window_visuals(
            WindowVisuals::new().backdrop(WindowBackdrop::Mica).client_size(1100.0, 720.0),
        );
        let rows = self.lines.iter().map(|(id, title)| {
            KeyedView::new(
                id.clone(),
                Border::new()
                    .padding(12.0)
                    .corner_radius(4.0)
                    .border_thickness(1.0)
                    .background(ThemeBrush::CardBackground)
                    .border_brush(ThemeBrush::CardStroke)
                    .content(
                        Grid::new().columns([GridLength::Star(1.0), GridLength::Auto]).children((
                            TextBlock::new().text(title.clone()).grid_column(0),
                            ToggleSwitch::new().grid_column(1),
                        )),
                    ),
            )
        });
        let page = ScrollViewer::new().content(
            Border::new()
                .padding(Thickness::new(36.0, 24.0, 36.0, 36.0))
                .content(StackPanel::new().spacing(4.0).keyed_children(rows)),
        );
        let item = |tag: &str, label: &str, symbol: Symbol, selected: bool| {
            KeyedView::new(
                tag.to_owned(),
                NavigationViewItem::new().tag(tag).is_selected(selected).slots([
                    SlotView::new(NavigationViewItemSlot::Content, label),
                    SlotView::new(NavigationViewItemSlot::Icon, SymbolIcon::new().symbol(symbol)),
                ]),
            )
        };
        NavigationView::new()
            .is_settings_visible(false)
            .is_back_button_visible(NavigationViewBackButtonVisible::Collapsed)
            .slots([
                SlotView::collection(
                    NavigationViewSlot::MenuItems,
                    [
                        item("home", "Home", Symbol::Home, false),
                        item("tweaks", "Tweaks", Symbol::Setting, true),
                    ],
                ),
                SlotView::new(NavigationViewSlot::Content, page),
            ])
    }
}

fn main() {
    let instance = std::env::args()
        .nth(1)
        .and_then(|name| Instance::named(&name).ok())
        .unwrap_or_else(Instance::product);
    if let Err(error) = App::run_component::<Spike>(instance) {
        eprintln!("{error:?}");
    }
}
