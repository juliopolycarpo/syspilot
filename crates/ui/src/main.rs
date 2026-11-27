//! SysPilot desktop app.
//!
//! A view over what the service reports. It holds no logic about the
//! system and changes nothing itself: every change is a request to the
//! service, which decides whether this user may make it.
#![windows_subsystem = "windows"]

mod app;
mod backend;
mod contrast;
mod demo;
mod form;
mod launch;
mod model;
mod pages;
mod preferences;
mod widgets;

use std::path::PathBuf;
use std::sync::Arc;

use syspilot_proto::Instance;
use windows_reactor::App as Reactor;

use crate::app::{App, Launch, Page};
use crate::backend::{Backend, BackendHandle, ServiceBackend};
use crate::demo::{DemoBackend, Scenario};
use crate::model::ViewLevel;
use crate::preferences::Theme;

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    /// Service instance to talk to.
    instance: Instance,
    /// Demo scenario to play instead of talking to a service.
    demo: Option<Scenario>,
    /// Page to open on.
    page: Page,
    /// Level for this run.
    level: Option<ViewLevel>,
    /// Colours for this run.
    theme: Option<Theme>,
    /// Started by [`launch::relaunch_elevated`]: asking for elevation
    /// again would not change the answer.
    relaunched: bool,
}

impl Options {
    /// What no arguments mean: the product's service, as the user left it.
    fn plain() -> Self {
        Self {
            instance: Instance::product(),
            demo: None,
            page: Page::Home,
            level: None,
            theme: None,
            relaunched: false,
        }
    }

    /// Whether the app should restart with administrator rights before it
    /// opens a window. Most of what it does needs them, and a window that
    /// silently runs without is easy to mistake for one that has them. A
    /// demo changes nothing, and a copy that was already restarted once
    /// stays as it is whatever Windows made of it (no prompt loop).
    fn wants_elevation(&self, elevated: bool) -> bool {
        !elevated && self.demo.is_none() && !self.relaunched
    }
}

/// Parses the arguments after the program name.
///
/// `--instance <name>`, `--demo [scenario]`, `--page <tag>`,
/// `--level simple|advanced`, `--theme light|dark`.
fn parse(arguments: &[String]) -> Result<Options, String> {
    let mut options = Options::plain();
    let mut rest = arguments.iter().peekable();
    while let Some(argument) = rest.next() {
        let mut value = |what: &str| {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("({argument}): expected {what} | received: nothing"))
        };
        match argument.as_str() {
            "--instance" => {
                let name = value("an instance name")?;
                options.instance = Instance::named(&name).map_err(|error| error.to_string())?;
            }
            "--page" => {
                let tag = value("a page name")?;
                options.page = Page::from_tag(&tag).ok_or_else(|| {
                    let known: Vec<&str> = Page::ALL.iter().map(|page| page.tag()).collect();
                    format!("(--page): expected one of {} | received: {tag}", known.join(", "))
                })?;
            }
            "--level" => {
                options.level = Some(match value("simple or advanced")?.as_str() {
                    "simple" => ViewLevel::Simple,
                    "advanced" => ViewLevel::Advanced,
                    other => {
                        return Err(format!(
                            "(--level): expected simple | advanced | received: {other}"
                        ));
                    }
                });
            }
            "--theme" => {
                options.theme = Some(match value("light or dark")?.as_str() {
                    "light" => Theme::Light,
                    "dark" => Theme::Dark,
                    other => {
                        return Err(format!(
                            "(--theme): expected light | dark | received: {other}"
                        ));
                    }
                });
            }
            launch::RELAUNCHED_ARGUMENT => options.relaunched = true,
            "--demo" => {
                // The scenario is optional: `--demo --page home` is valid.
                let named = rest.peek().and_then(|next| Scenario::parse(next));
                if named.is_some() {
                    rest.next();
                }
                options.demo = Some(named.unwrap_or(Scenario::Healthy));
            }
            other => {
                return Err(format!(
                    "(argument): expected --instance, --demo, --page, --level or --theme | received: {other}"
                ));
            }
        }
    }
    Ok(options)
}

