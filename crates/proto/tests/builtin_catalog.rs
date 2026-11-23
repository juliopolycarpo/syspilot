//! Validates the built-in tweak catalog in `catalog/tweaks/`.
//!
//! The schema checks in `Catalog::parse_all` guarantee that every entry is
//! well-formed. This file adds the editorial rules from `docs/tweak-catalog.md`:
//! sources, wording, levels and coverage. Each rule is its own test and reports
//! every offending entry, one per line, as
//! `(catalog entry "<id>"): expected <shape> | received: <value>`.
//!
//! The rule checks are plain functions over strings, so the tests at the bottom
//! can prove that each rule fires without needing a bad catalog on disk.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::fs;
use std::path::{Path, PathBuf};

use syspilot_proto::action::Action;
use syspilot_proto::catalog::{Catalog, Category, Direction, Level, Tweak};

/// Hosts a `source` URL may point to: primary Microsoft documentation only.
const ALLOWED_SOURCE_HOSTS: [&str; 7] = [
    "learn.microsoft.com",
    "support.microsoft.com",
    "devblogs.microsoft.com",
    "techcommunity.microsoft.com",
    "www.microsoft.com",
    "msrc.microsoft.com",
    "docs.microsoft.com",
];

/// Windows 11 builds an entry may require: 21H2, 22H2 and 24H2.
const ALLOWED_MIN_BUILDS: [u32; 3] = [22000, 22621, 26100];

/// Longest title that fits the list view without truncation.
const MAX_TITLE_CHARS: usize = 60;

/// Capitalized words allowed after the first word of a title: product and
/// feature names as Microsoft writes them. Everything else that is capitalized
/// is treated as Title Case. Acronyms (`SMB`, `NTLMv2`) are never flagged
/// because their second letter is also upper case. Add a word here only when
/// it is a name, not to silence a Title Case title.
const PROPER_NOUNS: &[&str] = &[
    "Windows",
    "Update",
    "Microsoft",
    "Defender",
    "Edge",
    "Bing",
    "Copilot",
    "Recall",
    "Widgets",
    "Cortana",
    "Spotlight",
    "PowerShell",
    "Bluetooth",
    "Xbox",
    "OneDrive",
    "AutoRun",
    "AutoPlay",
    "Wi-Fi",
    "Store",
    "Start",
    "File",
    "Explorer",
    "Search",
    "Storage",
    "Sense",
    "Find",
    "My",
    "Device",
    "Remote",
    "Desktop",
    "Assistance",
    "Error",
    "Reporting",
    "Secure",
    "Boot",
    "Spectre",
    "Meltdown",
];

/// Verbs a title must not start with: titles say "Turn on" and "Turn off".
const FORBIDDEN_TITLE_VERBS: [&str; 4] = ["Disable", "Enable", "Activate", "Deactivate"];

/// Marketing words that never belong in a title or summary.
const FORBIDDEN_WORDS: [&str; 5] = ["boost", "supercharge", "blazing", "turbo", "ultimate"];

/// Detect-only entries the catalog must carry (inclusive).
const AUDIT_ENTRIES: std::ops::RangeInclusive<usize> = 5..=10;

fn catalog_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("catalog").join("tweaks")
}

/// Every `*.toml` file of the catalog directory as `(file stem, text)`, sorted
/// by name so failures are stable.
fn catalog_files() -> Vec<(String, String)> {
    let dir = catalog_dir();
    let entries = fs::read_dir(&dir).unwrap_or_else(|error| {
        panic!(
            "(catalog directory {}): expected a readable directory | received: {error}",
            dir.display()
        )
    });
    let mut files: Vec<(String, String)> = entries
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "toml"))
        .map(|path| {
            let stem = path.file_stem().expect("a file name").to_string_lossy().into_owned();
            let text = fs::read_to_string(&path).unwrap_or_else(|error| {
                panic!("(catalog file {}): expected UTF-8 text | received: {error}", path.display())
            });
            (stem, text)
        })
        .collect();
    files.sort();
    files
}

/// Parses the whole catalog. Each file is parsed on its own first so a syntax
/// error names the file, then all files together so duplicate IDs are caught.
fn load_catalog() -> Catalog {
    let files = catalog_files();
    for (stem, text) in &files {
        if let Err(error) = Catalog::parse(text) {
            panic!("(catalog file {stem}.toml): expected a valid catalog | received: {error}");
        }
    }
    Catalog::parse_all(files.iter().map(|(_, text)| text.as_str()))
        .unwrap_or_else(|error| panic!("(catalog): expected unique entries | received: {error}"))
}

