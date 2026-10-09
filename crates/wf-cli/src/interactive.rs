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

/// Fuzzy-pick one of `choices` inline, below the prompt, and return its value.
pub fn pick(what: &str, choices: Vec<Choice>) -> anyhow::Result<String> {
    use dialoguer::FuzzySelect;

    if choices.is_empty() {
        anyhow::bail!("there is no {what} to pick from");
    }
    let picked = FuzzySelect::new()
        .with_prompt(format!("Pick {what}"))
        .items(&choices)
        .max_length(12)
        .interact_opt()?;
    match picked {
        Some(index) => Ok(choices[index].value.clone()),
        None => anyhow::bail!("no {what} picked"),
    }
}

/// Print `text` through `$PAGER` (`less -R` by default) if it is taller than the terminal,
/// otherwise print it directly. Also prints directly if the pager cannot be started.
pub fn page(text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let fits = terminal_size::terminal_size()
        .is_none_or(|(_, height)| text.lines().count() < usize::from(height.0));
    if fits {
        print!("{text}");
        return Ok(());
    }
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less -R".to_string());
    let mut words = pager.split_whitespace();
    let child = words.next().and_then(|program| {
        Command::new(program)
            .args(words)
            .stdin(Stdio::piped())
            .spawn()
            .ok()
    });
    let Some(mut child) = child else {
        print!("{text}");
        return Ok(());
    };
    if let Some(mut stdin) = child.stdin.take() {
        // The reader quitting early closes the pipe; that is not an error.
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait()?;
    Ok(())
}
