use std::num::NonZeroUsize;
use std::path::PathBuf;

use clap::builder::{PossibleValuesParser, TypedValueParser};
use clap::{ArgGroup, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use wf_core::autonomy::{Autonomy, PermissionMode, StartMode};
use wf_core::graph::Readiness;
use wf_core::item::{ItemState, Priority};
use wf_core::name::{
    AgentName, CommandName, HarnessName, Label, LaneName, RecoveryLevelName, Requirement,
    ReviewLevelName, ReviewStepName, TypeName,
};

#[derive(Parser)]
#[command(
    name = "wf",
    max_term_width = 80,
    version,
    about = "Coordination for coding agents and their principals\n\n\
             Plain `wf` at a terminal opens the dashboard: what waits on you,\n\
             who works on what, items and agents."
)]
pub struct Cli {
    /// Print machine-readable output with a versioned schema.
    #[arg(long, global = true)]
    pub json: bool,

    /// Open the dashboard on this item.
    #[arg(long)]
    pub item: Option<String>,

    // `None` opens the dashboard at a terminal and prints the help otherwise.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// How the commands are grouped in `wf --help`, a blank line between groups. Clap lists
/// subcommands in one flat block.
const COMMAND_GROUPS: &[&[&str]] = &[
    &[
        "init", "status", "policy", "log", "stats", "export", "import", "members", "fork",
    ],
    &["item", "comment", "mentions", "review", "check", "inbox"],
    &["agent", "harness", "session", "work", "permissions"],
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
    let cli = Cli::from_arg_matches_mut(&mut matches)
        .unwrap_or_else(|error| error.format(&mut command()).exit());
    // `args_conflicts_with_subcommands` would refuse `--json` with a command as well.
    if cli.item.is_some() && cli.command.is_some() {
        command()
            .error(
                clap::error::ErrorKind::ArgumentConflict,
                "`--item` opens the dashboard on an item and takes no command",
            )
            .exit();
    }
    cli
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
    /// Comment on an item.
    ///
    /// Without text, opens the editor when run at a terminal.
    Comment(CommentArgs),
    /// Show where a principal or agent is mentioned.
    Mentions {
        /// `@ada` or `@ada/coder`
        target: String,
    },
    /// Show or change the policy.
    #[command(subcommand)]
    Policy(PolicyCommand),
    /// Print every event of the log, verified.
    Log(LogArgs),
    /// Sessions, submissions and change requests of each item worked on.
    ///
    /// For each item that was worked on: its sessions, submissions, change requests and the time
    /// from its first work session to done. The most submissions first.
    Stats,
    /// Start agents on items, and watch, stop or answer their sessions.
    #[command(subcommand)]
    Session(SessionCommand),
    /// Commands for agents, run inside a session.
    #[command(subcommand)]
    Work(WorkCommand),
    /// Give a verdict on the submitted work of an item.
    Review(ReviewArgs),
    /// Run the policy's checks on this checkout as it is.
    ///
    /// In the sandbox of checks, as they run before a merge: `wf check` before `wf work submit`,
    /// `wf check clippy` for one.
    Check {
        /// The checks to run, by their names in the policy. Defaults to all of them.
        names: Vec<CommandName>,
    },
    /// What waits on you: blockers, reviews, merges, permission requests.
    Inbox,
    /// Try out how this machine decides agents' permission requests.
    #[command(subcommand)]
    Permissions(PermissionsCommand),
    /// The agent harnesses this machine can start.
    #[command(subcommand)]
    Harness(HarnessCommand),
    /// Exchange events with the other devices of the workforce.
    Sync(SyncArgs),
    /// Start, stop or run the background process of this machine.
    #[command(subcommand)]
    Daemon(DaemonCommand),
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
    /// Fork the workforce, without the devices or members that were lost.
    ///
    /// Starts a new workforce from this one.
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
    /// Print the log as JSON lines, for backups and for `jq`.
    ///
    /// Every event of the log, one per line.
    Export,
    /// Add the events of a file `wf export` wrote to this machine.
    ///
    /// They go into the store of their workforce on this machine, which is created if there is
    /// none. The daemon must not run.
    ///
    /// With `--resign`, the export of an older wf, whose events this one does not read, becomes a
    /// new workforce: every event is signed again by this device, under a new genesis signed with
    /// the founder's principal key, which this machine must have. Ids change: a map of the old to
    /// the new is written beside the new store, and the repositories, branches and transcripts of
    /// this machine are moved over. Invites, binds, other devices and a fork's link to its parent
    /// are left out. The old workforce's directory is left as it was.
    Import {
        /// The file `wf export` wrote.
        file: PathBuf,
        /// Sign the export of an older wf again as a new workforce.
        #[arg(long)]
        resign: bool,
    },
    /// What `git fetch` runs to reach another device through the daemon: pipes its input and
    /// output to the socket the daemon handed git for the fetch, as descriptor 3.
    #[command(hide = true)]
    GitBridge,
}

#[derive(Args)]
pub struct LogArgs {
    /// Print the whole log, then the events as they arrive, until interrupted: one line for
    /// each thing an event did, as the dashboard shows them, instead of the table. With
    /// `--json`, one object per line in that form, instead of one object for the log.
    #[arg(long, short)]
    pub follow: bool,
    /// Print only the events of this item.
    #[arg(long)]
    pub item: Option<String>,
}

#[derive(Subcommand)]
pub enum PermissionsCommand {
    /// Run the deciders of the machine config on a permission request.
    ///
    /// The request is given as JSON on stdin, as hooks get it, or made up around a command.
    Test {
        /// A shell command the request asks to run.
        #[arg(long)]
        command: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum HarnessCommand {
    /// List the models a harness offers, for `wf agent add --model`.
    ///
    /// Starts the harness to ask it.
    Models {
        /// `claude-code`, `opencode`, `codex` or `gemini-cli`.
        harness: HarnessName,
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
    /// The principal has to decide. Only for agents.
    #[arg(long)]
    pub escalate: bool,
    /// Why, for whoever works on it next or decides after you.
    #[arg(long)]
    pub note: Option<String>,
}

#[derive(Subcommand)]
pub enum WorkCommand {
    /// The first prompt of this session: what to do and how to report.
    Brief,
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
    /// Start an agent on an item, in its own clone of the repository. Needs the daemon.
    Start(SessionStartArgs),
    /// List the sessions, live ones first.
    List {
        /// Only the sessions that have not ended, parked ones included.
        #[arg(long)]
        live: bool,
    },
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
    /// The review step to take, when the item awaits a review with several steps open. Without
    /// one, pick at a terminal.
    #[arg(short, long)]
    pub step: Option<ReviewStepName>,
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
    /// Follow a fork this device is a member of.
    ///
    /// Takes the fork in and uses it in this directory.
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

#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Start the daemon in the background.
    Start(DaemonArgs),
    /// Stop the daemon and its sessions.
    Stop,
    /// Stop the daemon if it runs, then start this `wf` in the background.
    ///
    /// A rebuilt `wf` takes over: the new daemon is the one that runs this command's binary.
    Restart(DaemonArgs),
    /// Whether the daemon of this machine runs, and where it logs.
    Status,
    /// Show the log of this machine's daemon in a pager.
    Log,
    /// Run the daemon in this terminal until it is interrupted.
    Run(DaemonArgs),
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
    /// Print a code to add a device. Needs your principal key.
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
    /// List the machines bound to the workforce.
    List,
    /// Change what this machine says about itself.
    Edit(DeviceEditArgs),
    /// Print a git allowed-signers file of the workforce's devices.
    ///
    /// It checks the merges they sign: `wf device allowed-signers > .git/allowed_signers`, then
    /// `git config gpg.ssh.allowedSignersFile .git/allowed_signers`.
    AllowedSigners,
}

/// Fields that are given replace the current value.
#[derive(Args)]
pub struct DeviceEditArgs {
    /// The name of this machine, shown by `wf device list`.
    #[arg(long)]
    pub name: Option<String>,
    /// What the machine has, such as `gpu, os:linux`; items and review steps that require it
    /// can run here.
    #[arg(long)]
    pub hardware: Option<String>,
    /// How many agent sessions this machine should run concurrently at most.
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
    /// What the machine has, such as `gpu, os:linux`; its os and arch when left out.
    #[arg(long)]
    pub hardware: Option<String>,
    /// Use this relay server instead of the default ones.
    #[arg(long, value_name = "URL")]
    pub relay: Option<String>,
}

#[derive(Args)]
pub struct CommentArgs {
    /// The item to comment on. Without one, pick at a terminal.
    pub item: Option<String>,
    /// What to say; `#item` refers to an item and `@handle` mentions someone.
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
    /// The name of the workforce.
    #[arg(long, required_unless_present = "interactive")]
    pub name: Option<String>,
    /// Ask for the name, and which of the harnesses found to define agents for, with their
    /// permissions and model.
    #[arg(long, short)]
    pub interactive: bool,
    /// Your handle, the `ada` in `@ada`. Defaults to `$USER`.
    #[arg(long)]
    pub handle: Option<String>,
    /// A name for this machine, shown by `wf device list`. Defaults to its host name.
    #[arg(long)]
    pub device_name: Option<String>,
}

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Define an agent: a harness, a model, and what it may work on.
    Add(AgentAddArgs),
    /// List your agents and those of the other members.
    List,
    /// Show an agent: its definition, devices, sessions and activity.
    ///
    /// The devices are those that host it.
    Show {
        /// The agent: its name for one of yours, `@ada/grace` for one of another member.
        agent: String,
    },
    /// Change an agent of yours.
    ///
    /// Fields that are given replace the current value.
    Edit(AgentEditArgs),
    /// Remove an agent of yours.
    ///
    /// Its sessions stay in the log; `wf agent add` brings it back.
    Remove {
        /// The agent to remove.
        name: AgentName,
    },
}

#[derive(Args)]
pub struct AgentAddArgs {
    /// The agent's name, the `grace` in `@ada/grace`. Defaults to the first name of a scientist
    /// that no agent in the workforce has.
    pub name: Option<AgentName>,
    /// The program that runs the agent: `claude-code`, `opencode`, `codex` or `gemini-cli`.
    #[arg(long)]
    pub harness: HarnessName,
    /// Define the harness although this machine cannot start it, because another member's
    /// machine can.
    #[arg(long)]
    pub harness_unknown_here: bool,
    /// The model the harness runs, as `wf harness models` lists them. Defaults to the
    /// harness's own choice.
    #[arg(long)]
    pub model: Option<String>,
    /// What the agent can do, matched against what items require, such as `rust` or
    /// `review`; repeat for several.
    #[arg(long = "capability")]
    pub capabilities: Vec<Requirement>,
    /// The lanes the agent takes items from. Defaults to `default`; `supervision` alone makes an
    /// agent that only answers questions and holds no items.
    #[arg(long = "lane", default_value = "default")]
    pub lanes: Vec<LaneName>,
    /// What the agent is told in every brief, such as how strictly to review.
    #[arg(long)]
    pub instructions: Option<String>,
    #[command(flatten)]
    pub autonomy: AutonomyArgs,
}

// How much the agent does without the principal. Fields that are not given keep their default, or
// with `wf agent edit` their current value.
#[derive(Args)]
#[command(next_help_heading = "Autonomy")]
pub struct AutonomyArgs {
    /// `auto` lets the dispatcher start the agent on ready items.
    #[arg(long)]
    pub start: Option<StartMode>,
    /// `allow` (the default), `ask` or `deny` the permission requests of its sessions. A machine
    /// that runs agents without a sandbox asks instead of allowing.
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

#[derive(Args)]
pub struct AgentEditArgs {
    /// The agent to change.
    pub name: AgentName,
    /// The program that runs the agent.
    #[arg(long)]
    pub harness: Option<HarnessName>,
    /// Define the harness although this machine cannot start it, because another member's
    /// machine can.
    #[arg(long)]
    pub harness_unknown_here: bool,
    /// The model the harness runs, as `wf harness models` lists them.
    #[arg(long)]
    pub model: Option<String>,
    /// Replaces the capabilities; repeat for several.
    #[arg(long = "capability")]
    pub capabilities: Vec<Requirement>,
    /// Replaces the lanes the agent takes items from; repeat for several.
    #[arg(long = "lane")]
    pub lanes: Vec<LaneName>,
    /// What the agent is told in every brief; an empty text removes them.
    #[arg(long)]
    pub instructions: Option<String>,
    #[command(flatten)]
    pub autonomy: AutonomyArgs,
}

#[derive(Subcommand)]
pub enum ItemCommand {
    /// Create an item; without `--title`, in `$VISUAL` or `$EDITOR`.
    ///
    /// Without `--title`, write it in `$VISUAL` or `$EDITOR` at a terminal.
    Add(ItemAddArgs),
    /// List the live items, filtered and grouped.
    List(ItemListArgs),
    /// Find items by words in their title, body and comments.
    Search(ItemSearchArgs),
    /// Show an item: its comments, links, review and what happened to it.
    Show {
        /// The start of an item id. Without one, pick at a terminal.
        item: Option<String>,
    },
    /// Change an item; without flags, in `$VISUAL` or `$EDITOR`.
    ///
    /// Without flags, edit it in `$VISUAL` or `$EDITOR` at a terminal.
    Edit(ItemEditArgs),
    /// Tick steps of an item: `wf item check k3f 1 3`.
    ///
    /// The steps are numbered as `wf item show` prints them. `wf check` runs the policy's checks
    /// instead.
    Check(ItemCheckArgs),
    /// Release drafts or reopen items: `wf item open k3f a8c`.
    ///
    /// Released drafts go to the dispatcher.
    Open(ItemMoveArgs),
    /// Hold items back as drafts, until `wf item open`.
    ///
    /// Nothing starts a draft until `wf item open`.
    Draft(ItemMoveArgs),
    /// Finish items without a session. For a proposed decision: accept it.
    Done(ItemMoveArgs),
    /// Cancel an item. For a proposed decision: reject it.
    ///
    /// A session working on it stops after its current turn.
    Cancel {
        /// The start of an item id. Without one, pick at a terminal.
        item: Option<String>,
        /// Why, kept with the move.
        #[arg(long)]
        note: Option<String>,
    },
    /// Add or remove a dependency, or say a decision supersedes another.
    Link(ItemLinkArgs),
    /// Show the work of an item through git.
    ///
    /// The pager, colors and diff settings of git apply.
    Diff {
        /// The start of an item id. Without one, pick at a terminal.
        item: Option<String>,
        /// Passed on to `git diff`, such as `--stat` or `--diff-algorithm patience -- src/`; paths follow `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        git_args: Vec<String>,
    },
    /// Check out the work of an item in the main checkout; `-` goes back.
    ///
    /// The work is checked out detached.
    Checkout {
        /// The item, or `-` for where the checkout was before.
        item: Option<String>,
    },
    /// Print the directory of the agent repository of an item, as in `cd $(wf item path k3f7)`.
    Path {
        /// The start of an item id. Without one, pick at a terminal.
        item: Option<String>,
    },
    /// Hide an item from lists; its history stays.
    ///
    /// `--done` and `--cancelled` archive every live item that is done or cancelled.
    Archive {
        /// The start of an item id. Without one, pick at a terminal.
        #[arg(conflicts_with_all = ["done", "cancelled"])]
        item: Option<String>,
        /// Archive every live item that is done.
        #[arg(long)]
        done: bool,
        /// Archive every live item that is cancelled.
        #[arg(long)]
        cancelled: bool,
    },
    /// Bring an archived item back.
    Unarchive {
        /// The start of an item id. Without one, pick at a terminal.
        item: Option<String>,
    },
}

#[derive(Args)]
pub struct ItemMoveArgs {
    /// The starts of item ids. Without any, pick one at a terminal.
    pub items: Vec<String>,
    /// Why, kept with each move.
    #[arg(long)]
    pub note: Option<String>,
}

/// The states, as `--state` takes them: the values complete and show in `--help`.
fn item_state_parser() -> impl TypedValueParser<Value = ItemState> {
    PossibleValuesParser::new(ItemState::ALL.map(ItemState::name))
        .map(|name| name.parse().expect("every possible value is a state"))
}

/// Without `--title`, opens a new item as markdown with TOML front matter in `$VISUAL` or
/// `$EDITOR` when run at a terminal.
#[derive(Args)]
pub struct ItemAddArgs {
    /// What the item is about, in one line.
    #[arg(short, long)]
    pub title: Option<String>,
    /// A type of the policy, such as `task`, `decision` or `distill`.
    #[arg(long = "type", default_value = "task")]
    pub item_type: TypeName,
    /// The description, in markdown; `- [ ]` lines are steps.
    #[arg(long, allow_hyphen_values = true)]
    pub body: Option<String>,
    /// `P0` (most urgent) to `P3`.
    #[arg(long)]
    pub priority: Option<Priority>,
    /// The lane whose agents take it. Defaults to its type's lane.
    #[arg(long)]
    pub lane: Option<LaneName>,
    /// A label; repeat for several.
    #[arg(long = "label")]
    pub labels: Vec<Label>,
    /// What an agent or machine needs to take it, such as `rust` or `gpu`; repeat for several.
    #[arg(long = "requires")]
    pub requires: Vec<Requirement>,
    /// The review level the work needs, a name from the policy's `[[review]]` list.
    #[arg(long)]
    pub review: Option<ReviewLevelName>,
    /// What happens when a session on it ends without finishing, a name from the policy's
    /// `[[recovery]]` list.
    #[arg(long)]
    pub recovery: Option<RecoveryLevelName>,
    /// Create it as a draft: nothing starts it until `wf item open`.
    #[arg(long)]
    pub draft: bool,
    /// Open the item in the editor before creating it, filled in with the flags given.
    #[arg(long)]
    pub edit: bool,
}

/// The filters `wf item list` and `wf item search` narrow the items with.
#[derive(Args)]
pub struct ItemFilterArgs {
    /// Only items of this type.
    #[arg(long = "type")]
    pub item_type: Option<TypeName>,
    /// Only items in these states, such as `draft,open`.
    #[arg(long, value_delimiter = ',', value_parser = item_state_parser())]
    pub state: Vec<ItemState>,
    /// Only items with this label.
    #[arg(long)]
    pub label: Option<Label>,
    /// Only items in this lane, their own or their type's.
    #[arg(long)]
    pub lane: Option<LaneName>,
    /// Only items of this priority.
    #[arg(long)]
    pub priority: Option<Priority>,
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
}

#[derive(Args)]
pub struct ItemListArgs {
    #[command(flatten)]
    pub filters: ItemFilterArgs,
    /// Show the items in groups.
    #[arg(long, value_enum)]
    pub group_by: Option<ItemGrouping>,
    /// The order of the items, within each group.
    #[arg(long, value_enum, default_value_t = ItemOrder::Priority)]
    pub sort: ItemOrder,
}

#[derive(Args)]
pub struct ItemSearchArgs {
    /// The words to find, in any order and case. An item must have all of them in its title, body
    /// and comments together; an item id or its start (`#7ph6ds`) finds that item.
    #[arg(required = true)]
    pub query: Vec<String>,
    #[command(flatten)]
    pub filters: ItemFilterArgs,
}

/// What `wf item list --sort` orders by. Archived items come after the live ones, by when they
/// were archived.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ItemOrder {
    /// By priority, `P0` first and items without one last, then by creation.
    #[default]
    Priority,
    /// By creation, the oldest first.
    Created,
    /// In the order their work went into the target, by the latest merge; items not merged
    /// after them, by priority.
    Merged,
}

/// What `wf item list --group-by` groups by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ItemGrouping {
    /// By state, in the order `draft`, `open`, `active`, `review`, `done`, `cancelled`.
    State,
    /// Open items by `ready`, `claimed`, `blocked` and `stranded`; the rest as `not_open`.
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

impl ItemFilterArgs {
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
    /// The start of an item id. Without one, pick at a terminal.
    pub item: Option<String>,
    /// Open the item in the editor after applying the flags given.
    #[arg(long)]
    pub edit: bool,
    /// Replaces the title.
    #[arg(short, long)]
    pub title: Option<String>,
    /// Replaces the description.
    #[arg(long, allow_hyphen_values = true)]
    pub body: Option<String>,
    /// `P0` (most urgent) to `P3`.
    #[arg(long)]
    pub priority: Option<Priority>,
    /// The lane whose agents take it.
    #[arg(long)]
    pub lane: Option<LaneName>,
    /// Add a label; repeat for several.
    #[arg(long = "add-label")]
    pub add_labels: Vec<Label>,
    /// Remove a label; repeat for several.
    #[arg(long = "remove-label")]
    pub remove_labels: Vec<Label>,
    /// Add a requirement; repeat for several.
    #[arg(long = "add-requires")]
    pub add_requires: Vec<Requirement>,
    /// Remove a requirement; repeat for several.
    #[arg(long = "remove-requires")]
    pub remove_requires: Vec<Requirement>,
    /// The review level the work needs, a name from the policy's `[[review]]` list, or `default`
    /// for the level of the item's type.
    #[arg(long)]
    pub review: Option<LevelChoice<ReviewLevelName>>,
    /// What happens when a session on it ends without finishing, a name from the policy's
    /// `[[recovery]]` list, or `default` for the level of the item's type.
    #[arg(long)]
    pub recovery: Option<LevelChoice<RecoveryLevelName>>,
}

/// What `wf item edit --review` and `--recovery` set.
#[derive(Clone, Debug)]
pub enum LevelChoice<Name> {
    /// The item follows the level of its type.
    Default,
    Level(Name),
}

impl<Name: std::str::FromStr> std::str::FromStr for LevelChoice<Name> {
    type Err = Name::Err;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "default" => Ok(LevelChoice::Default),
            level => level.parse().map(LevelChoice::Level),
        }
    }
}

impl<Name> LevelChoice<Name> {
    /// The level the item names itself: none when it follows its type.
    pub fn into_level(self) -> Option<Name> {
        match self {
            LevelChoice::Default => None,
            LevelChoice::Level(name) => Some(name),
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
            || self.recovery.is_some()
            || !self.add_labels.is_empty()
            || !self.remove_labels.is_empty()
            || !self.add_requires.is_empty()
            || !self.remove_requires.is_empty()
    }
}

#[derive(Args)]
pub struct ItemCheckArgs {
    /// The start of an item id.
    pub item: String,
    /// The numbers of the steps.
    #[arg(required = true)]
    pub numbers: Vec<NonZeroUsize>,
    /// Unticks the steps instead.
    #[arg(long)]
    pub remove: bool,
}

#[derive(Args)]
#[command(group(ArgGroup::new("link").args(["depends_on", "remove_depends_on", "supersedes"])))]
pub struct ItemLinkArgs {
    /// The start of an item id. Without one, pick at a terminal.
    pub item: Option<String>,
    /// The item cannot start before this one is done.
    #[arg(long)]
    pub depends_on: Option<String>,
    /// Remove a dependency.
    #[arg(long)]
    pub remove_depends_on: Option<String>,
    /// For decisions: this one replaces the target, which keeps its state and shows as
    /// superseded.
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
    Apply {
        /// A TOML file with the whole policy.
        file: PathBuf,
    },
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
            .filter(|subcommand| !subcommand.is_hide_set())
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

#[cfg(test)]
mod help_tests {
    /// Every command, argument and flag says what it is for.
    #[test]
    fn everything_has_help() {
        fn walk(command: &clap::Command, path: &str, missing: &mut Vec<String>) {
            for argument in command.get_arguments() {
                let builtin = matches!(argument.get_id().as_str(), "help" | "version");
                if !builtin && argument.get_help().is_none() && !argument.is_hide_set() {
                    missing.push(format!("{path} {}", argument.get_id()));
                }
            }
            for sub in command.get_subcommands() {
                let sub_path = format!("{path} {}", sub.get_name());
                if sub.get_about().is_none() && sub.get_name() != "help" {
                    missing.push(sub_path.clone());
                }
                walk(sub, &sub_path, missing);
            }
        }
        let mut missing = Vec::new();
        walk(&super::command(), "wf", &mut missing);
        assert!(missing.is_empty(), "no help for:\n{}", missing.join("\n"));
    }
}
