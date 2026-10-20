//! Plain text output, rendered from the same views `--json` prints.

mod activity;
mod agent;
mod item;
mod status;
mod table;
mod text;

pub use activity::{event_line, event_line_for};
pub use agent::{agent_detail, agent_detail_for};
pub(crate) use item::state_icon;
pub use item::{
    flag_detail_for, item_choice, item_detail, item_detail_for, item_groups, item_icon, item_list,
    item_stats, search_hits,
};
pub use status::{session, session_choice, status};
pub use table::{agents, devices, inbox, item_stats_list, log, members, mentions, sessions};
pub(crate) use text::paint;
