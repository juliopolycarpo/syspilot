//! The form for adding a custom action, as data.
//!
//! The page binds controls to a [`CustomForm`]; turning it into a
//! [`CustomAction`] is checked here, without a window.

use syspilot_proto::action::{
    Action, CommandAction, CommandLine, RegData, RegistryAction, RunAs, Scope, ServiceAction,
    StartType,
};
use syspilot_proto::profile::{CUSTOM_ID_PREFIX, CustomAction};

/// Seconds a custom command may run before it is stopped.
const COMMAND_TIMEOUT_SECS: u32 = 60;

/// Choices of the "What to enforce" box.
pub const KINDS: [&str; 3] = ["Registry value", "Service start type", "Command"];
/// Choices of the registry "Where" box, in [`Scope`] order.
pub const SCOPES: [&str; 2] = ["This PC (HKEY_LOCAL_MACHINE)", "Every user (HKEY_CURRENT_USER)"];
/// Choices of the registry "Type" box.
pub const DATA_TYPES: [&str; 4] =
    ["Number (DWORD)", "Large number (QWORD)", "Text (SZ)", "The value must not exist"];
/// Choices of the service "Start type" box, in [`StartType`] order.
pub const START_TYPES: [&str; 4] = ["Automatic", "Automatic (delayed start)", "Manual", "Disabled"];
/// Choices of the command "Run as" box, in [`RunAs`] order.
pub const ACCOUNTS: [&str; 2] = ["System", "The signed-in user"];

/// A text field of the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// Name shown in the list.
    Title,
    /// Registry key path.
    Key,
    /// Registry value name.
    Name,
    /// Registry data.
    Data,
    /// Service key name.
    Service,
    /// Program path.
    Program,
    /// Program arguments.
    Arguments,
}

/// A choice box of the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Index into [`KINDS`].
    Kind,
    /// Index into [`SCOPES`].
    Scope,
    /// Index into [`DATA_TYPES`].
    DataType,
    /// Index into [`START_TYPES`].
    Start,
    /// Index into [`ACCOUNTS`].
    Account,
}

/// What the user has entered so far.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CustomForm {
    /// Name shown in the list.
    pub title: String,
    /// Index into [`KINDS`].
    pub kind: usize,
    /// Index into [`SCOPES`].
    pub scope: usize,
    /// Registry key path below the hive.
    pub key: String,
    /// Registry value name.
    pub name: String,
    /// Index into [`DATA_TYPES`].
    pub data_type: usize,
    /// Registry data as typed.
    pub data: String,
    /// Service key name.
    pub service: String,
    /// Index into [`START_TYPES`].
    pub start: usize,
    /// Program path.
    pub program: String,
    /// Program arguments as typed.
    pub arguments: String,
    /// Index into [`ACCOUNTS`].
    pub account: usize,
}

/// Splits a line into arguments at spaces; double quotes keep spaces.
pub fn split_arguments(text: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let (mut quoted, mut started) = (false, false);
    for character in text.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            ' ' | '\t' if !quoted => {
                if started {
                    arguments.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            _ => {
                current.push(character);
                started = true;
            }
        }
    }
    if started {
        arguments.push(current);
    }
    arguments
}

/// Turns a title into the word part of an ID: `Keep Num Lock on` becomes
/// `keep-num-lock-on`.
fn slug(title: &str) -> String {
    let mut slug = String::new();
    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').chars().take(48).collect::<String>().trim_end_matches('-').to_owned()
}

