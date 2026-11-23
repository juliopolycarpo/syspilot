//! Recorded register values, as a small text file.
//!
//! A fixture is what a recording wrapper saw a real device return, stored as
//! one `key = value` pair per line so a diff shows exactly which register
//! changed. Replay fakes serve reads from it.
//!
//! ```text
//! # any line starting with a hash is a comment
//! msr.0.0.0x1a2 = 0x640e00
//! ```

use std::collections::BTreeMap;
use std::fmt;

/// A set of recorded values with stable, sorted keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fixture {
    header: Vec<String>,
    entries: BTreeMap<String, u64>,
}

/// A fixture file that does not follow the format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureError {
    line: usize,
    expected: &'static str,
    received: String,
}

impl fmt::Display for FixtureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "(fixture line {}): expected {} | received: {:?}",
            self.line, self.expected, self.received
        )
    }
}

impl std::error::Error for FixtureError {}

impl Fixture {
    /// An empty fixture.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a comment line written at the top of the file.
    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        self.header.push(comment.into());
        self
    }

    /// Records `value` under `key`, replacing an earlier value.
    pub fn set(&mut self, key: impl Into<String>, value: u64) {
        self.entries.insert(key.into(), value);
    }

    /// The value recorded under `key`.
    pub fn get(&self, key: &str) -> Option<u64> {
        self.entries.get(key).copied()
    }

    /// Number of recorded values.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The recorded pairs in key order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, u64)> {
        self.entries.iter().map(|(key, value)| (key.as_str(), *value))
    }

    /// Parses the text form. Values are decimal or `0x` hexadecimal.
    pub fn parse(text: &str) -> Result<Self, FixtureError> {
        let mut fixture = Self::new();
        for (index, line) in text.lines().enumerate() {
            let line_number = index + 1;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let fail = |expected| FixtureError {
                line: line_number,
                expected,
                received: trimmed.to_owned(),
            };
            let (key, value) = trimmed.split_once('=').ok_or_else(|| fail("`<key> = <number>`"))?;
            let (key, value) = (key.trim(), value.trim());
            if key.is_empty() {
                return Err(fail("a non-empty key before `=`"));
            }
            let number = match value.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16),
                None => value.parse(),
            }
            .map_err(|_| fail("a decimal or 0x-prefixed hexadecimal number after `=`"))?;
            fixture.entries.insert(key.to_owned(), number);
        }
        Ok(fixture)
    }

    /// The text form: comments, then one `key = 0xvalue` line per entry.
    pub fn to_text(&self) -> String {
        let mut text = String::new();
        for comment in &self.header {
            text.push_str("# ");
            text.push_str(comment);
            text.push('\n');
        }
        for (key, value) in &self.entries {
            text.push_str(&format!("{key} = {value:#x}\n"));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips_in_key_order() {
        let mut fixture = Fixture::new().with_comment("recorded on a test machine");
        fixture.set("msr.0.1.0x19c", 0x8834_0000);
        fixture.set("cfg.0x20", 0xC5);
        let text = fixture.to_text();
        assert_eq!(
            text,
            "# recorded on a test machine\ncfg.0x20 = 0xc5\nmsr.0.1.0x19c = 0x88340000\n"
        );
        let parsed = Fixture::parse(&text).unwrap();
        assert_eq!(parsed.get("cfg.0x20"), Some(0xC5));
        assert_eq!(parsed.get("msr.0.1.0x19c"), Some(0x8834_0000));
        assert_eq!(parsed.get("missing"), None);
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn decimal_values_comments_and_blank_lines_parse() {
        let parsed = Fixture::parse("\n# note\n  a = 17  \nb=0x11\n").unwrap();
        assert_eq!(parsed.get("a"), Some(17));
        assert_eq!(parsed.get("b"), Some(17));
        assert!(!parsed.is_empty());
    }

    #[test]
    fn a_line_without_equals_names_the_line_and_the_shape() {
        let error = Fixture::parse("a = 1\nbroken\n").unwrap_err();
        assert_eq!(
            error.to_string(),
            "(fixture line 2): expected `<key> = <number>` | received: \"broken\""
        );
    }

    #[test]
    fn a_bad_number_and_an_empty_key_are_rejected() {
        assert!(Fixture::parse("a = 0xZZ").is_err());
        assert!(Fixture::parse("a = -1").is_err());
        assert!(Fixture::parse("= 1").is_err());
    }

    #[test]
    fn a_later_value_replaces_an_earlier_one() {
        let mut fixture = Fixture::new();
        fixture.set("a", 1);
        fixture.set("a", 2);
        assert_eq!(fixture.entries().collect::<Vec<_>>(), [("a", 2)]);
    }
}