fn category_name(category: Category) -> &'static str {
    match category {
        Category::Performance => "performance",
        Category::Privacy => "privacy",
        Category::Security => "security",
        Category::Reliability => "reliability",
        Category::Other => "other",
    }
}

fn problem(id: &str, expected: impl Display, received: impl Display) -> String {
    format!("(catalog entry {id:?}): expected {expected} | received: {received}")
}

fn assert_clean(rule: &str, problems: &[String]) {
    assert!(problems.is_empty(), "{rule}: {} problem(s)\n{}", problems.len(), problems.join("\n"));
}

fn is_title_cased(word: &str) -> bool {
    let mut letters = word.chars();
    matches!((letters.next(), letters.next()), (Some(first), Some(second))
        if first.is_uppercase() && second.is_lowercase())
}

fn title_problems(id: &str, title: &str) -> Vec<String> {
    let mut found = Vec::new();
    let length = title.chars().count();
    if length > MAX_TITLE_CHARS {
        found.push(problem(id, format!("title <= {MAX_TITLE_CHARS} chars"), length));
    }
    if title.trim_end().ends_with('.') {
        found.push(problem(id, "title without a trailing period", format!("{title:?}")));
    }
    if !title.chars().next().is_some_and(char::is_uppercase) {
        found.push(problem(id, "title starting with a capital letter", format!("{title:?}")));
    }
    let first_word = title.split_whitespace().next().unwrap_or_default();
    if FORBIDDEN_TITLE_VERBS.contains(&first_word) {
        found.push(problem(id, "title starting with \"Turn on\" or \"Turn off\"", first_word));
    }
    let title_cased: Vec<&str> = title
        .split_whitespace()
        .skip(1)
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-'))
        .filter(|word| is_title_cased(word) && !PROPER_NOUNS.contains(word))
        .collect();
    if !title_cased.is_empty() {
        found.push(problem(
            id,
            "sentence case (only the first word and proper nouns capitalized)",
            format!("capitalized words {title_cased:?}"),
        ));
    }
    found
}

fn summary_problems(id: &str, summary: &str) -> Vec<String> {
    let mut found = Vec::new();
    if !summary.trim_end().ends_with('.') {
        found.push(problem(id, "summary ending with a period", format!("{summary:?}")));
    }
    found
}

/// Wording rules that apply to every text field a user reads.
fn wording_problems(id: &str, field: &str, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    if text.contains('!') {
        found.push(problem(id, format!("{field} without exclamation marks"), format!("{text:?}")));
    }
    let lowered = text.to_lowercase();
    for word in FORBIDDEN_WORDS {
        if lowered.contains(word) {
            found.push(problem(
                id,
                format!("{field} without the word {word:?}"),
                format!("{text:?}"),
            ));
        }
    }
    found
}

fn is_locale_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.len() == 5
        && bytes[2] == b'-'
        && bytes.iter().enumerate().all(|(i, b)| i == 2 || b.is_ascii_lowercase())
}

fn source_problem(id: &str, source: &str) -> Option<String> {
    let Some(rest) = source.strip_prefix("https://") else {
        return Some(problem(id, "source starting with https://", format!("{source:?}")));
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if !ALLOWED_SOURCE_HOSTS.contains(&host) {
        return Some(problem(id, format!("source host in {ALLOWED_SOURCE_HOSTS:?}"), host));
    }
    if is_locale_segment(path.split('/').next().unwrap_or_default()) {
        return Some(problem(id, "locale-free source URL (no /en-us/)", source));
    }
    None
}

/// The system state an action manages, lower-cased because registry paths,
/// service names and GUIDs compare case-insensitively. Commands have no target.
fn target_of(action: &Action) -> Option<String> {
    let target = match action {
        Action::Registry(registry) => {
            format!("registry {:?}\\{}\\{}", registry.scope, registry.key, registry.name)
        }
        Action::Service(service) => format!("service {}", service.service),
        Action::Power(power) => format!("power {}/{}", power.subgroup, power.setting),
        Action::Command(_) => return None,
    };
    Some(target.to_lowercase())
}

/// Targets claimed by more than one entry. Two entries that write the same
/// value would fight over it, so each target belongs to exactly one entry.
fn shared_target_problems(tweaks: &[Tweak]) -> Vec<String> {
    let mut owners: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for tweak in tweaks {
        let targets: BTreeSet<String> = tweak.actions.iter().filter_map(target_of).collect();
        for target in targets {
            owners.entry(target).or_default().push(&tweak.id);
        }
    }
    owners
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(target, ids)| {
            problem(
                ids[1],
                format!("a target owned by one entry ({target})"),
                format!("also in {:?}", ids[0]),
            )
        })
        .collect()
}

