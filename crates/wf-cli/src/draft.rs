//! The TOML draft that `wf item edit` opens in `$EDITOR`, and how it turns into events.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use wf_core::crdt::SetChange;
use wf_core::event::Change;
use wf_core::item::{Item, Priority};
use wf_core::name::{Label, LaneName, Requirement};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub priority: Option<Priority>,
    #[serde(default)]
    pub lane: Option<LaneName>,
    #[serde(default)]
    pub labels: BTreeSet<Label>,
    #[serde(default)]
    pub requires: BTreeSet<Requirement>,
}

impl Draft {
    pub fn of(item: &Item) -> Self {
        Self {
            title: item.title().to_string(),
            body: item.body().text().to_string(),
            priority: item.priority(),
            lane: item.lane().cloned(),
            labels: item.labels().values().cloned().collect(),
            requires: item.requires().values().cloned().collect(),
        }
    }

    /// TOML has no null, so unset fields appear as commented-out hints.
    pub fn to_toml(&self) -> anyhow::Result<String> {
        let mut text = toml::to_string(self)?;
        let mut hints = Vec::new();
        if self.priority.is_none() {
            hints.push("# priority = \"P2\"  # P0, P1, P2 or P3");
        }
        if self.lane.is_none() {
            hints.push("# lane = \"default\"");
        }
        if !hints.is_empty() {
            text.push('\n');
            text.push_str(&hints.join("\n"));
            text.push('\n');
        }
        Ok(text)
    }

    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(text)?)
    }

    /// The edits that turn `item` into this draft. Removals name the adds the item currently has.
    pub fn changes(&self, item: &Item) -> Vec<Change> {
        let current = Draft::of(item);
        let mut changes = Vec::new();
        if self.title != current.title {
            changes.push(Change::Title(self.title.clone()));
        }
        if self.body != current.body {
            changes.push(Change::Body(self.body.clone()));
        }
        if self.priority != current.priority {
            changes.push(Change::Priority(self.priority));
        }
        if self.lane != current.lane {
            changes.push(Change::Lane(self.lane.clone()));
        }
        for label in self.labels.difference(&current.labels) {
            changes.push(Change::Labels(SetChange::Add(label.clone())));
        }
        for label in current.labels.difference(&self.labels) {
            changes.push(Change::Labels(item.labels().removal(label)));
        }
        for requirement in self.requires.difference(&current.requires) {
            changes.push(Change::Requires(SetChange::Add(requirement.clone())));
        }
        for requirement in current.requires.difference(&self.requires) {
            changes.push(Change::Requires(item.requires().removal(requirement)));
        }
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wf_core::item::ItemView;
    use wf_core::testing::*;

    #[test]
    fn unchanged_drafts_make_no_changes_and_edits_become_changes() {
        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let created = fixture.apply(
            &mut state,
            0,
            2_000,
            create_with_body("task", "Old", "text"),
        );
        let id = created.id().into();
        let ItemView::Live(item) = state.item(id).unwrap() else {
            panic!()
        };
        let draft = Draft::of(item);
        assert!(draft.changes(item).is_empty());

        let text = draft.to_toml().unwrap();
        assert!(text.contains("# priority = ") && text.contains("# lane = "));
        let mut edited = Draft::from_toml(&text).unwrap();
        assert_eq!(edited, draft);
        edited.title = "New".into();
        edited.priority = Some(Priority::P1);
        edited.labels.insert(Label::new("urgent").unwrap());
        let changes = edited.changes(item);
        assert_eq!(changes.len(), 3);
        assert!(matches!(&changes[0], Change::Title(title) if title == "New"));
        let set = edited.to_toml().unwrap();
        assert!(set.contains("priority = \"P1\"") && !set.contains("# priority"));
        assert_eq!(Draft::from_toml(&set).unwrap(), edited);
    }

    #[test]
    fn invalid_drafts_are_rejected() {
        assert!(Draft::from_toml("title = \"x\"\nbody = \"\"\npriority = \"P9\"\n").is_err());
        assert!(
            Draft::from_toml("title = \"x\"\nbody = \"\"\nlabels = [\"Bad Label\"]\n").is_err()
        );
        assert!(Draft::from_toml("title = \"x\"\nsurprise = 1\n").is_err());
    }
}
