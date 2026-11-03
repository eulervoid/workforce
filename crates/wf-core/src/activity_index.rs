//! Which events of the log can show in the activity of an item, found without reading them all.
//! A reader that keeps the index leaves the events out that cannot concern the item, and reads
//! the rest: [`State::can_be_activity_of`] still decides about those. Pure: the caller feeds it
//! the events in log order.

use std::collections::BTreeSet;

use crate::crdt::SetChange;
use crate::event::{Change, Event, EventBody, Link};
use crate::id::{HashId, ItemId, MIN_PREFIX_LEN};
use crate::item::ItemEntry;
use crate::state::State;

/// What of one event decides whether it can show in the activity of an item.
struct Entry {
    /// The items the event is about, links to or moves.
    items: Vec<ItemId>,
    /// What the event says, where it can mention an item by the start of its id.
    text: Option<String>,
}

/// For each event of the log, in order, the items it concerns and the text it can mention one in.
#[derive(Default)]
pub struct ActivityIndex {
    entries: Vec<Entry>,
}

impl ActivityIndex {
    /// How many events the index has taken in: the log position of the next one.
    pub fn len(&self) -> u64 {
        self.entries.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Takes in the event after those taken in so far, as `state`, which has folded it, knows it.
    pub fn push(&mut self, state: &State, event: &Event) {
        let mut items: Vec<ItemId> = state.item_of(event).into_iter().collect();
        let text = match &event.content.body {
            EventBody::ItemCreated(created) => Some(created.body.clone()),
            EventBody::Commented(comment) => Some(comment.text.clone()),
            EventBody::ItemEdited(edited) => {
                let bodies: Vec<&str> = edited
                    .changes
                    .iter()
                    .filter_map(|change| match change {
                        Change::Body(body) => Some(body.as_str()),
                        Change::Title(_)
                        | Change::Priority(_)
                        | Change::Lane(_)
                        | Change::Labels(_)
                        | Change::Requires(_)
                        | Change::ReviewLevel(_)
                        | Change::StepDone { .. }
                        | Change::RecoveryLevel(_) => None,
                    })
                    .collect();
                (!bodies.is_empty()).then(|| bodies.join("\n"))
            }
            EventBody::ItemLinked(linked) => {
                match &linked.link {
                    Link::DependsOn(SetChange::Add(target)) | Link::Supersedes(target) => {
                        items.push(*target);
                    }
                    Link::DependsOn(SetChange::Remove { value, .. }) => items.push(*value),
                }
                None
            }
            EventBody::ItemMoved(_)
            | EventBody::ItemArchived(_)
            | EventBody::ItemUnarchived(_)
            | EventBody::SessionStarted(_)
            | EventBody::SessionEnded(_)
            | EventBody::Reviewed(_)
            | EventBody::Merged(_)
            | EventBody::MergeClaimed(_)
            | EventBody::Genesis(_)
            | EventBody::TargetMoved(_)
            | EventBody::PolicySet(_)
            | EventBody::AgentDefined(_)
            | EventBody::AgentRemoved(_)
            | EventBody::InviteCreated(_)
            | EventBody::DeviceBound(_)
            | EventBody::DeviceProfile(_)
            | EventBody::ForkAnnounced(_)
            | EventBody::Unknown { .. } => None,
        };
        self.entries.push(Entry { items, text });
    }

    /// The log positions of the events that can show in the activity of any of `items`, as
    /// [`State::can_be_activity_of`] has it for `state`, and possibly some that cannot: those
    /// that concern the item or one it depends on, and those whose text has the start of its id.
    pub fn candidates(&self, state: &State, items: &[ItemId]) -> Vec<u64> {
        let mut concerned: BTreeSet<ItemId> = items.iter().copied().collect();
        for item in items {
            if let Some(ItemEntry::Live(live)) = state.item(*item) {
                concerned.extend(live.depends_on().values().copied());
            }
        }
        let starts: Vec<String> = items
            .iter()
            .map(|item| item.to_base32()[..MIN_PREFIX_LEN].to_owned())
            .collect();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.items.iter().any(|item| concerned.contains(item))
                    || entry
                        .text
                        .as_deref()
                        .is_some_and(|text| starts.iter().any(|start| text.contains(start)))
            })
            .map(|(position, _)| position as u64)
            .collect()
    }
}
