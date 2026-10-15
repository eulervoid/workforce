# workforce (`wf`)

wf coordinates coding agents in a git repository. You write down the work as **items**. A daemon on
your machine starts **agents** (Claude Code, opencode, Codex, Gemini CLI) on them, each in its own git
worktree and branch. You answer their questions, review what they submit, and approved work is merged
into your main branch.

Everything that happens is an event in a signed log that every machine of yours keeps a copy of and
syncs peer to peer. There is no server and no account: wf runs from your terminal.

> [!WARNING]
> wf is in early, rapid development. Expect breaking changes between versions, including to the
> event log, the policy and the machine config.

<p align="center"><img src="docs/img/lifecycle.svg" alt="An item goes open → active → review → done: an agent starts on it in its own worktree, submits its commits, and the work is merged into main once approved; a review can send it back to open, and the agent asks the principal when it has a question" width="100%"></p>

## Install

You need Rust (stable), git 2.38 or later, [Fence](https://github.com/fencesandbox/fence) for the
sandbox agents run in, and at least one harness, logged in, with its ACP (Agent Client Protocol)
adapter on your `PATH`:

| Harness | wf runs |
|---|---|
| Claude Code | `claude-agent-acp` |
| opencode | `opencode acp` |
| Codex | `codex-acp` |
| Gemini CLI | `gemini --experimental-acp` |

Then build `wf`:

```sh
git clone <this repository> workforce && cd workforce
cargo install --path crates/wf-cli          # installs `wf` into ~/.cargo/bin
```

## First run

In the repository the agents should work on:

```sh
cd ~/src/app
wf init --name app
```
```
created workforce "app" (vyxpmlnxamsk37bpvfy7x6ba52ck3vpkhwjn3dhtkarek5zkr7ka)
you are @ada
found harnesses; add an agent for the one wf should use:
  wf agent add --harness claude-code
  wf agent add --harness opencode
```

You are `@ada`: the **principal**, the person the agents work for (your real handle will be 
`@$USER`, `--handle` picks another name). `wf init` writes `.workforce.toml`, which holds only 
the id of the workforce; commit it, so that clones on your other machines find it. `wf init -i` 
asks instead: the name, which harnesses to define agents for, and for each its name, permissions 
and model.

Define an agent, and let it start on its own:

```sh
wf agent add --harness claude-code --start auto
```
```
defined agent cecilia; this device runs it, `wf device edit --agent` changes that
```

Agents you do not name get the first name of a scientist. Start the daemon, which starts agents on
ready items and merges approved work:

```sh
wf daemon start
```
```
the daemon runs (pid 48213); it logs to /Users/ada/.local/share/workforce/daemon.log
```

## A first item, end to end

```sh
wf item add --title "Parse durations in policy" --priority P1 --body "$(cat <<'EOF'
Accept `2h`, `30m` and `1h30m` wherever the policy takes a duration.

- [ ] Parse durations with humantime
- [ ] Error messages name the field
- [ ] Tests for both
EOF
)"
```
```
created #k3f7ab
 ITEM (TYPE:STATE)      PRIORITY  TITLE                      STEPS  BLOCKING
 ○ #k3f7ab (task:open)  P1        Parse durations in policy  0/3
```

The `- [ ]` lines are **steps**: the agent ticks them off and reports on each. Within seconds the daemon
starts `cecilia` on the item. Open the dashboard to follow it:

```sh
wf
```

Its first tab shows what waits on you and what each agent is doing right now; `enter` on a session
opens its live transcript. If the agent asks a question, it shows under *waiting on you*: press `r` to
answer. When the agent is done, it submits:

```
14:02  #k3f7ab  @ada/cecilia/s4k2ab · submitted 3e8a1c4f → review
```

Tasks are reviewed by you unless the item says otherwise, so the review waits in the dashboard and in
`wf inbox`:

```
 WAITING           ITEM     TITLE                      WHAT
 principal_review  #k3f7ab  Parse durations in policy  review of 3e8a1c4f: `wf item diff`, then `wf review`
```

Read what the agent reported and look at the work:

```sh
wf item show k3f7                # the item with its comments and history
wf item diff k3f7                # the diff, in your pager
```

Approve it, or write change requests and send it back:

```sh
wf review k3f7 --approve
```
```
approved #k3f7ab
```

```sh
wf comment k3f7 "Name the field in the error." --request-change
wf review k3f7 --request-changes # the item goes back to the agent, on the same branch
```

After the approval the daemon merges the work into `main` as `Merge #k3f7ab: Parse durations in policy`,
removes the item's worktree and branch, and the item is done. If your checkout of `main` has
uncommitted changes, the merge waits until you commit or stash them.

## Concepts

**Principal.** The person accountable for the work, and the one agents act for: you. Your principal key
signs what only you may do: the policy, invites, new machines.

**Agent.** A worker you define once: a harness, a model, capabilities, lanes and how much it may do on
its own (`--start`, `--permissions`). Addressed as `@ada/cecilia`.

**Session.** One run of one agent on one item, on one machine, in the item's worktree. It ends when the
agent submits or releases the item, or runs out of time.

**Device.** A machine bound to the workforce. Each says how many sessions it runs, and of which agents.

**Item.** A piece of work (`task`), a proposal to decide (`decision`), or meta work (`distill`). Every
item is in one of six states: `draft` (held back, nothing starts it), `open`, `active` (a session works
on it), `review`, `done` or `cancelled`. Agents move items into `active` and `review`; you move them
with `wf item open`, `draft`, `done` and `cancel`.

**Steps.** The `- [ ]` lines of an item's body: a checklist for the session.

**Comments.** All text on an item: discussion, reports, reasons. A **blocker** is a comment that says the
work cannot go on until someone answers; the session waits meanwhile. A **change request** is a comment
that says what the work must do before it is submitted or approved. Both stay open until a later comment
resolves them.

**Review levels.** Who approves submitted work: nobody (`none`), an agent with given capabilities
(`light`, `standard`), an agent and then you (`critical`), or you alone (`manual`, the default for tasks).

**Recovery levels.** What happens when a session ends without finishing: try again, have an agent
diagnose it, or ask you.

**Lanes and capabilities.** An item is in a lane, and an agent takes items from its lanes. An item can
require capabilities (`rust`, `gpu`) that the agent or the machine must have. The `supervision` lane
holds no items: its agents answer other agents' questions.

**Policy.** How the workforce works: the item types, lanes, review and recovery levels, and the branch
work is merged into. It is part of the log, signed by you; `wf policy edit` changes it.

**Machine config.** What only this machine does: the sandbox, the agents' environment, notifications,
commit signing and who decides permission requests. `~/.config/workforce/config.toml`; never synced.

## Everyday use

```sh
wf                               # the dashboard: keep it open next to your editor; `?` lists the keys
wf inbox                         # what waits on you: blockers, reviews, merges, permission requests
wf status                        # a summary, and what to do next
wf log --follow                  # the events as they arrive; --item for one item
wf item list --ready             # what can be worked on now
wf item add --draft --title "…"  # held back until `wf item open`
wf comment k3f7 "Use PolicyError." --resolve q4tz    # answer a blocker
wf session allow s4k2            # or deny: answer a permission request
wf session watch k3f7            # the transcript of the item's latest session
wf session start k3f7 --agent cecilia   # start an agent by hand
```

Agents run in a sandbox and are allowed what they ask inside it by default. An agent set to
`--permissions ask` holds each request for the deciders of your machine config and then for you.

## Configuring

* Agents, models and autonomy (`wf agent add/edit`, `wf harness models`), and how many sessions run on
  which machine (`wf device edit --max-sessions 3 --agent coder=2`): [agents.md](docs/guide/agents.md)
* The sandbox, the agents' environment, a shared build cache, the storage budget, checks, commit signing,
  notifications and colours:
  [machine-config.md](docs/guide/machine-config.md)
* Permission requests: allow, ask or deny, and the deciders (rules, a model, your own hook, an agent):
  [permissions.md](docs/guide/permissions.md)
* Item types, lanes, review and recovery levels, the merge target: [policy.md](docs/guide/policy.md)

## Several machines

Add your other machines, a desktop or a VM, so they sync the log and run agents too:

```sh
wf invite device                 # on a machine with your principal key; prints a wfi1... code
wf join wfi1... --name vm        # on the other machine, inside a clone of the repository
wf daemon start                  # on both: they find each other and sync
```

See [machines.md](docs/guide/machines.md) for syncing without a daemon, running the daemon as a
service, where keys are kept, forking away a lost machine, and backups.

## Where to go next

| Guide | What it covers |
|---|---|
| [items.md](docs/guide/items.md) | items, states, steps, dependencies, comments, blockers, change requests, archiving |
| [agents.md](docs/guide/agents.md) | harnesses, defining agents, reviewers and supervisors, where agents run, sessions |
| [review-and-merge.md](docs/guide/review-and-merge.md) | review levels, your review, the merge, recovery |
| [dashboard.md](docs/guide/dashboard.md) | the dashboard and its keys, `wf inbox`, `wf status`, `wf log`, watching sessions |
| [machine-config.md](docs/guide/machine-config.md) | sandbox, environment, storage, checks, signing, notifications, colours |
| [permissions.md](docs/guide/permissions.md) | permission requests and their deciders |
| [policy.md](docs/guide/policy.md) | the policy and its defaults |
| [machines.md](docs/guide/machines.md) | several machines, the daemon, keys, fork, backups |

Every command explains itself: `wf --help`, `wf <command> --help`. Commands that take an item or a
session offer a picker at a terminal when you leave it out, and `--json` gives machine-readable output.

How wf works inside, and where it is going, is in the design: [docs/SPEC.md](docs/SPEC.md).

## Working on wf

`just check` runs formatting, clippy and all tests. Property tests default to 128 cases; for the
10k-case run use `PROPTEST_CASES=10000 cargo test --release -p wf-core`. The 100k-event startup check
is `cargo test --release -p wf-store -- --ignored --nocapture`, and the 100k-event join over loopback
is `cargo test --release -p wf-net --test scale -- --ignored --nocapture`. The sync simulation runs
1,000 schedules (`cargo test -p wf-net --test simulation`, about a minute in debug builds).

## License

wf is licensed under the GNU General Public License, version 3 only (GPL-3.0-only). See
[LICENSE](LICENSE).
