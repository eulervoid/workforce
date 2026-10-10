use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand};
use wf_core::item::Priority;
use wf_core::name::{AgentName, HarnessName, Label, LaneName, Requirement, StateName, TypeName};

#[derive(Parser)]
#[command(
    name = "wf",
    version,
    about = "Coordination for coding agents and the humans who work with them"
)]
pub struct Cli {
    /// Print machine-readable output with a versioned schema.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a workforce for the current directory.
    Init(InitArgs),
    /// Manage agent definitions.
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Create and manage items: tasks, questions, decisions.
    #[command(subcommand)]
    Item(ItemCommand),
    /// Comment on an item. Without text, opens the editor when run at a terminal.
    Comment(CommentArgs),
    /// Show where a principal or agent is mentioned.
    Mentions {
        /// `@joshua` or `@joshua/coder`
        target: String,
    },
    /// Show or change the policy.
    #[command(subcommand)]
    Policy(PolicyCommand),
    /// Print every event of the log, verified.
    Log,
    /// Exchange events with the other devices of the workforce.
    Sync(SyncArgs),
    /// Invite another machine of yours to the workforce.
    #[command(subcommand)]
    Invite(InviteCommand),
    /// Bind this machine to a workforce with an invite code.
    Join(JoinArgs),
    /// List and describe the machines bound to the workforce.
    #[command(subcommand)]
    Device(DeviceCommand),
    /// List the members of the workforce, with their roles and machines.
    Members,
    /// Summarize the workforce.
    Status {
        /// Ignore the snapshot and fold the whole log again.
        #[arg(long)]
        refold: bool,
    },
}

#[derive(Args)]
pub struct SyncArgs {
    /// Keep listening for other devices until interrupted.
    #[arg(long)]
    pub serve: bool,
    /// Use this relay server instead of the default ones.
    #[arg(long, value_name = "URL")]
    pub relay: Option<String>,
}

#[derive(Subcommand)]
pub enum InviteCommand {
    /// Print a code that binds one more machine to you. Needs your principal key.
    Device(InviteDeviceArgs),
}

#[derive(Args)]
pub struct InviteDeviceArgs {
    /// How long the code works, such as `24h` or `30m`.
    #[arg(long, default_value = "24h")]
    pub expires: humantime::Duration,
}

#[derive(Subcommand)]
pub enum DeviceCommand {
    List,
    /// Change what this machine says about itself.
    Edit(DeviceEditArgs),
}

/// Fields that are given replace the current value.
#[derive(Args)]
pub struct DeviceEditArgs {
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub hardware: Option<String>,
    #[arg(long)]
    pub max_sessions: Option<u32>,
    /// Replaces the agents of this machine: `coder=2`. Repeat for several.
    #[arg(long = "agent", value_name = "NAME=SESSIONS")]
    pub agents: Vec<String>,
}

#[derive(Args)]
pub struct JoinArgs {
    /// The `wfi1...` code printed by `wf invite device`.
    pub code: String,
    /// A name for this machine. Asked for at a terminal when left out.
    #[arg(long)]
    pub name: Option<String>,
    /// How many agent sessions this machine may run at once.
    #[arg(long, default_value_t = 1)]
    pub max_sessions: u32,
    /// An agent this machine can run and how many sessions of it, as `coder=2`.
    #[arg(long = "agent", value_name = "NAME=SESSIONS")]
    pub agents: Vec<String>,
    /// Describe the hardware; detected when left out.
    #[arg(long)]
    pub hardware: Option<String>,
    /// Use this relay server instead of the default ones.
    #[arg(long, value_name = "URL")]
    pub relay: Option<String>,
}

#[derive(Args)]
pub struct CommentArgs {
    pub item: Option<String>,
    pub text: Vec<String>,
    /// Write the comment in `$VISUAL` or `$EDITOR`, starting from the text given.
    #[arg(long)]
    pub edit: bool,
}

#[derive(Args)]
pub struct InitArgs {
    #[arg(long)]
    pub name: String,
    /// Your handle, the `joshua` in `@joshua`. Defaults to `$USER`.
    #[arg(long)]
    pub handle: Option<String>,
    /// A name for this machine, shown by `wf device list`. Defaults to its host name.
    #[arg(long)]
    pub device_name: Option<String>,
    /// Do not define a default agent for each harness found on PATH.
    #[arg(long)]
    pub no_agents: bool,
}

#[derive(Subcommand)]
pub enum AgentCommand {
    Add(AgentAddArgs),
    List,
    Edit(AgentEditArgs),
}

