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
