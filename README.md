# workforce (`wf`)

A peer-to-peer, CLI-first coordination layer for coding agents and the humans who work with them.
Event-sourced, forkable, built on iroh. M0 (signed event log, item model, CLI), M1 (sync between
your machines, device invites, daemon, fork) and M2 (agents at work on one machine: sessions, review
and merge) are implemented; sessions across machines come next.

- [docs/SPEC.md](docs/SPEC.md): the design
- [docs/M0.md](docs/M0.md): plan for the first milestone
- [docs/M1.md](docs/M1.md): sync, device invites, daemon and fork, with what was built
- [docs/M2.md](docs/M2.md): agents at work on one machine, with what was built

## Try it
```sh
cargo build
export PATH="$PWD/target/debug:$PATH"
cd ~/src/some-repo
wf init --name demo
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
wf daemon &                      # starts the agents on what is ready, and merges what is approved
wf session list                  # live and finished sessions
wf session watch <item>          # follow the transcript of the latest session on an item
wf inbox                         # what waits on you: questions, reviews, merges, permission requests
wf session allow <session>       # or `deny`, for a permission request an agent is waiting on
wf review <item> --approve       # your step of a review level that has one
wf item diff <item>              # what the work changes, as a review sees it
```
A session runs one agent on one item in its own git worktree and branch (`wf/<item>`), started by the
daemon over ACP (`opencode acp`, `claude-agent-acp`, `codex-acp`, `gemini --experimental-acp`). Inside a
session the agent works with `wf work report`, `wf work ask`, `wf work submit` and `wf work release`.
Submitted work is reviewed at the level the item names, then merged into the target branch of the
`[merge]` policy. Agents start with `--start manual` and `--permissions ask`; `wf session start <item>
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

## Several machines
```sh
# laptop, where `wf init` ran (it holds your principal key)
wf daemon &                      # serves sync and join, follows gossip
wf invite device                 # prints wfi1...
# vm, inside a clone of the repository
wf join wfi1... --name vm        # binds this machine to you and takes in the log
wf daemon &
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
relay). Run the daemon as a service yourself, for example as a systemd user unit or a launchd agent
running `wf daemon`.

`just check` runs formatting, clippy and all tests. Property tests default to 128 cases; for the
10k-case run use `PROPTEST_CASES=10000 cargo test --release -p wf-core`. The 100k-event startup check
is `cargo test --release -p wf-store -- --ignored --nocapture`, and the 100k-event join over loopback
is `cargo test --release -p wf-net --test scale -- --ignored --nocapture`. The sync simulation runs 1,000
schedules (`cargo test -p wf-net --test simulation`, about a minute in debug builds).
