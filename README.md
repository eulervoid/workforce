# workforce (`wf`)

A peer-to-peer, CLI-first coordination layer for coding agents and the humans who work with them.
Event-sourced, forkable, built on iroh. M0 (signed event log, item model, CLI), M1 (sync between
your machines, device invites, daemon, fork) and M2 (agents at work on one machine: sessions, review
and merge) are implemented; sessions across machines come next.

- [docs/SPEC.md](docs/SPEC.md): the design
- [docs/M0.md](docs/M0.md): plan for the first milestone
- [docs/M1.md](docs/M1.md): sync, device invites, daemon and fork, with what was built
- [docs/M2.md](docs/M2.md): agents at work on one machine, with what was built
- [docs/M2.2.md](docs/M2.2.md): daily use on one machine (in progress)

## Try it
```sh
cargo build
export PATH="$PWD/target/debug:$PATH"
cd ~/src/some-repo
wf init --name demo              # or `wf init -i` to pick the harnesses, permissions and models
wf item add --title "Implement feed sync" --priority P1
wf item list --ready
wf log
```
## Agents at work
```sh
wf agent add coder --harness opencode --capability rust --lane default --start auto
wf agent add reviewer --harness claude-code --capability review --capability strong-model --start auto
wf agent add supervisor --harness claude-code --lane supervision --start auto
wf device edit --max-sessions 3 --agent coder=1 --agent reviewer=1 --agent supervisor=1
wf item add --title "Parse durations in policy" --review standard
wf daemon start                  # starts the agents on what is ready, and merges what is approved
wf session list                  # live and finished sessions
wf session watch <item>          # follow the transcript of the latest session on an item
wf watch                         # events as they arrive; answer what waits on you right there
wf inbox                         # what waits on you: blockers, reviews, merges, permission requests
wf session allow <session>       # or `deny`, for a permission request an agent is waiting on
wf review <item> --approve       # your step of a review level that has one
wf item diff <item> [--stat]     # the work, through git: your pager and delta apply
wf item checkout <item>          # the main checkout on the work, detached; `checkout -` goes back
cd $(wf item path <item>)        # the item's worktree
```
A session runs one agent on one item in its own git worktree and branch (`wf/<item>`), started by the
daemon over ACP (`opencode acp`, `claude-agent-acp`, `codex-acp`, `gemini --experimental-acp`). Inside a
session the agent comments with `wf comment <item>` (`--block` when it needs an answer, which a
supervisor or you give with `--resolve`), and ends with `wf work submit` or `wf work release`.
Submitted work is reviewed at the level the item names, then merged into the target branch of the
`[merge]` policy. Agents start with `--start manual` and `--permissions allow`, which lets them do
what they ask inside the sandbox and asks you on a machine without one; `wf session start <item>
--agent <name>` runs one by hand. The harnesses named above have not all been run against this build
yet: see *As built* in [docs/M2.md](docs/M2.md).

Set `default_agent = "coder"` at the top of the policy (before any `[table]`) to omit `--agent`.
Use `-a <name>` to override it. Missing item/session arguments open a picker at a terminal;
stop offers local live sessions, permission commands offer pending requests, and watch offers
sessions with a local transcript. Add `--watch` to `session start` to follow immediately: Ctrl-C
detaches without stopping the agent, and the watcher exits after the session ends.

Watch renders basic markdown, distinguishes thoughts with dim styling, and shows tool results,
plans, and unified diffs supplied by the agent. Tool output and diffs are capped at 20 lines per
block; use `--full` for everything. Use `--raw` for unrendered markdown with plain, color-free
layout. Both options also work with `session start --watch`. Markdown is buffered until a
paragraph or event boundary; raw mode streams text chunks immediately.