fn launch(options: &Options, arguments: Vec<String>) -> Launch {
    let backend: Arc<dyn Backend> = match options.demo {
        Some(scenario) => Arc::new(DemoBackend::new(scenario)),
        None => Arc::new(ServiceBackend::new(options.instance.clone())),
    };
    // A demo must not change what the real app remembers.
    let preferences_file = match options.demo {
        Some(_) => None,
        None => std::env::var_os("LOCALAPPDATA")
            .map(|root| preferences::file_in(&PathBuf::from(root), options.instance.name())),
    };
    Launch {
        backend: BackendHandle(backend),
        preferences_file,
        page: options.page,
        level: options.level,
        theme: options.theme,
        arguments,
        install_arguments: vec![
            "--instance".to_owned(),
            options.instance.name().to_owned(),
            "install".to_owned(),
        ],
        companion_instance: options.demo.is_none().then(|| options.instance.clone()),
    }
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    // A windowed program has no console to complain on; a bad argument is
    // ignored in favour of starting normally, and so are the other
    // arguments, so that the ones passed on to a restart are all understood.
    let (options, arguments) = match parse(&arguments) {
        Ok(options) => (options, arguments),
        Err(_) => (Options::plain(), Vec::new()),
    };
    // Before any window exists, so that the unelevated copy never shows one.
    // If the prompt is refused, carry on without rights: the app then shows
    // everything read-only and offers to restart elevated.
    if options.wants_elevation(launch::is_elevated())
        && launch::relaunch_elevated(&arguments).is_ok()
    {
        return;
    }
    if let Err(error) = Reactor::run_component::<App>(launch(&options, arguments)) {
        eprintln!("{error:?}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syspilot_proto::catalog::Category;

    fn arguments(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn no_arguments_mean_the_product_service_and_the_home_page() {
        let options = parse(&[]).unwrap();
        assert!(options.instance.is_product());
        assert_eq!((options.demo, options.page, options.level), (None, Page::Home, None));
    }

    #[test]
    fn every_option_is_understood() {
        let options = parse(&arguments(
            "--instance SysPilot-Test.ui --page privacy --level advanced --theme light",
        ))
        .unwrap();
        assert_eq!(options.theme, Some(Theme::Light));
        assert_eq!(
            parse(&arguments("--theme pink")).unwrap_err(),
            "(--theme): expected light | dark | received: pink"
        );
        assert_eq!(options.instance.name(), "SysPilot-Test.ui");
        assert_eq!(options.page, Page::Tweaks(Category::Privacy));
        assert_eq!(options.level, Some(ViewLevel::Advanced));
    }

    #[test]
    fn demo_scenario_is_optional() {
        assert_eq!(parse(&arguments("--demo")).unwrap().demo, Some(Scenario::Healthy));
        assert_eq!(parse(&arguments("--demo no-driver")).unwrap().demo, Some(Scenario::NoDriver));
        let followed = parse(&arguments("--demo --page sensors")).unwrap();
        assert_eq!((followed.demo, followed.page), (Some(Scenario::Healthy), Page::Sensors));
    }

    #[test]
    fn mistakes_name_what_was_received_and_what_is_valid() {
        assert_eq!(
            parse(&arguments("--level expert")).unwrap_err(),
            "(--level): expected simple | advanced | received: expert"
        );
        assert!(parse(&arguments("--page")).unwrap_err().contains("received: nothing"));
        let unknown = parse(&arguments("--page nowhere")).unwrap_err();
        assert!(
            unknown.contains("look-and-feel") && unknown.ends_with("received: nowhere"),
            "{unknown}"
        );
        assert!(parse(&arguments("--verbose")).unwrap_err().ends_with("received: --verbose"));
    }

    #[test]
    fn a_plain_start_without_rights_asks_for_them() {
        let plain = parse(&[]).unwrap();
        assert!(plain.wants_elevation(false), "(plain start): expected to ask for elevation");
        assert!(!plain.wants_elevation(true), "(elevated start): expected not to ask");
    }

    #[test]
    fn a_demo_or_a_restarted_copy_does_not_ask_for_elevation() {
        let demo = parse(&arguments("--demo")).unwrap();
        assert!(!demo.wants_elevation(false), "(--demo): expected not to ask");
        let restarted = parse(&arguments(launch::RELAUNCHED_ARGUMENT)).unwrap();
        assert!(restarted.relaunched);
        assert!(!restarted.wants_elevation(false), "(restarted copy): expected not to ask again");
    }

    #[test]
    fn the_restart_marker_keeps_the_other_options() {
        let restarted = parse(&arguments("--page sensors --elevated-restart")).unwrap();
        assert_eq!((restarted.page, restarted.relaunched), (Page::Sensors, true));
    }

    #[test]
    fn a_demo_keeps_no_preferences_and_installs_its_own_instance() {
        let demo = launch(&parse(&arguments("--demo")).unwrap(), Vec::new());
        assert_eq!(demo.preferences_file, None);
        assert!(demo.companion_instance.is_none(), "(demo): expected no update companion");
        let test = launch(&parse(&arguments("--instance SysPilot-Test.ui")).unwrap(), Vec::new());
        assert_eq!(test.install_arguments, ["--instance", "SysPilot-Test.ui", "install"]);
        assert_eq!(test.companion_instance.as_ref().map(Instance::name), Some("SysPilot-Test.ui"));
        if let Some(file) = test.preferences_file {
            assert!(file.ends_with(r"SysPilot-Test.ui\app.toml"), "{file:?}");
        }
    }
}