#[test]
fn every_category_has_one_file_and_its_entries_live_in_it() {
    // Fails first, and names the file, when any file does not parse.
    load_catalog();
    let files = catalog_files();
    let stems: Vec<&str> = files.iter().map(|(stem, _)| stem.as_str()).collect();
    let mut expected: Vec<&str> = Category::ALL.iter().map(|&c| category_name(c)).collect();
    expected.sort_unstable();
    assert_eq!(
        stems, expected,
        "(catalog directory): expected one file per category | received: {stems:?}"
    );

    let mut problems = Vec::new();
    for (stem, text) in &files {
        let parsed = Catalog::parse(text).expect("load_catalog accepted every file");
        for tweak in parsed.tweaks {
            if category_name(tweak.category) != stem {
                problems.push(problem(
                    &tweak.id,
                    format!("category {stem:?} to match {stem}.toml"),
                    category_name(tweak.category),
                ));
            }
        }
    }
    assert_clean("file layout", &problems);
}

#[test]
fn every_category_and_both_levels_have_entries() {
    let catalog = load_catalog();
    for category in Category::ALL {
        let count = catalog.tweaks.iter().filter(|t| t.category == category).count();
        assert!(
            count > 0,
            "(catalog): expected entries in category {} | received: {count}",
            category_name(category)
        );
    }
    for level in [Level::Simple, Level::Advanced] {
        let count = catalog.tweaks.iter().filter(|t| t.level == level).count();
        assert!(count > 0, "(catalog): expected entries at level {level:?} | received: {count}");
    }
}

#[test]
fn simple_covers_performance_and_advanced_covers_the_rest() {
    let catalog = load_catalog();
    let count = |level, category| {
        catalog.tweaks.iter().filter(|t| t.level == level && t.category == category).count()
    };
    assert!(
        count(Level::Simple, Category::Performance) > 0,
        "(catalog): expected simple performance entries | received: 0"
    );
    for category in [Category::Privacy, Category::Security, Category::Reliability, Category::Other]
    {
        assert!(
            count(Level::Advanced, category) > 0,
            "(catalog): expected advanced {} entries | received: 0",
            category_name(category)
        );
    }
}

#[test]
fn ids_carry_their_category_prefix() {
    let problems: Vec<String> = load_catalog()
        .tweaks
        .iter()
        .filter_map(|tweak| {
            let prefix = format!("{}.", category_name(tweak.category));
            (!tweak.id.starts_with(&prefix))
                .then(|| problem(&tweak.id, format!("id starting with {prefix:?}"), &tweak.id))
        })
        .collect();
    assert_clean("id prefixes", &problems);
}

#[test]
fn no_entry_uses_a_command_action() {
    let problems: Vec<String> = load_catalog()
        .tweaks
        .iter()
        .filter(|tweak| tweak.actions.iter().any(|a| matches!(a, Action::Command(_))))
        .map(|tweak| {
            problem(&tweak.id, "registry, service or power actions only", "a command action")
        })
        .collect();
    assert_clean("command actions", &problems);
}

#[test]
fn titles_are_sentence_case_short_and_unpunctuated() {
    let problems: Vec<String> =
        load_catalog().tweaks.iter().flat_map(|t| title_problems(&t.id, &t.title)).collect();
    assert_clean("titles", &problems);
}

#[test]
fn titles_are_unique() {
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    let catalog = load_catalog();
    let mut problems = Vec::new();
    for tweak in &catalog.tweaks {
        if let Some(first) = seen.insert(tweak.title.to_lowercase(), &tweak.id) {
            problems.push(problem(&tweak.id, "a unique title", format!("same as {first:?}")));
        }
    }
    assert_clean("unique titles", &problems);
}

#[test]
fn summaries_end_with_a_period() {
    let problems: Vec<String> =
        load_catalog().tweaks.iter().flat_map(|t| summary_problems(&t.id, &t.summary)).collect();
    assert_clean("summaries", &problems);
}

