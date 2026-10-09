//! Interaction that only happens at a terminal. Scripts and agents never see it: without a
//! terminal, commands fail on missing arguments instead of asking.

use std::io::IsTerminal;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A person is at the terminal, so missing arguments can be asked for.
    Interactive,
    Scripted,
}

impl Mode {
    pub fn detect(json: bool) -> Self {
        let at_terminal = std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal();
        if at_terminal && !json {
            Self::Interactive
        } else {
            Self::Scripted
        }
    }
}

/// One line in a picker, standing for `value`.
pub struct Choice {
    pub value: String,
    pub label: String,
}

impl std::fmt::Display for Choice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.label)
    }
}

/// Fuzzy-pick one of `choices` and return its value.
pub fn pick(what: &str, choices: Vec<Choice>) -> anyhow::Result<String> {
    use nucleo_picker::error::PickError;
    use nucleo_picker::{Picker, render::DisplayRenderer};

    if choices.is_empty() {
        anyhow::bail!("there is no {what} to pick from");
    }
    let mut picker = Picker::new(DisplayRenderer);
    picker.push_batch(choices);
    match picker.pick() {
        Ok(Some(choice)) => Ok(choice.value.clone()),
        Ok(None) | Err(PickError::UserInterrupted) => anyhow::bail!("no {what} picked"),
        Err(error) => Err(error.into()),
    }
}