#[derive(Args)]
pub struct AgentAddArgs {
    pub name: AgentName,
    #[arg(long)]
    pub harness: HarnessName,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long = "capability")]
    pub capabilities: Vec<Requirement>,
    #[arg(long = "lane")]
    pub lanes: Vec<LaneName>,
}

/// Fields that are given replace the current value.
#[derive(Args)]
pub struct AgentEditArgs {
    pub name: AgentName,
    #[arg(long)]
    pub harness: Option<HarnessName>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long = "capability")]
    pub capabilities: Vec<Requirement>,
    #[arg(long = "lane")]
    pub lanes: Vec<LaneName>,
}

#[derive(Subcommand)]
pub enum ItemCommand {
    Add(ItemAddArgs),
    List(ItemListArgs),
    Show {
        item: Option<String>,
        /// Print every event behind the item, including edits that lost a concurrent write.
        #[arg(long)]
        history: bool,
    },
    /// Items that can be worked on now.
    Ready,
    Edit(ItemEditArgs),
    Move {
        item: Option<String>,
        state: Option<StateName>,
        #[arg(long)]
        note: Option<String>,
    },
    Link(ItemLinkArgs),
    Archive {
        item: Option<String>,
    },
    Unarchive {
        item: Option<String>,
    },
}

/// Without `--title`, opens a new item as markdown with TOML front matter in `$VISUAL` or
/// `$EDITOR` when run at a terminal.
#[derive(Args)]
pub struct ItemAddArgs {
    #[arg(short, long)]
    pub title: Option<String>,
    #[arg(long = "type", default_value = "task")]
    pub item_type: TypeName,
    #[arg(long)]
    pub body: Option<String>,
    #[arg(long)]
    pub priority: Option<Priority>,
    #[arg(long)]
    pub lane: Option<LaneName>,
    #[arg(long = "label")]
    pub labels: Vec<Label>,
    #[arg(long = "requires")]
    pub requires: Vec<Requirement>,
    /// Open the item in the editor before creating it, filled in with the flags given.
    #[arg(long)]
    pub edit: bool,
}

#[derive(Args)]
pub struct ItemListArgs {
    #[arg(long = "type")]
    pub item_type: Option<TypeName>,
    #[arg(long)]
    pub state: Option<StateName>,
    /// Include archived items.
    #[arg(long)]
    pub archived: bool,
}

/// Without an item, picks one at a terminal. Without any flag, opens the item as markdown with TOML front matter in `$VISUAL` or `$EDITOR`
/// when run at a terminal.
#[derive(Args)]
pub struct ItemEditArgs {
    pub item: Option<String>,
    /// Open the item in the editor after applying the flags given.
    #[arg(long)]
    pub edit: bool,
    #[arg(short, long)]
    pub title: Option<String>,
    #[arg(long)]
    pub body: Option<String>,
    #[arg(long)]
    pub priority: Option<Priority>,
    #[arg(long)]
    pub lane: Option<LaneName>,
    #[arg(long = "add-label")]
    pub add_labels: Vec<Label>,
    #[arg(long = "remove-label")]
    pub remove_labels: Vec<Label>,
    #[arg(long = "add-requires")]
    pub add_requires: Vec<Requirement>,
    #[arg(long = "remove-requires")]
    pub remove_requires: Vec<Requirement>,
}

impl ItemEditArgs {
    pub fn has_changes(&self) -> bool {
        self.title.is_some()
            || self.body.is_some()
            || self.priority.is_some()
            || self.lane.is_some()
            || !self.add_labels.is_empty()
            || !self.remove_labels.is_empty()
            || !self.add_requires.is_empty()
            || !self.remove_requires.is_empty()
    }
}

#[derive(Args)]
#[command(group(ArgGroup::new("link").args(["depends_on", "remove_depends_on", "supersedes"])))]
pub struct ItemLinkArgs {
    pub item: Option<String>,
    #[arg(long)]
    pub depends_on: Option<String>,
    #[arg(long)]
    pub remove_depends_on: Option<String>,
    /// For decisions: the target moves to `superseded`.
    #[arg(long)]
    pub supersedes: Option<String>,
}

#[derive(Subcommand)]
pub enum PolicyCommand {
    /// Print the policy in effect.
    Show,
    /// Edit the policy in `$VISUAL` or `$EDITOR`, then apply it.
    Edit,
    /// Replace the policy with a file. Needs the principal key of an owner.
    Apply { file: PathBuf },
}
