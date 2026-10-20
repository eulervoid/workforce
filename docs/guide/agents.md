# Agents

An agent is a worker you define once: a harness (the program that runs the model), a model, what it
can do, which items it takes, and how much it may do on its own. It is yours: `@ada/grace` is the agent
`grace` of the principal `@ada`. A **session** is one run of an agent on one item.

## Harnesses

wf talks to coding agents over ACP, the Agent Client Protocol. Install the harness you want to use and
its ACP adapter, and log in to it as you normally would:

| Harness | wf runs |
|---|---|
| `claude-code` | `claude-agent-acp` |
| `opencode` | `opencode acp` |
| `codex` | `codex-acp` |
| `gemini-cli` | `gemini --experimental-acp` |

`wf init` lists the ones it finds on your `PATH`. `wf harness models <harness>` starts one and lists
the models it offers.

## Defining agents

```sh
wf agent add --harness claude-code
```
```
defined agent grace
```

Without a name, wf picks the first name of a scientist that no agent in the workforce has yet. Or name
it by role: `wf agent add coder --harness opencode`. The name cannot change later; everything else can,
with `wf agent edit`.

| Flag | Says |
|---|---|
| `--harness` | `claude-code`, `opencode`, `codex` or `gemini-cli` |
| `--model` | the model, as `wf harness models` lists it; left out, the harness's own choice |
| `--capability` | what it can do, matched against what items and review steps require, such as `rust`, `review` or `strong-model`; repeat for several |
| `--lane` | the lanes it takes items from; `default` when left out. `supervision` alone makes a supervisor |
| `--instructions` | what it is told in every brief, such as how strictly to review |

Its autonomy:

| Flag | Default | Says |
|---|---|---|
| `--start` | `manual` | `auto` lets the daemon start it on ready items; `manual` runs it only through `wf session start` |
| `--permissions` | `allow` | `allow`, `ask` or `deny` what its sessions ask permission for; see [permissions.md](permissions.md) |
| `--nudges` | `3` | how many turns in a row without a commit before its session is released |
| `--park-timeout` | `2h` | how long a session may wait for an answer to a blocker |
| `--max-duration` | none | how long a session may run, such as `1h` |

```sh
wf agent list                    # your agents and those of other members
wf agent show grace              # its definition, the devices that run it, its sessions and activity
wf agent edit grace --model opus --start auto
wf agent remove grace            # its sessions stay in the log; `wf agent add` brings it back
```

## A small team

A coder that works on tasks, a reviewer for the `light` and `standard` review levels, and a supervisor
that answers the coder's questions where it can:

```sh
wf agent add coder --harness opencode --capability rust --start auto
wf agent add reviewer --harness claude-code --capability review --capability strong-model --start auto
wf agent add supervisor --harness claude-code --lane supervision --start auto
wf device edit --max-sessions 3 --agent coder=1 --agent reviewer=1 --agent supervisor=1
wf item add --title "Parse durations in policy" --review standard
```

* **Workers** take items from their lanes whose `requires` their capabilities (and the machine's
  hardware) meet.
* **Reviewers** are agents whose capabilities meet a review step's `requires`. A reviewer never
  reviews an item it worked on. When a review level has steps that run at once, the daemon starts
  a session of a different agent for each, told the step's name and instructions in its brief.
* **Supervisors** are agents in the `supervision` lane. When an agent asks a question with a blocker
  that mentions no one, a supervisor that never worked on the item gets it, with the item's history.
  It answers, or escalates the question to you. Without a supervisor, questions come to you.

When the daemon has a free slot for an agent with `--start auto`, it starts, in this order: a review
the agent can take, a blocker to supervise, then the first ready item in its lanes.

## Where agents run

Each machine says how many sessions it runs at once, and of which agents:

```sh
wf device edit --max-sessions 3                  # at most 3 sessions on this machine
wf device edit --agent coder=2 --agent reviewer=1   # replaces the list: these agents, this many each
wf device edit --hardware "gpu, os:linux"        # items that require `gpu` can run here
```

`wf agent add --start auto` and `wf agent edit --start auto` add the agent to this machine with one
session, unless the machine lists it already. A machine runs one session at a time unless you raise
`--max-sessions`; parked sessions count. `wf status` flags an agent that starts on its own
but that no machine runs.

## Running a session by hand

```sh
wf session start k3f7 --agent grace          # needs the daemon
wf session start k3f7 --watch                # and follow the transcript
wf session start k3f7 --agent ada --step security   # review the step `security` of an item in review
```

```
started session s4k2ab of grace on #k3f7ab
```

Without `--agent`, the policy's `default_agent` is used (see [policy.md](policy.md#default_agent)).
`--watch` follows the transcript; `Ctrl-C` stops watching and leaves the session running. Watching
takes the `--full` and `--raw` of `wf session watch`.

```sh
wf session stop s4k2             # stop it; the item goes back to open
```

## What happens in a session

The daemon creates a repository of the agent's own for the item and starts the agent there, on the
branch `wf/<item>` at the tip of the target. It lives outside your checkout, under
`~/.local/share/workforce/<workforce id>/repos/`, and reads the objects of wf's own repository
(`~/.local/share/workforce/<workforce id>/git`, see [review-and-merge.md](review-and-merge.md#the-merge))
without being able to change it at all; `wf item path <item>` prints it. The daemon copies the agent's
commits into wf's repository, on the branch `wf/<item>`, at the end of every turn and when it submits.
`wf item checkout <item>` fetches them into your checkout.
Every session on the item continues the same repository, so the next one picks up where the last one
stopped. `wf work submit` rebases the work onto the target first; when that stops at a conflict, the
agent resolves the files and submits again.

The agent gets a **brief**: the item, its steps, open change requests, what was said before, and how to
report. It works, commits, and reports with `wf comment`, ticking steps as it goes. When the policy has
checks, the brief tells it to build and run the tests near its change and leave the whole suite to
its submission, which runs the checks in its sandbox; a reviewer is told they passed. It asks a question
with a blocker (`wf comment <item> "..." --block`) and parks until it is answered. It ends with:

* `wf work submit --note "..."`: the work is done. With checks in the policy the daemon runs them
  first, while the session waits: what fails is the agent's next prompt, and when they pass the work
  goes to review, or is merged right away if the item's review level has no steps.
* `wf work release --note "..."`: it gives up; the item goes back to open, and its recovery level
  says what happens next.

A session also ends when it runs out of nudges or time. Agents commit as themselves (`@ada/grace`)
and unsigned; see [machine-config.md](machine-config.md#commits-and-signatures).

If the daemon stops, its sessions do not end: the next daemon resumes them in their agents'
conversations, where the harness supports that.
