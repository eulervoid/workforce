//! The state of the dashboard and what keys do to it. Pure: what a key asks of the workforce comes
//! back as an [`Effect`], which the loop carries out.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::snapshot::Snapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Now,
    Items,
    Agents,
    Log,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Now, Tab::Items, Tab::Agents, Tab::Log];

    pub fn name(self) -> &'static str {
        match self {
            Tab::Now => "Now",
            Tab::Items => "Items",
            Tab::Agents => "Agents",
            Tab::Log => "Log",
        }
    }

    fn index(self) -> usize {
        match self {
            Tab::Now => 0,
            Tab::Items => 1,
            Tab::Agents => 2,
            Tab::Log => 3,
        }
    }

    fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    fn previous(self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

/// What a key asks of the workforce or the terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Quit,
}

pub struct App {
    pub snapshot: Snapshot,
    pub tab: Tab,
    /// Why the last refresh or action failed, until the next one works.
    pub error: Option<String>,
}

impl App {
    pub fn new(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            tab: Tab::Now,
            error: None,
        }
    }

    pub fn refresh(&mut self, snapshot: Snapshot) {
        self.snapshot = snapshot;
        self.error = None;
    }

    pub fn failed(&mut self, error: &anyhow::Error) {
        self.error = Some(format!("{error:#}"));
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Effect::Quit);
        }
        match key.code {
            KeyCode::Char('q') => return Some(Effect::Quit),
            KeyCode::Char('1') => self.tab = Tab::Now,
            KeyCode::Char('2') => self.tab = Tab::Items,
            KeyCode::Char('3') => self.tab = Tab::Agents,
            KeyCode::Char('4') => self.tab = Tab::Log,
            KeyCode::Tab => self.tab = self.tab.next(),
            KeyCode::BackTab => self.tab = self.tab.previous(),
            _ => {}
        }
        None
    }
}
