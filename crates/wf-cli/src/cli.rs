use std::num::NonZeroUsize;
use std::path::PathBuf;

use clap::{ArgGroup, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use wf_core::autonomy::{Autonomy, PermissionMode, StartMode};
use wf_core::graph::Readiness;
use wf_core::item::Priority;
use wf_core::name::{
    AgentName, HarnessName, Label, LaneName, Requirement, ReviewName, StateName, TypeName,
};

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

/// How the commands are grouped in `wf --help`, a blank line between groups. Clap lists
/// subcommands in one flat block.
const COMMAND_GROUPS: &[&[&str]] = &[
    &["init", "status", "policy", "log", "members", "fork"],
    &["item", "comment", "mentions", "review", "inbox"],
    &["agent", "session", "work"],
    &["daemon", "sync", "invite", "join", "device"],
];

/// The command line, with the commands of `wf --help` in groups.
pub fn command() -> clap::Command {
    let command = Cli::command();
    let styles = command.get_styles().clone();
    let heading = styles.get_header();
    let literal = styles.get_literal();
    let width = COMMAND_GROUPS
        .iter()
        .flat_map(|names| names.iter())
        .map(|name| name.len())
        .max()
        .unwrap_or_default();
    let mut template = format!(
        "{{about}}\n\n{heading}Usage:{heading:#} {{usage}}\n\n{heading}Commands:{heading:#}"
    );
    for names in COMMAND_GROUPS {
        template.push('\n');
        for name in *names {
            let about = command
                .find_subcommand(name)
                .and_then(|subcommand| subcommand.get_about())
                .map(|about| about.to_string())
                .unwrap_or_default();
            let about = about.lines().next().unwrap_or_default();
            template.push_str(&format!("  {literal}{name:<width$}{literal:#}  {about}\n"));
        }
    }
    template.push_str(&format!(
        "\n{heading}Options:{heading:#}\n{{options}}{{after-help}}"
    ));
    command.help_template(template)
}

/// Parses the arguments of this process, and exits with help or an error as clap does.
pub fn parse() -> Cli {
    let mut matches = command().get_matches();
    Cli::from_arg_matches_mut(&mut matches)
        .unwrap_or_else(|error| error.format(&mut command()).exit())
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a workforce for the current directory.
    Init(InitArgs),
    /// Manage agent definitions.
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Create and manage items: tasks, decisions.
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
    /// Start agents on items, and watch, stop or answer their sessions.
    #[command(subcommand)]
    Session(SessionCommand),
    /// Commands for agents, run inside a session.
    #[command(subcommand)]
    Work(WorkCommand),
    /// Give a verdict on the submitted work of an item.
    Review(ReviewArgs),
    /// What waits on you: blockers, reviews, merges and permission requests.
    Inbox,
    /// Exchange events with the other devices of the workforce.
    Sync(SyncArgs),
    /// Run the background process of this machine
    Daemon(DaemonArgs),
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
    /// Start a new workforce from this one, without the devices or members that were lost.
    #[command(args_conflicts_with_subcommands = true)]
    Fork {
        #[command(flatten)]
        args: ForkArgs,
        #[command(subcommand)]
        command: Option<ForkCommand>,
    },
    /// Summarize the workforce.
    Status {
        /// Ignore the snapshot and fold the whole log again.
        #[arg(long)]
        refold: bool,
    },
}

#[derive(Args)]
#[command(group(ArgGroup::new("verdict").required(true).args(["approve", "request_changes", "escalate"])))]
pub struct ReviewArgs {
    /// Defaults to the item of your review session.
    pub item: Option<String>,
    /// The work is good as it is.
    #[arg(long)]
    pub approve: bool,
    /// The work needs changes. The item goes back to open for the next session.
    #[arg(long)]
    pub request_changes: bool,
    /// A person has to decide. Only for agents.
    #[arg(long)]
    pub escalate: bool,
    /// Why, for whoever works on it next or decides after you.
    #[arg(long)]
    pub note: Option<String>,
}

#[derive(Subcommand)]
pub enum WorkCommand {
    /// The work is finished: end the session and hand its commits on.
    Submit {
        /// What the next reader should know.
        #[arg(long)]
        note: Option<String>,
    },
    /// Give up: end the session so the item can be picked up again.
    Release {
        /// Why, for whoever picks it up.
        #[arg(long)]
        note: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum SessionCommand {
    /// Start an agent on an item, in its own worktree. Needs the daemon.
    Start(SessionStartArgs),
    /// List the sessions, live ones first.
    List,
    /// Show one session.
    Show {
        /// The start of a session id. Without one, pick at a terminal.
        session: Option<String>,
    },
    /// Stop a live session of this machine. Its item goes back to open.
    Stop {
        /// The start of a session id. Without one, pick at a terminal.
        session: Option<String>,
    },
    /// Let a session do what it asked permission for.
    Allow {
        /// The start of a session id. Without one, pick at a terminal.
        session: Option<String>,
    },
    /// Refuse what a session asked permission for.
    Deny {
        /// The start of a session id. Without one, pick at a terminal.
        session: Option<String>,
    },
    /// Print a transcript and follow it while the agent is still writing.
    Watch(SessionWatchArgs),
}

#[derive(Args)]
pub struct SessionStartArgs {
    /// Without one, pick at a terminal.
    pub item: Option<String>,
    /// One of your agents, as listed by `wf agent list`. Defaults to the policy's `default_agent`.
    #[arg(short, long)]
    pub agent: Option<AgentName>,
    /// Follow the transcript right after starting. Quitting the watch leaves the session running.
    #[arg(long)]
    pub watch: bool,
    /// Show all tool output and diff lines when watching.
    #[arg(long, requires = "watch")]
    pub full: bool,
    /// Use plain text layout without markdown rendering or color when watching.
    #[arg(long, requires = "watch")]
    pub raw: bool,
}

#[derive(Args)]
pub struct SessionWatchArgs {
    /// The start of a session id, or of an item id for its latest session. Without one, pick at
    /// a terminal.
    pub session: Option<String>,
    /// Print what is there and stop, instead of waiting for more.
    #[arg(long)]
    pub no_follow: bool,
    /// Show all tool output and diff lines.
    #[arg(long)]
    pub full: bool,
    /// Use plain text layout without markdown rendering or color.
    #[arg(long)]
    pub raw: bool,
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

#[derive(Args)]
pub struct ForkArgs {
    /// A device (its name or the start of its key) or a member (`@handle`) the fork leaves out.
    #[arg(long = "exclude", value_name = "DEVICE|@MEMBER")]
    pub exclude: Vec<String>,
    /// Found the fork with a new principal key, for when a lost machine held yours.
    #[arg(long)]
    pub new_principal: bool,
    /// The name of the new workforce. Defaults to the old name and "(fork)".
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Subcommand)]
pub enum ForkCommand {
    /// Follow a fork this device is a member of: take it in and use it in this directory.
    Accept {
        /// The id of the new workforce, as shown by `wf status` or `wf fork list`.
        id: String,
        /// Use this relay server instead of the default ones.
        #[arg(long, value_name = "URL")]
        relay: Option<String>,
    },
    /// Show what this workforce was forked from and the forks announced in it.
    List,
}

#[derive(Args)]
pub struct DaemonArgs {
    /// Use this relay server instead of the default ones.
    #[arg(long, value_name = "URL")]
    pub relay: Option<String>,
    /// Neither relays nor address lookup: for tests.
    #[arg(long, hide = true)]
    pub no_network: bool,
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
    /// A blocker: the work cannot start or continue until someone else resolves it.
    #[arg(long, conflicts_with = "request_change")]
    pub block: bool,
    /// A change request: the work has to do this before it is submitted.
    #[arg(long)]
    pub request_change: bool,
    /// Resolve this blocker or change request of the item; repeat for several.
    #[arg(long, value_name = "COMMENT")]
    pub resolve: Vec<String>,
    /// Resolve every open blocker of the item.
    #[arg(long)]
    pub resolve_blockers: bool,
    /// Resolve every open change request of the item.
    #[arg(long)]
    pub resolve_change_requests: bool,
    /// The step the comment is about, numbered as `wf item show` prints them.
    #[arg(long, value_name = "NUMBER")]
    pub step: Option<NonZeroUsize>,
    /// The comment concludes the step, so it is checked off too.
    #[arg(long, requires = "step")]
    pub done: bool,
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
    #[command(flatten)]
    pub autonomy: AutonomyArgs,
}

/// How much the agent does without a person. Fields that are not given keep their default, or
/// with `wf agent edit` their current value.
#[derive(Args)]
pub struct AutonomyArgs {
    /// `auto` lets the dispatcher start the agent on ready items.
    #[arg(long)]
    pub start: Option<StartMode>,
    /// `allow`, `ask` or `deny` the permission requests of its sessions.
    #[arg(long)]
    pub permissions: Option<PermissionMode>,
    /// Turns in a row without a commit before a session is released.
    #[arg(long)]
    pub nudges: Option<u32>,
    /// How long a session may wait for an answer, such as `2h`.
    #[arg(long, value_name = "DURATION")]
    pub park_timeout: Option<humantime::Duration>,
    /// How long a session may run, such as `1h`.
    #[arg(long, value_name = "DURATION")]
    pub max_duration: Option<humantime::Duration>,
}

impl AutonomyArgs {
    pub fn applied_to(self, mut autonomy: Autonomy) -> Autonomy {
        autonomy.start = self.start.unwrap_or(autonomy.start);
        autonomy.permissions = self.permissions.unwrap_or(autonomy.permissions);
        autonomy.nudges = self.nudges.unwrap_or(autonomy.nudges);
        autonomy.park_timeout = self.park_timeout.map_or(autonomy.park_timeout, Into::into);
        autonomy.max_duration = self.max_duration.map(Into::into).or(autonomy.max_duration);
        autonomy
    }
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
    #[command(flatten)]
    pub autonomy: AutonomyArgs,
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
    Edit(ItemEditArgs),
    Check(ItemCheckArgs),
    Move {
        item: Option<String>,
        state: Option<StateName>,
        #[arg(long)]
        note: Option<String>,
    },
    Link(ItemLinkArgs),
    /// Show what the work of an item changes in the repository, as a review sees it.
    Diff {
        item: Option<String>,
    },
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
    #[arg(long, allow_hyphen_values = true)]
    pub body: Option<String>,
    #[arg(long)]
    pub priority: Option<Priority>,
    #[arg(long)]
    pub lane: Option<LaneName>,
    #[arg(long = "label")]
    pub labels: Vec<Label>,
    #[arg(long = "requires")]
    pub requires: Vec<Requirement>,
    /// The review level the work needs, a name from the policy's `[[review]]` list.
    #[arg(long)]
    pub review: Option<ReviewName>,
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
    /// Only open items whose dependencies are all done, so they can be worked on now.
    #[arg(short, long)]
    pub ready: bool,
    /// Only the items this agent of yours would be given on this machine.
    #[arg(long = "for", value_name = "AGENT", requires = "ready")]
    pub for_agent: Option<AgentName>,
    /// Only open items waiting on dependencies that are not done yet.
    #[arg(long)]
    pub blocked: bool,
    /// Only open items with a cancelled dependency; someone has to act to free them.
    #[arg(long)]
    pub stranded: bool,
    /// Show the items in groups.
    #[arg(long, value_enum)]
    pub group_by: Option<ItemGrouping>,
}

/// What `wf item list --group-by` groups by.
#[derive(Clone, Copy, ValueEnum)]
pub enum ItemGrouping {
    /// By state, ordered by state category: `open`, `in_progress`, `done`, ...
    State,
    /// Open items by `ready`, `blocked` and `stranded`; the rest as `not_open`.
    Readiness,
    /// By label, alphabetically; an item with several labels appears under each, and items
    /// without one under `unlabeled`.
    Label,
    /// By item type, alphabetically.
    Type,
    /// By lane, alphabetically; items without one under `no_lane`.
    Lane,
    /// By priority, `P0` first; items without one under `no_priority`.
    Priority,
}

impl ItemListArgs {
    /// Whether an item passes the `--ready`, `--blocked` and `--stranded` filters; given together
    /// they match any of them.
    pub fn accepts(&self, readiness: Option<Readiness>) -> bool {
        if !(self.ready || self.blocked || self.stranded) {
            return true;
        }
        match readiness {
            Some(Readiness::Ready) => self.ready,
            Some(Readiness::Claimed) => false,
            Some(Readiness::Blocked) => self.blocked,
            Some(Readiness::Stranded) => self.stranded,
            None => false,
        }
    }
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
    #[arg(long, allow_hyphen_values = true)]
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
    /// The review level the work needs, a name from the policy's `[[review]]` list, or `default`
    /// for the level of the item's type.
    #[arg(long)]
    pub review: Option<ReviewChoice>,
}

/// What `wf item edit --review` sets.
#[derive(Clone, Debug)]
pub enum ReviewChoice {
    /// The item follows the review level of its type.
    Default,
    Level(ReviewName),
}

impl std::str::FromStr for ReviewChoice {
    type Err = <ReviewName as std::str::FromStr>::Err;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "default" => Ok(ReviewChoice::Default),
            level => level.parse().map(ReviewChoice::Level),
        }
    }
}

impl ItemEditArgs {
    pub fn has_changes(&self) -> bool {
        self.title.is_some()
            || self.body.is_some()
            || self.priority.is_some()
            || self.lane.is_some()
            || self.review.is_some()
            || !self.add_labels.is_empty()
            || !self.remove_labels.is_empty()
            || !self.add_requires.is_empty()
            || !self.remove_requires.is_empty()
    }
}

/// Ticks steps of an item, numbered as `wf item show` prints them: `wf item check #abc 1 3`.
#[derive(Args)]
pub struct ItemCheckArgs {
    pub item: String,
    #[arg(required = true)]
    pub numbers: Vec<NonZeroUsize>,
    /// Unticks the steps instead.
    #[arg(long)]
    pub remove: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_is_in_exactly_one_help_group() {
        let command = Cli::command();
        let grouped: Vec<&str> = COMMAND_GROUPS
            .iter()
            .flat_map(|names| names.iter().copied())
            .collect();
        let mut missing: Vec<_> = command
            .get_subcommands()
            .map(|subcommand| subcommand.get_name())
            .filter(|name| *name != "help" && !grouped.contains(name))
            .collect();
        missing.sort();
        assert!(missing.is_empty(), "not in a help group: {missing:?}");
        for name in &grouped {
            assert!(
                command.find_subcommand(name).is_some(),
                "no command `{name}`"
            );
            assert_eq!(
                grouped.iter().filter(|other| *other == name).count(),
                1,
                "`{name}` twice"
            );
        }
    }
}
