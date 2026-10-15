# The machine config

The policy says how the workforce works, and it syncs to every machine. What only makes sense on
one machine lives in its **machine config**: the sandbox agents and checks run in, the environment
they get, how commits are signed, how you are notified, the colours of `wf`, and who decides
permission requests (see [permissions.md](permissions.md)). It is never synced, so another machine
can never make yours run a program or call a paid API.

The file is `~/.config/workforce/config.toml`, or `$XDG_CONFIG_HOME/workforce/config.toml` when that
is set. Without a file, every default below applies. Unknown keys are an error, so a typo does not
pass silently. The daemon reads the file anew each time it needs it, so a change applies to the next
session or permission request without a restart.

A complete example:

```toml
[sandbox]
kind = "fence"
allow_write = ["~/.cache/workforce/target"]
allow_domains = ["*.example.com"]

[environment]
inherit = ["ANTHROPIC_API_KEY"]
set = { CARGO_TARGET_DIR = "~/.cache/workforce/target" }

[agent.coder]
environment = { set = { RUST_LOG = "debug" } }
sandbox = { allow_domains = ["docs.rs"] }

[commits]
agent_signing = "unsigned"
merge_signing = "as_configured"

[notify]
command = ["terminal-notifier", "-title", "wf: {title}", "-message", "{what}"]

[check]
max_timeout = "15m"

[permissions]
guidance = "Allow reading anything and writing in the worktree. Ask about the network."

[theme]
accent = "#1e50a0"
state.review = "bold yellow"
```

## The sandbox

