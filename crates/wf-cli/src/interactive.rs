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

/// Print `text` through `$PAGER` (`less -R` by default) if it is taller than the terminal,
/// otherwise print it directly. Also prints directly if the pager cannot be started.
pub fn page(text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let fits = terminal_size::terminal_size()
        .is_none_or(|(_, height)| text.lines().count() < usize::from(height.0));
    if fits {
        anstream::print!("{text}");
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
        anstream::print!("{text}");
        return Ok(());
    };
    if let Some(mut stdin) = child.stdin.take() {
        // The reader quitting early closes the pipe; that is not an error.
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait()?;
    Ok(())
}

/// An agent that `wf init -i` defines.
pub struct AgentSetup {
    pub name: wf_core::name::AgentName,
    pub harness: wf_core::name::HarnessName,
    pub permissions: wf_core::autonomy::PermissionMode,
    /// `None` keeps the harness's own default.
    pub model: Option<String>,
}

/// What `wf init -i` asks for before it writes anything.
pub struct InitAnswers {
    pub name: String,
    pub agents: Vec<AgentSetup>,
}

/// Ask for the name of the workforce, which of the found harnesses to define agents for, and the
/// name, permissions and model of each.
pub fn init_dialog(name: Option<String>, harnesses: &[&str]) -> anyhow::Result<InitAnswers> {
    use wf_core::autonomy::PermissionMode;
    use wf_core::name::{AgentName, HarnessName};

    cliclack::intro("wf init")?;
    let name: String = match name {
        Some(name) => name,
        None => cliclack::input("Name of the workforce")
            .validate(|input: &String| {
                if input.trim().is_empty() {
                    Err("give it a name")
                } else {
                    Ok(())
                }
            })
            .interact()?,
    };

    let picked: Vec<&str> = if harnesses.is_empty() {
        cliclack::log::warning(
            "no agent harness found on PATH; add agents later with `wf agent add`",
        )?;
        Vec::new()
    } else {
        let items: Vec<(&str, &str, &str)> = harnesses
            .iter()
            .map(|harness| (*harness, *harness, ""))
            .collect();
        cliclack::multiselect("Which harnesses should wf use? (space to pick)")
            .items(&items)
            .required(false)
            .interact()?
    };

    let mut agents: Vec<AgentSetup> = Vec::new();
    for harness in picked {
        let suggested = if agents.is_empty() {
            "coder".to_string()
        } else {
            format!("coder-{harness}")
        };
        let taken: Vec<String> = agents.iter().map(|agent| agent.name.to_string()).collect();
        let agent_name: AgentName = cliclack::input(format!("Name of the {harness} agent"))
            .default_input(&suggested)
            .validate(move |input: &String| match AgentName::new(input) {
                Err(error) => Err(error.to_string()),
                Ok(_) if taken.contains(input) => Err(format!("`{input}` is already taken")),
                Ok(_) => Ok(()),
            })
            .interact()?;
        let permissions = cliclack::select(format!("What may {agent_name} do?"))
            .item(
                PermissionMode::Allow,
                "allow",
                "everything it asks, inside the sandbox; asks on a machine without one",
            )
            .item(PermissionMode::Ask, "ask", "hold each request for you")
            .item(PermissionMode::Deny, "deny", "refuse every request")
            .interact()?;
        let model = ask_model(harness, &agent_name)?;
        agents.push(AgentSetup {
            name: agent_name,
            harness: HarnessName::new(harness)?,
            permissions,
            model,
        });
    }
    cliclack::outro("creating the workforce")?;
    Ok(InitAnswers { name, agents })
}

/// Pick one of the models the harness offers, or type one when it offers none or cannot be
/// asked. `None` keeps the harness's default.
fn ask_model(harness: &str, agent: &wf_core::name::AgentName) -> anyhow::Result<Option<String>> {
    let spinner = cliclack::spinner();
    spinner.start(format!("asking {harness} for its models"));
    let models = match crate::harness::models_of(harness) {
        Ok(models) if models.is_empty() => {
            spinner.error(format!("{harness} offers no models to choose from"));
            Vec::new()
        }
        Ok(models) => {
            spinner.stop(format!("{harness} offers {} models", models.len()));
            models
        }
        Err(error) => {
            spinner.error(format!("could not ask {harness} for its models: {error:#}"));
            Vec::new()
        }
    };

    if models.is_empty() {
        let model: String = cliclack::input(format!("Model for {agent}"))
            .placeholder("the harness's default")
            .required(false)
            .interact()?;
        let model = model.trim();
        return Ok((!model.is_empty()).then(|| model.to_string()));
    }
    let default = models
        .iter()
        .find(|model| model.is_default)
        .map_or_else(String::new, |model| format!(", now {}", model.name));
    let mut select = cliclack::select(format!("Model for {agent}"))
        .item(
            None,
            "the harness's default",
            format!("follows the harness{default}"),
        )
        .max_rows(12);
    if models.len() > 8 {
        select = select.filter_mode();
    }
    for model in models {
        let label = format!("{} ({})", model.value, model.name);
        select = select.item(Some(model.value), label, "");
    }
    Ok(select.interact()?)
}
