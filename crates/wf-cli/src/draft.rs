//! The YAML draft that `wf item edit` opens in `$EDITOR`, and how it turns into events.

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
    pub priority: Option<Priority>,
    pub lane: Option<LaneName>,
    pub labels: BTreeSet<Label>,
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

    pub fn to_yaml(&self) -> anyhow::Result<String> {
        Ok(serde_yaml_ng::to_string(self)?)
    }

    pub fn from_yaml(text: &str) -> anyhow::Result<Self> {
        Ok(serde_yaml_ng::from_str(text)?)
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

        let yaml = draft.to_yaml().unwrap();
        let mut edited = Draft::from_yaml(&yaml).unwrap();
        assert_eq!(edited, draft);
        edited.title = "New".into();
        edited.priority = Some(Priority::P1);
        edited.labels.insert(Label::new("urgent").unwrap());
        let changes = edited.changes(item);
        assert_eq!(changes.len(), 3);
        assert!(matches!(&changes[0], Change::Title(title) if title == "New"));
    }

    #[test]
    fn invalid_drafts_are_rejected() {
        assert!(
            Draft::from_yaml(
                "title: x\nbody: ''\npriority: P9\nlane: null\nlabels: []\nrequires: []\n"
            )
            .is_err()
        );
        assert!(Draft::from_yaml("title: x\nbody: ''\npriority: null\nlane: null\nlabels: [Bad Label]\nrequires: []\n").is_err());
        assert!(Draft::from_yaml("title: x\nsurprise: 1\n").is_err());
    }
}