The daemon runs every agent in [Fence](https://github.com/fencesandbox/fence), which has to be
installed. Inside it, an agent may:

* write in its own worktree, and where it needs to commit and talk to the daemon;
* reach model APIs and package registries.

Everything else is denied. Whatever you add, an agent never reads wf's keys or config, and never
changes git's config or hooks. Because the sandbox holds agents in, an agent with
`--permissions allow` (the default) is allowed what it asks without anyone being asked.

```toml
[sandbox]
kind = "fence"                   # the default
allow_write = ["/opt/cache"]     # more paths agents may write to
allow_domains = ["*.example.com"]   # more domains agents may reach
```

`kind = "off"` runs agents with your own privileges. Then an agent set to `allow` is asked instead:
each permission request goes through the deciders and then to you. `allow_write` and
`allow_domains` are refused with `kind = "off"`, since there is nothing to add them to.

## The environment

Agents do not get the daemon's environment. They get:

* what wf sets: `WF_SESSION`, `WF_WORKFORCE`, `PATH` and their git identity;
* what every program expects: `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `LANG`, `LC_ALL`,
  `LC_CTYPE` and the `XDG_*_HOME` directories;
* what the machine config names.

So the API key a model decider uses does not reach the agents unless you pass it on.

```toml
[environment]
inherit = ["ANTHROPIC_API_KEY"]  # passed on from the daemon's environment, when it is set
set = { CARGO_TARGET_DIR = "~/.cache/workforce/target" }
```

Values under `set` are expanded when the config is read: `~`, `$NAME` and `${NAME}` come from the
daemon's environment, and `$$` is a literal `$`. A variable that is not set is an error. A relative
path is relative to the agent's directory, its worktree. wf's own variables (`PATH`, `WF_*`,
`GIT_AUTHOR_NAME`, `GIT_COMMITTER_NAME`, `GIT_CONFIG_*`) cannot be set here.

Setting a variable allows nothing by itself: a directory an agent is to write to goes in
`allow_write` as well.

### A shared build cache

Each item gets its own worktree, so each would build from scratch. One build directory for every
worktree saves the time and the disk:

```toml
[environment]
set = { CARGO_TARGET_DIR = "~/.cache/workforce/target" }

[sandbox]
kind = "fence"
allow_write = ["~/.cache/workforce/target"]
```

The same works for other build tools with a cache directory set by a variable.

## Storage

A shared build cache mixes the builds of different branches; one in each worktree (cargo's default
`target`, without `CARGO_TARGET_DIR`) does not, but each takes its own space, also while its item
waits in review. With a budget, the daemon keeps the caches in the worktrees under it: when a session
ends and when the daemon starts, it deletes the caches of the least recently built worktree that no
session runs in, whole, until the rest fits. A cold build the next time is the only cost. Without
`[storage]`, nothing is deleted. `wf status` shows what the caches take.

```toml
[storage]
budget = "20GB"
caches = ["target"]              # paths inside a worktree that can be rebuilt
```

The checkout of a review goes when the review ends, the worktree of an item when it is done or
cancelled, and a daemon that starts removes whatever else is left in its worktrees directory.

## One agent

`[agent.<name>]` gives one of your agents more, on top of what every agent gets. Where both set the
same variable, the agent's value wins.

```toml
[agent.coder]
environment = { inherit = ["SSH_AUTH_SOCK"], set = { RUST_LOG = "debug" } }
sandbox = { allow_domains = ["docs.rs"], allow_write = ["~/.cache/coder"] }
```

## Checks

The checks of the policy (`[[merge.check]]`) run on the merge result before the target moves, in
the workforce's check worktree. They get the environment of every agent, without
`CARGO_TARGET_DIR`, so their build output stays their own. Commits they make, such as those of
tests, are not signed: they read your global git config with signing turned off, so they never sign
with your key. A repository that turns signing on in its own config, as tests of signing do, still
signs.

By default they run in the sandbox of every agent, `[sandbox]`, without what `[agent.<name>]` adds.
Some tests need what Fence cannot allow: on macOS, iroh watches the routes through a socket that
Fence denies whatever its settings, so wf's own network tests fail in it. Then let the checks run
without a sandbox, with your privileges, on this machine only:

```toml
[check]
max_timeout = "20m"              # the longest a check runs here, whatever the policy says
sandbox = { kind = "off" }       # or a sandbox of their own, as under [sandbox]; default: [sandbox]
```

Checks run the code of the work they check, so without a sandbox an agent's code runs with your
privileges at merge, also for work that no one reviewed. A check that fails says the sandbox it ran
in. `wf check` outside a session runs them the same way; in a session it runs them in the session's
sandbox, with the policy's timeouts.

## Commits and signatures

Agents commit as themselves, `@you/coder`, with the email of your git config. They commit unsigned,
so they never wait for a key you have to unlock or approve.

When your git config signs commits (`commit.gpgsign`), the merge commit that brings approved work
into the target branch is signed as you: the signature says you accepted the work. Or the daemon
signs it with its device's key, which needs nobody to approve it.

```toml
[commits]
agent_signing = "unsigned"       # the default; "as_configured" signs as your git config says
merge_signing = "as_configured"  # the default; "unsigned" never signs, "device" signs with the device key
```

With `merge_signing = "device"`, let git check those merges against the devices of the workforce:

```sh
wf device allowed-signers > .git/allowed_signers
git config gpg.ssh.allowedSignersFile .git/allowed_signers
```

`git log --show-signature` and `git verify-commit` then name the device that signed, as
`<handle>@<device name>`.

## Notifications

With a command under `[notify]`, the daemon runs it once for each new entry of `wf inbox`: a
blocker, a proposal to decide, a review that is yours, a merge that waits, sessions that did not
finish, a permission request.

```toml
[notify]
command = ["terminal-notifier", "-title", "wf: {title}", "-message", "{what}"]          # macOS
# command = ["osascript", "-e", "on run argv", "-e", "display notification (item 2 of argv) with title (item 1 of argv)", "-e", "end run", "wf: {title}", "{what}"]
# command = ["notify-send", "wf: {title}", "{what}"]                                    # Linux
```

The command runs without a shell. In its arguments, `{kind}`, `{item}`, `{title}` and `{what}` are
replaced as `wf inbox` shows them; `{kind}` is one of `blocker`, `proposal`, `principal_review`,
`merge`, `recovery` and `permission`. What it already told you of is not told again after a
restart. A command that fails is logged in the daemon's log.

## Colours

`[theme]` sets the colours of the CLI and the dashboard on this machine. Every key is optional;
what you leave out keeps the built-in style. Values use git's color syntax, as in `.gitconfig`: a
foreground, then a background, then attributes, such as `"bold red"`, `"#1e50a0"`,
`"brightblue dim"`, `"black yellow"` or `"normal"`. A colour is a name (`red`, `brightred`, …), a
number of the terminal's palette (`0` to `255`) or `#rrggbb`. The names follow your terminal's
palette; `#rrggbb` does not.

```toml
[theme]
background = "#1b1d23"           # "none", the default, keeps the terminal's own
text = "#d0d0d0"
text_dimmed = "#6c7080"          # a colour in place of the dim attribute
divider = "#3a3d46"
accent = "#1e50a0"
state.review = "yellow"
priority.p1 = "bold yellow"
```

| Key | Built in | What it colours |
|---|---|---|
| `background` | `"none"` | behind every cell of the dashboard, popups included; one colour |
| `text` | the terminal's | text with no other style, in the dashboard |
| `text_dimmed` | `dim` | secondary text: types, counts, hints, timestamps |
| `text_bold` | `bold` | names, titles, headings, markdown bold |
| `divider` | `dim` | rules, pane separators, the frames of popups |
| `link` | `ul` | what the pointer is over, in the dashboard |
| `accent` | `#1e50a0` | the selection that has the keys; one colour |
| `accent_dimmed` | `#1e2d4b` | a selection without the keys, or in an unfocused pane; one colour |
| `on_accent` | `brightwhite` | text on the full accent |
| `tab_active` | `bold reverse` | the label of the tab on screen |
| `tab_active_dimmed` | `bold reverse dim` | the label of the tab on screen, in an unfocused pane |
| `key` | `yellow` | keys to press, in hints |
| `id` | `cyan` | item ids |
| `principal`, `agent`, `daemon` | `magenta`, `brightblue`, `green` | who did something; in transcripts, the headers of answers (`agent`) and of prompts from wf or the brief (`daemon`) |
| `success`, `error` | `green`, `red` | what went through or failed; in transcripts, ✓ and ✗ |
| `warning` | `bold red` | what went wrong and needs you |
| `attention` | `yellow` | what waits on you |
| `live` | `blue` | live sessions |
| `code` | `blue` | inline code |
| `diff_added`, `diff_removed` | `green`, `red` | lines of diffs in transcripts |
| `state.draft` … `state.cancelled` | `dim`, `green`, `blue`, `yellow`, `magenta`, `brightblack` | items in each state |
| `readiness.blocked`, `readiness.stranded` | `red`, `bold red` | ready and claimed items look like `state.open` and `state.active` |
| `priority.p0` … `priority.p3` | `bold red`, `yellow`, `normal`, `dim` | priorities |

`background` and `text` apply to the dashboard only: printed output stays in the terminal's own
colours, where a background would break as soon as it scrolls. Without a terminal, wf prints no
colours at all, whatever the theme. `wf item diff` uses git's own colours.

`wf` reads `[theme]` when it starts, so a change shows in the next command or dashboard. Unlike the
rest of the file, a mistake in it does not stop `wf`: it warns, naming the key, and uses the
built-in colours. Likewise, a mistake elsewhere in the file leaves the colours alone.

## Permission requests

`[permissions]` sets who decides what an agent asks before you are asked. That has its own guide:
[permissions.md](permissions.md).
