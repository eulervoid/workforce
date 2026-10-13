//! The markdown draft that `wf item edit` opens in `$EDITOR`, and how it turns into events.
//!
//! A draft is TOML front matter between `+++` fences followed by the body as plain markdown.

use std::collections::BTreeSet;

use anyhow::{Context, bail};

use serde::{Deserialize, Serialize};
use wf_core::crdt::SetChange;
use wf_core::event::Change;
use wf_core::item::{Item, Priority};
use wf_core::name::{Label, LaneName, RecoveryLevelName, Requirement, ReviewLevelName, TypeName};

const FENCE: &str = "+++";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub title: String,
    pub body: String,
    pub priority: Option<Priority>,
    pub lane: Option<LaneName>,
    pub labels: BTreeSet<Label>,
    pub requires: BTreeSet<Requirement>,
    /// The item's own review level, not the one it gets from its type.
    pub review: Option<ReviewLevelName>,
    /// The item's own recovery level, not the one it gets from its type.
    pub recovery: Option<RecoveryLevelName>,
}

/// Everything in a draft except the body, which is the markdown after the front matter.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontMatter {
    title: String,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    item_type: Option<TypeName>,
    #[serde(default)]
    priority: Option<Priority>,
    #[serde(default)]
    lane: Option<LaneName>,
    #[serde(default)]
    labels: BTreeSet<Label>,
    #[serde(default)]
    requires: BTreeSet<Requirement>,
    #[serde(default)]
    review: Option<ReviewLevelName>,
    #[serde(default)]
    recovery: Option<RecoveryLevelName>,
}

impl Draft {
    pub fn of(item: &Item) -> Self {
        Self {
            title: item.title().to_string(),
            body: item.body().render(),
            priority: item.priority(),
            lane: item.lane().cloned(),
            labels: item.labels().values().cloned().collect(),
            requires: item.requires().values().cloned().collect(),
            review: item.review_level().cloned(),
            recovery: item.recovery_level().cloned(),
        }
    }

    pub fn to_markdown(&self) -> anyhow::Result<String> {
        self.render(None)
    }

    pub fn from_markdown(text: &str) -> anyhow::Result<Self> {
        let (item_type, draft) = Self::parse(text)?;
        if item_type.is_some() {
            bail!("the type of an existing item cannot be changed");
        }
        Ok(draft)
    }

    /// TOML has no null, so unset fields appear as commented-out hints.
    fn render(&self, item_type: Option<&TypeName>) -> anyhow::Result<String> {
        let mut front_matter = toml::to_string(&FrontMatter {
            item_type: item_type.cloned(),
            title: self.title.clone(),
            priority: self.priority,
            lane: self.lane.clone(),
            labels: self.labels.clone(),
            requires: self.requires.clone(),
            review: self.review.clone(),
            recovery: self.recovery.clone(),
        })?;
        if self.priority.is_none() {
            front_matter.push_str("# priority = \"P2\"  # P0, P1, P2 or P3\n");
        }
        if self.lane.is_none() {
            front_matter.push_str("# lane = \"default\"\n");
        }
        if self.review.is_none() {
            front_matter.push_str("# review = \"standard\"\n");
        }
        if self.recovery.is_none() {
            front_matter.push_str("# recovery = \"standard\"\n");
        }
        Ok(format!(
            "{FENCE}\n{front_matter}{FENCE}\n\n{}\n",
            self.body.trim_end()
        ))
    }

    fn parse(text: &str) -> anyhow::Result<(Option<TypeName>, Self)> {
        let mut lines = text.split_inclusive('\n');
        if lines.next().map(str::trim_end) != Some(FENCE) {
            bail!("the draft must start with a `{FENCE}` line before the TOML front matter");
        }
        let mut front_matter = String::new();
        let mut closed = false;
        let mut body = String::new();
        for line in lines {
            if closed {
                body.push_str(line);
            } else if line.trim_end() == FENCE {
                closed = true;
            } else {
                front_matter.push_str(line);
            }
        }
        if !closed {
            bail!("the front matter is not closed by a `{FENCE}` line");
        }
        let front_matter: FrontMatter =
            toml::from_str(&front_matter).context("invalid front matter")?;
        let draft = Self {
            title: front_matter.title,
            body: body.trim().to_string(),
            priority: front_matter.priority,
            lane: front_matter.lane,
            labels: front_matter.labels,
            requires: front_matter.requires,
            review: front_matter.review,
            recovery: front_matter.recovery,
        };
        Ok((front_matter.item_type, draft))
    }