## Sandbox
The daemon runs every agent in [Fence](https://github.com/fencesandbox/fence), which has to be
installed: an agent may write only in its worktree and where it needs to commit and talk to the
daemon, and reach model APIs and package registries. Inside it, agents are allowed what they ask by
default. The machine config can add to what Fence allows, or turn the sandbox off; then agents run
with your privileges, and those set to `allow` are asked instead.
```toml
[sandbox]
kind = "fence"                   # or "off"
allow_write = ["/opt/cache"]
allow_domains = ["*.example.com"]
```

## Signed commits
Agents commit without signing, so that they never wait for a key you have to unlock or approve. When
your git config signs commits (`commit.gpgsign`), the merge commit that brings approved work into the
target is signed, as you: the signature says you accepted the work. The machine config changes either:
```toml
[commits]
agent_signing = "unsigned"       # default; "as_configured" signs as your git config says
merge_signing = "as_configured"  # default; "unsigned" never signs the merge commit
```

## Permission requests
An agent with `--permissions ask`, or any agent on a machine without a sandbox, asks before its
harness runs a tool. The request goes through the
deciders of this machine's config, `~/.config/workforce/config.toml` (or `$XDG_CONFIG_HOME`), in
their order; what all of them pass on waits for you in `wf watch` and `wf inbox`. Without a config,
the built-in rules decide alone: a session may run its own `wf` commands.
```toml
[permissions]
guidance = "Allow reading anything and writing in the worktree. Ask about the network."

[[permissions.decider]]
kind = "rules"                   # built in: a session's own `wf` commands

[[permissions.decider]]
kind = "model"                   # a decision model over the System One API, such as Jev
url = "https://openrouter.ai/api/v1/systemone"
model = "typesafe/jev-1.13"
api_key_env = "OPENROUTER_API_KEY"
question = { noul = { allow_above = 0.95, deny_below = 0.05 } }   # or { choice = { min_confidence = 0.8 } }

[[permissions.decider]]
kind = "hook"                    # any program: the request as JSON on stdin,
command = ["my-judge"]           # `{"permission": "allow" | "deny" | "ask", "reason": ...}` on stdout

[[permissions.decider]]
kind = "agent"                   # an agent of the workforce, in a harness session the daemon keeps
agent = "judge"
```
`wf permissions test --command "cargo test"` runs the chain without a session; the transcript of a
session says who decided each request and why.

## Several machines
```sh
# laptop, where `wf init` ran (it holds your principal key)
wf daemon start                  # serves sync and join, follows gossip
wf invite device                 # prints wfi1...
# vm, inside a clone of the repository
wf join wfi1... --name vm        # binds this machine to you and takes in the log
wf daemon start
wf item add --title "Try sync" --priority P1
# anywhere
wf sync                          # one shot, without a daemon: `wf sync --serve` listens meanwhile
wf device list                   # name, last seen, flags
wf members
wf fork --exclude vm             # the vm was lost: a new workforce without it
wf fork accept <id>              # on each remaining device: follow the fork
```
The principal key of a workforce is stored next to its store, in
`~/.local/share/workforce/<workforce id>/principal.key`; every workforce has its own. If you created a
workforce before this was the case, move `~/.local/share/workforce/keys/principal.key` there.

Without a daemon the CLI opens the store itself; with one it asks the daemon, which owns the store.
Devices find each other through iroh (relays and address lookup from n0; `--relay <url>` replaces the
relay). `wf daemon start` runs the daemon in the background and logs to
`~/.local/share/workforce/daemon.log` (`WF_LOG` takes a `tracing` filter); `wf daemon stop` and
`wf daemon status` do what they say. To run it as a service, have a systemd user unit or a launchd
agent run `wf daemon run`, which stays in the foreground and stops on SIGTERM. Agents call the copy of
`wf` the daemon made when it started, so rebuilding `wf` does not change what running agents call;
restart the daemon to give them the new one. Stopping the daemon does not end its sessions: the next
daemon resumes them in their agents' conversations, where the agent supports that.

`wf export > backup.jsonl` writes the log as JSON lines, one event per line, readable with `jq`
(`content` is the event, `event` its signed encoding). `wf import backup.jsonl` adds those events to
the store of their workforce on this machine, or creates it: for a `store.redb` that no longer
decodes, move it away and import. The keys next to the store are not part of an export.

`just check` runs formatting, clippy and all tests. Property tests default to 128 cases; for the
10k-case run use `PROPTEST_CASES=10000 cargo test --release -p wf-core`. The 100k-event startup check
is `cargo test --release -p wf-store -- --ignored --nocapture`, and the 100k-event join over loopback
is `cargo test --release -p wf-net --test scale -- --ignored --nocapture`. The sync simulation runs 1,000
schedules (`cargo test -p wf-net --test simulation`, about a minute in debug builds).
