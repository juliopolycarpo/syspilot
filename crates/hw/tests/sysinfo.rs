//! Smoke test of `sysinfo::collect()` against the real machine.
//!
//! Hardware differs from machine to machine, so the checks are about shape
//! and plausibility, not values: what any Windows 10/11 PC must have.

use syspilot_hw::sysinfo;

#[test]
fn collect_describes_this_machine_with_plausible_facts() {
    let info = sysinfo::collect();
    let summary = &info.summary;
    assert!(!summary.computer.is_empty(), "summary.computer: expected a name | received: empty");
    assert!(
        summary.os.starts_with("Windows"),
        "summary.os: expected \"Windows ...\" | received: {:?}",
        summary.os
    );
    assert!(!summary.cpu.is_empty(), "summary.cpu: expected a name | received: empty");
    assert!(
        summary.memory_bytes >= 512 << 20,
        "summary.memory_bytes: expected >= 512 MiB | received: {}",
        summary.memory_bytes
    );
    assert!(
        summary.memory_bytes <= 64 << 40,
        "summary.memory_bytes: expected <= 64 TiB | received: {}",
        summary.memory_bytes
    );
}

#[test]
fn sections_are_well_formed() {
    let info = sysinfo::collect();
    let titles: Vec<&str> = info.sections.iter().map(|section| section.title.as_str()).collect();
    for required in ["System", "Processor", "Operating system"] {
        assert!(
            titles.contains(&required),
            "sections: expected {required:?} | received: {titles:?}"
        );
    }
    for section in &info.sections {
        assert!(!section.title.is_empty(), "section title: expected non-empty | received: empty");
        assert!(
            !section.rows.is_empty(),
            "section {:?}: expected at least one row | received: none",
            section.title
        );
        for (label, value) in &section.rows {
            assert!(
                !label.is_empty() && !value.is_empty(),
                "section {:?} row: expected a label and a value | received: ({label:?}, {value:?})",
                section.title
            );
            for text in [label, value] {
                assert!(
                    text == text.trim() && !text.chars().any(char::is_control),
                    "section {:?}: expected trimmed text without control characters | received: {text:?}",
                    section.title
                );
            }
        }
    }
    let mut unique = titles.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        titles.len(),
        "section titles: expected all different | received: {titles:?}"
    );
}

#[test]
fn collecting_twice_gives_the_same_answer() {
    // Nothing in the answer is a live reading, so two calls must agree.
    assert_eq!(sysinfo::collect(), sysinfo::collect());
}