    /// The edits that turn `item` into this draft. Removals name the adds the item currently has.
    pub fn changes(&self, item: &Item) -> Vec<Change> {
        let current = Draft::of(item);
        let mut changes = Vec::new();
        if self.title != current.title {
            changes.push(Change::Title(self.title.clone()));
        }
        if self.body.trim_end() != current.body.trim_end() {
            changes.push(Change::Body(self.body.clone()));
        }
        if self.priority != current.priority {
            changes.push(Change::Priority(self.priority));
        }
        if self.lane != current.lane {
            changes.push(Change::Lane(self.lane.clone()));
        }
        if self.review != current.review {
            changes.push(Change::ReviewLevel(self.review.clone()));
        }
        if self.recovery != current.recovery {
            changes.push(Change::RecoveryLevel(self.recovery.clone()));
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

/// A draft of an item that does not exist yet, so it also says what type to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewItemDraft {
    pub item_type: TypeName,
    pub draft: Draft,
}

impl NewItemDraft {
    pub fn to_markdown(&self) -> anyhow::Result<String> {
        self.draft.render(Some(&self.item_type))
    }

    pub fn from_markdown(text: &str) -> anyhow::Result<Self> {
        match Draft::parse(text)? {
            (Some(item_type), draft) => Ok(Self { item_type, draft }),
            (None, _) => bail!("the front matter needs a `type`, for example `type = \"task\"`"),
        }
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

        let text = draft.to_markdown().unwrap();
        assert!(text.starts_with("+++\ntitle = \"Old\"\n"));
        assert!(text.contains("\npriority = \"P2\"\n") && text.contains("# lane = "));
        let unset = Draft {
            priority: None,
            ..draft.clone()
        };
        assert!(unset.to_markdown().unwrap().contains("# priority = "));
        assert!(text.ends_with("+++\n\ntext\n"));
        let mut edited = Draft::from_markdown(&text).unwrap();
        assert_eq!(edited, draft);
        edited.title = "New".into();
        edited.priority = Some(Priority::P1);
        edited.labels.insert(Label::new("urgent").unwrap());
        let changes = edited.changes(item);
        assert_eq!(changes.len(), 3);
        assert!(matches!(&changes[0], Change::Title(title) if title == "New"));
        let set = edited.to_markdown().unwrap();
        assert!(set.contains("priority = \"P1\"") && !set.contains("# priority"));
        assert_eq!(Draft::from_markdown(&set).unwrap(), edited);
    }

    #[test]
    fn the_body_is_taken_verbatim_after_the_front_matter() {
        let draft = Draft::from_markdown(
            "+++\ntitle = \"x\"\n+++\n\n# Heading\n\n+++\nnot a fence\n\n- item\n",
        )
        .unwrap();
        assert_eq!(draft.body, "# Heading\n\n+++\nnot a fence\n\n- item");
        assert_eq!(
            Draft::from_markdown(&draft.to_markdown().unwrap()).unwrap(),
            draft
        );
    }

    #[test]
    fn invalid_drafts_are_rejected() {
        let draft = |front_matter: &str| format!("+++\n{front_matter}+++\n\nbody\n");
        assert!(Draft::from_markdown(&draft("title = \"x\"\npriority = \"P9\"\n")).is_err());
        assert!(Draft::from_markdown(&draft("title = \"x\"\nlabels = [\"Bad Label\"]\n")).is_err());
        assert!(Draft::from_markdown(&draft("title = \"x\"\nsurprise = 1\n")).is_err());
        assert!(Draft::from_markdown(&draft("")).is_err());
        assert!(Draft::from_markdown("title = \"x\"\n").is_err());
        assert!(Draft::from_markdown("+++\ntitle = \"x\"\n").is_err());
    }

    #[test]
    fn new_item_drafts_carry_the_type() {
        let new = NewItemDraft {
            item_type: TypeName::new("decision").unwrap(),
            draft: Draft {
                title: String::new(),
                body: String::new(),
                priority: None,
                lane: None,
                labels: BTreeSet::new(),
                requires: BTreeSet::new(),
                review: None,
                recovery: None,
            },
        };
        let text = new.to_markdown().unwrap();
        assert!(text.starts_with("+++\ntitle = \"\"\ntype = \"decision\"\n"));
        assert_eq!(NewItemDraft::from_markdown(&text).unwrap(), new);
        assert!(Draft::from_markdown(&text).is_err());
        assert!(NewItemDraft::from_markdown("+++\ntitle = \"x\"\n+++\n").is_err());
    }
}