#[test]
fn text_has_no_exclamation_marks_or_marketing_words() {
    let catalog = load_catalog();
    let mut problems = Vec::new();
    for tweak in &catalog.tweaks {
        problems.extend(wording_problems(&tweak.id, "title", &tweak.title));
        problems.extend(wording_problems(&tweak.id, "summary", &tweak.summary));
        problems.extend(wording_problems(&tweak.id, "detail", &tweak.detail));
    }
    assert_clean("wording", &problems);
}

#[test]
fn sources_are_locale_free_microsoft_documentation() {
    let problems: Vec<String> = load_catalog()
        .tweaks
        .iter()
        .filter_map(|tweak| source_problem(&tweak.id, &tweak.source))
        .collect();
    assert_clean("sources", &problems);
}

#[test]
fn security_entries_are_advanced_and_carry_a_direction() {
    let catalog = load_catalog();
    let mut problems = Vec::new();
    for tweak in catalog.tweaks.iter().filter(|t| t.category == Category::Security) {
        if tweak.direction.is_none() {
            problems.push(problem(&tweak.id, "direction hardens | weakens", "none"));
        }
        if tweak.level != Level::Advanced {
            problems.push(problem(
                &tweak.id,
                "level advanced for a security entry",
                format!("{:?}", tweak.level),
            ));
        }
    }
    assert_clean("security entries", &problems);
}

#[test]
fn weakening_entries_are_advanced_and_state_the_risk() {
    let catalog = load_catalog();
    let mut problems = Vec::new();
    for tweak in catalog.tweaks.iter().filter(|t| t.weakens()) {
        if tweak.level != Level::Advanced {
            problems.push(problem(
                &tweak.id,
                "level advanced for a weakening entry",
                format!("{:?}", tweak.level),
            ));
        }
        match tweak.risk.as_deref().map(str::trim) {
            None | Some("") => {
                problems.push(problem(&tweak.id, "a non-empty risk for a weakening entry", "none"));
            }
            Some(risk) if !risk.ends_with('.') => {
                problems.push(problem(&tweak.id, "risk ending with a period", format!("{risk:?}")));
            }
            Some(_) => {}
        }
    }
    assert_clean("weakening entries", &problems);
}

#[test]
fn advanced_entries_explain_themselves() {
    let problems: Vec<String> = load_catalog()
        .tweaks
        .iter()
        .filter(|t| t.level == Level::Advanced && t.detail.trim().is_empty())
        .map(|t| problem(&t.id, "a non-empty detail for an advanced entry", "empty"))
        .collect();
    assert_clean("advanced detail", &problems);
}

#[test]
fn min_build_is_a_windows_11_release() {
    let problems: Vec<String> = load_catalog()
        .tweaks
        .iter()
        .filter_map(|tweak| {
            let build = tweak.min_build?;
            (!ALLOWED_MIN_BUILDS.contains(&build))
                .then(|| problem(&tweak.id, format!("min-build in {ALLOWED_MIN_BUILDS:?}"), build))
        })
        .collect();
    assert_clean("min-build", &problems);
}

#[test]
fn audit_entries_harden_and_are_few() {
    let catalog = load_catalog();
    let audits: Vec<&Tweak> = catalog.tweaks.iter().filter(|t| t.audit).collect();
    assert!(
        AUDIT_ENTRIES.contains(&audits.len()),
        "(catalog): expected {AUDIT_ENTRIES:?} audit entries | received: {}",
        audits.len()
    );
    let mut problems = Vec::new();
    for tweak in audits {
        if tweak.category != Category::Security {
            problems.push(problem(
                &tweak.id,
                "category security for an audit entry",
                category_name(tweak.category),
            ));
        }
        if tweak.direction != Some(Direction::Hardens) {
            problems.push(problem(
                &tweak.id,
                "direction hardens for an audit entry",
                format!("{:?}", tweak.direction),
            ));
        }
    }
    assert_clean("audit entries", &problems);
}

#[test]
fn no_two_entries_manage_the_same_setting() {
    assert_clean("shared targets", &shared_target_problems(&load_catalog().tweaks));
}

mod rule_checks {
    //! Proves each rule fires on a bad input, so a green catalog test means
    //! "compliant" and not "the rule never ran".

    use super::*;

    const TWO_ENTRIES_ONE_TARGET: &str = r#"
[[tweak]]
id = "privacy.first"
title = "First"
summary = "First."
level = "simple"
category = "privacy"
source = "https://learn.microsoft.com/windows/privacy/manage-connections"
[[tweak.action]]
registry = { scope = "user", key = 'Software\Example', name = "Value", data = { dword = 0 } }

[[tweak]]
id = "privacy.second"
title = "Second"
summary = "Second."
level = "simple"
category = "privacy"
source = "https://learn.microsoft.com/windows/privacy/manage-connections"
[[tweak.action]]
registry = { scope = "user", key = 'SOFTWARE\EXAMPLE', name = "VALUE", data = { dword = 1 } }
"#;

