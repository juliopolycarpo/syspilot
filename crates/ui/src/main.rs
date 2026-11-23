//! SysPilot desktop app.
//!
//! A view over what the service reports. It holds no logic about the
//! system and changes nothing itself: every change is a request to the
//! service, which decides whether this user may make it.
#![windows_subsystem = "windows"]

mod app;
mod backend;
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
}

/// Parses the arguments after the program name.
///
/// `--instance <name>`, `--demo [scenario]`, `--page <tag>`,
/// `--level simple|advanced`.
fn parse(arguments: &[String]) -> Result<Options, String> {
    let mut options =
        Options { instance: Instance::product(), demo: None, page: Page::Home, level: None };
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
                    "(argument): expected --instance, --demo, --page or --level | received: {other}"
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
        arguments,
        install_arguments: vec![
            "--instance".to_owned(),
            options.instance.name().to_owned(),
            "install".to_owned(),
        ],
    }
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    // A windowed program has no console to complain on; a bad argument is
    // ignored in favour of starting normally.
    let options = parse(&arguments).unwrap_or_else(|_| Options {
        instance: Instance::product(),
        demo: None,
        page: Page::Home,
        level: None,
    });
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
        let options =
            parse(&arguments("--instance SysPilot-Test.ui --page privacy --level advanced"))
                .unwrap();
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
    fn a_demo_keeps_no_preferences_and_installs_its_own_instance() {
        let demo = launch(&parse(&arguments("--demo")).unwrap(), Vec::new());
        assert_eq!(demo.preferences_file, None);
        let test = launch(&parse(&arguments("--instance SysPilot-Test.ui")).unwrap(), Vec::new());
        assert_eq!(test.install_arguments, ["--instance", "SysPilot-Test.ui", "install"]);
        if let Some(file) = test.preferences_file {
            assert!(file.ends_with(r"SysPilot-Test.ui\app.toml"), "{file:?}");
        }
    }
}