/// Parses `42` or `0x2A`.
fn parse_number(text: &str) -> Option<u64> {
    let text = text.trim();
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

impl CustomForm {
    /// Stores a text field.
    pub fn set_text(&mut self, field: Field, text: String) {
        *match field {
            Field::Title => &mut self.title,
            Field::Key => &mut self.key,
            Field::Name => &mut self.name,
            Field::Data => &mut self.data,
            Field::Service => &mut self.service,
            Field::Program => &mut self.program,
            Field::Arguments => &mut self.arguments,
        } = text;
    }

    /// Stores a choice.
    pub fn set_choice(&mut self, choice: Choice, index: usize) {
        *match choice {
            Choice::Kind => &mut self.kind,
            Choice::Scope => &mut self.scope,
            Choice::DataType => &mut self.data_type,
            Choice::Start => &mut self.start,
            Choice::Account => &mut self.account,
        } = index;
    }

    fn registry_data(&self) -> Result<RegData, String> {
        let number = || {
            parse_number(&self.data).ok_or_else(|| {
                format!(
                    "(data): expected a whole number such as 1 or 0x10 | received: {:?}",
                    self.data.trim()
                )
            })
        };
        match self.data_type {
            0 => u32::try_from(number()?).map(RegData::Dword).map_err(|_| {
                format!(
                    "(data): expected a number up to 4294967295 | received: {}",
                    self.data.trim()
                )
            }),
            1 => number().map(RegData::Qword),
            2 => Ok(RegData::Sz(self.data.clone())),
            _ => Ok(RegData::Absent),
        }
    }

    fn action(&self) -> Result<Action, String> {
        match self.kind {
            0 => Ok(Action::Registry(RegistryAction {
                scope: if self.scope == 0 { Scope::Machine } else { Scope::User },
                key: self.key.trim().trim_matches('\\').to_owned(),
                name: self.name.trim().to_owned(),
                data: self.registry_data()?,
            })),
            1 => Ok(Action::Service(ServiceAction {
                service: self.service.trim().to_owned(),
                start: match self.start {
                    0 => StartType::Automatic,
                    1 => StartType::AutomaticDelayed,
                    2 => StartType::Manual,
                    _ => StartType::Disabled,
                },
            })),
            _ => Ok(Action::Command(CommandAction {
                apply: CommandLine {
                    program: self.program.trim().trim_matches('"').to_owned(),
                    args: split_arguments(&self.arguments),
                },
                run_as: if self.account == 0 { RunAs::System } else { RunAs::User },
                timeout_secs: COMMAND_TIMEOUT_SECS,
                check: None,
                undo: None,
            })),
        }
    }

    /// Builds the action, or says what is wrong with the form. `is_taken`
    /// tells whether an ID is already in use.
    pub fn build(&self, is_taken: impl Fn(&str) -> bool) -> Result<CustomAction, String> {
        let title = self.title.trim();
        let word = slug(title);
        if word.is_empty() {
            return Err(format!(
                "(name): expected a name with at least one letter or digit | received: {title:?}"
            ));
        }
        let action = self.action()?;
        action.validate().map_err(|invalid| invalid.to_string())?;
        let base = format!("{CUSTOM_ID_PREFIX}{word}");
        let id = std::iter::once(base.clone())
            .chain((2..).map(|number| format!("{base}-{number}")))
            .find(|candidate| !is_taken(candidate))
            .unwrap_or(base);
        Ok(CustomAction {
            id,
            title: title.to_owned(),
            enabled: true,
            action,
            triggers: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_form() -> CustomForm {
        CustomForm {
            title: "Keep Num Lock on!".into(),
            scope: 1,
            key: r"\Control Panel\Keyboard\".into(),
            name: "InitialKeyboardIndicators".into(),
            data_type: 2,
            data: "2".into(),
            ..CustomForm::default()
        }
    }

    #[test]
    fn registry_form_becomes_a_per_user_registry_action() {
        let custom = registry_form().build(|_| false).unwrap();
        assert_eq!(custom.id, "custom.keep-num-lock-on");
        assert_eq!(custom.title, "Keep Num Lock on!");
        assert!(custom.enabled);
        assert_eq!(
            custom.action,
            Action::Registry(RegistryAction {
                scope: Scope::User,
                key: r"Control Panel\Keyboard".into(),
                name: "InitialKeyboardIndicators".into(),
                data: RegData::Sz("2".into()),
            })
        );
    }

    #[test]
    fn taken_id_gets_a_number() {
        let taken = ["custom.keep-num-lock-on", "custom.keep-num-lock-on-2"];
        let custom = registry_form().build(|id| taken.contains(&id)).unwrap();
        assert_eq!(custom.id, "custom.keep-num-lock-on-3");
    }

    #[test]
    fn numbers_accept_decimal_and_hex_and_reject_the_rest() {
        let mut form = registry_form();
        form.data_type = 0;
        form.data = " 0x10 ".into();
        let Action::Registry(registry) = form.build(|_| false).unwrap().action else {
            panic!("(form): expected a registry action");
        };
        assert_eq!(registry.data, RegData::Dword(16));

        form.data = "ten".into();
        let refusal = form.build(|_| false).unwrap_err();
        assert_eq!(
            refusal,
            "(data): expected a whole number such as 1 or 0x10 | received: \"ten\""
        );
        form.data = "4294967296".into();
        assert!(form.build(|_| false).unwrap_err().contains("up to 4294967295"));
        form.data_type = 1;
        assert!(form.build(|_| false).is_ok(), "a QWORD holds it");
    }

    #[test]
    fn name_without_letters_is_refused() {
        let mut form = registry_form();
        form.title = " !!! ".into();
        let refusal = form.build(|_| false).unwrap_err();
        assert!(refusal.starts_with("(name): expected a name"), "{refusal}");
    }

    #[test]
    fn proto_validation_errors_reach_the_user() {
        let mut form = registry_form();
        form.key = String::new();
        let refusal = form.build(|_| false).unwrap_err();
        assert!(refusal.contains("expected"), "{refusal}");
    }

    #[test]
    fn service_and_command_forms_build_their_actions() {
        let service = CustomForm {
            title: "No print spooler".into(),
            kind: 1,
            service: " Spooler ".into(),
            start: 3,
            ..CustomForm::default()
        };
        assert_eq!(
            service.build(|_| false).unwrap().action,
            Action::Service(ServiceAction {
                service: "Spooler".into(),
                start: StartType::Disabled
            })
        );

        let command = CustomForm {
            title: "Flush DNS".into(),
            kind: 2,
            program: r#""C:\Windows\System32\ipconfig.exe""#.into(),
            arguments: "/flushdns".into(),
            account: 1,
            ..CustomForm::default()
        };
        let Action::Command(action) = command.build(|_| false).unwrap().action else {
            panic!("(form): expected a command action");
        };
        assert_eq!(action.apply.program, r"C:\Windows\System32\ipconfig.exe");
        assert_eq!(action.apply.args, ["/flushdns"]);
        assert_eq!(action.run_as, RunAs::User);
    }

    #[test]
    fn arguments_split_at_spaces_outside_quotes() {
        assert_eq!(split_arguments(r#"/a  "two words" c"d e""#), ["/a", "two words", "cd e"]);
        assert_eq!(split_arguments(r#""" x"#), ["", "x"]);
        assert!(split_arguments("   ").is_empty());
    }

    #[test]
    fn fields_and_choices_are_stored_by_name() {
        let mut form = CustomForm::default();
        form.set_text(Field::Program, "p".into());
        form.set_text(Field::Title, "t".into());
        form.set_choice(Choice::Kind, 2);
        form.set_choice(Choice::Account, 1);
        assert_eq!((form.program.as_str(), form.title.as_str()), ("p", "t"));
        assert_eq!((form.kind, form.account), (2, 1));
    }

    #[test]
    fn slugs_are_short_lowercase_words() {
        assert_eq!(slug("  Hello,  World! "), "hello-world");
        assert_eq!(slug("Ünïcode only: é"), "n-code-only");
        assert!(slug(&"long word ".repeat(20)).len() <= 48);
    }
}