    /// One entry with every non-command action shape and every trigger form
    /// that `docs/tweak-catalog.md` shows, so the guide cannot drift from the
    /// schema.
    const GUIDE_ACTION_SHAPES: &str = r#"
[[tweak]]
id = "reliability.guide-shapes"
title = "Guide shapes"
summary = "Guide shapes."
level = "advanced"
category = "reliability"
source = "https://learn.microsoft.com/windows/x"
triggers = ["boot", "logon", "resume", "change", { interval = 300 }]
[[tweak.action]]
registry = { scope = "machine", key = 'SOFTWARE\Example', name = "Value", data = { dword = 1 } }
[[tweak.action]]
registry = { scope = "user", key = 'Software\Example', name = "Value", data = "absent" }
[[tweak.action]]
service = { service = "DiagTrack", start = "disabled" }
[[tweak.action]]
power = { subgroup = "0012ee47-9041-4b5d-9b77-535fba8b1442", setting = "6738e2c4-e8a5-4a42-b16a-e040e769756e", ac = 0 }
"#;

    #[test]
    fn the_action_shapes_in_the_guide_parse() {
        let catalog = Catalog::parse(GUIDE_ACTION_SHAPES)
            .unwrap_or_else(|error| panic!("the guide's action shapes must parse: {error}"));
        assert_eq!(catalog.tweaks[0].actions.len(), 4);
        assert_eq!(catalog.tweaks[0].triggers.len(), 5);
    }

    #[test]
    fn long_titles_are_reported_with_their_length() {
        let title = "x".repeat(61);
        let found = title_problems("privacy.x", &format!("T{title}"));
        assert!(
            found.contains(
                &"(catalog entry \"privacy.x\"): expected title <= 60 chars | received: 62"
                    .to_owned()
            ),
            "{found:?}"
        );
    }

    #[test]
    fn title_case_and_trailing_periods_are_reported() {
        let found = title_problems("privacy.x", "Turn Off the Advertising ID.");
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].contains("trailing period"), "{found:?}");
        assert!(found[1].contains("[\"Off\", \"Advertising\"]"), "{found:?}");
    }

    #[test]
    fn acronyms_and_proper_nouns_are_sentence_case() {
        assert!(title_problems("security.x", "Require SMB signing in Windows").is_empty());
        assert!(title_problems("security.x", "Turn off SMBv1 and NTLMv2 fallback").is_empty());
    }

    #[test]
    fn titles_must_not_start_with_disable_or_enable() {
        let found = title_problems("privacy.x", "Disable telemetry");
        assert!(found.iter().any(|p| p.contains("Turn on")), "{found:?}");
    }

    #[test]
    fn summaries_need_a_period() {
        assert_eq!(summary_problems("privacy.x", "Stops ads").len(), 1);
        assert!(summary_problems("privacy.x", "Stops ads.").is_empty());
    }

    #[test]
    fn wording_rejects_exclamation_marks_and_marketing_words() {
        assert_eq!(wording_problems("privacy.x", "summary", "Faster!").len(), 1);
        assert_eq!(wording_problems("privacy.x", "title", "Boost games").len(), 1);
        assert!(wording_problems("privacy.x", "title", "Turn off tips").is_empty());
    }

    #[test]
    fn sources_must_be_allow_listed_and_locale_free() {
        let id = "privacy.x";
        assert!(source_problem(id, "https://learn.microsoft.com/windows/x#anchor").is_none());
        let host = source_problem(id, "https://example.com/x").expect("a host problem");
        assert!(host.contains("received: example.com"), "{host}");
        let locale = source_problem(id, "https://learn.microsoft.com/en-us/windows/x");
        assert!(locale.is_some_and(|p| p.contains("locale-free")));
        assert!(source_problem(id, "http://learn.microsoft.com/x").is_some());
        assert!(source_problem(id, "https://learn.microsoft.com.evil.example/x").is_some());
    }

    #[test]
    fn two_entries_writing_one_value_are_reported_once() {
        let catalog = Catalog::parse(TWO_ENTRIES_ONE_TARGET).expect("the fixture is valid");
        let found = shared_target_problems(&catalog.tweaks);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("privacy.second") && found[0].contains("privacy.first"));
    }
}
