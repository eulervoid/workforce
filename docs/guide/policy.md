# The policy

The policy says how the workforce works: the item types and their states, the lanes, the review and
recovery levels, and where approved work is merged. It is part of the log, signed with your principal
key, so every machine follows the same policy. It is not a file in the repository.

```sh
wf policy show                   # the policy in effect
wf policy edit                   # edit it in $VISUAL or $EDITOR, then apply it
wf policy apply policy.toml      # replace it with a file
```

Changing it needs the principal key of an owner. What only one machine should do, such as running
programs outside the agents' sandbox or calling paid APIs, goes in the
[machine config](machine-config.md) instead.

## The default policy

`wf init` starts with this policy. Each part is explained below.

```toml
[lanes.default]

[lanes.meta]

# Holds no items: its agents answer and escalate blockers.
[lanes.supervision]

[merge]
mode = "local"
target = "main"

[[review]]
name = "none"
steps = []

[[review]]
name = "light"
steps = [{ requires = ["review"] }]

[[review]]
name = "standard"
steps = [{ requires = ["review", "strong-model"] }]

[[review]]
name = "critical"
steps = [{ requires = ["review", "strong-model"] }, "principal"]

[[review]]
name = "manual"
steps = ["principal"]

# What happens when a session ends without finishing: one step per unsuccessful attempt.
[[recovery]]
name = "none"
steps = ["principal"]

[[recovery]]
name = "standard"
steps = ["retry", "retry", "principal"]

# A reviewer says what went wrong before the one retry.
[[recovery]]
name = "diagnose"
steps = [{ agent = { requires = ["review"] } }, "retry", "principal"]

[types.task]
review = "manual"
recovery = "standard"
lane = "default"
fields = ["priority", "labels", "requires"]
depends_on = ["task", "decision"]
initial = "open"
states = ["draft", "open", "active", "review", "done", "cancelled"]

[types.decision]
lane = "default"
fields = ["labels"]
depends_on = []
initial = "review"
states = ["review", "done", "cancelled"]

[types.distill]
lane = "meta"
recovery = "standard"
fields = ["labels"]
depends_on = []
initial = "open"
states = ["open", "active", "done", "cancelled"]
```

## `default_agent`

The agent `wf session start` uses when you give no `--agent`; the default policy has none. Add it at
the top, before any `[table]`, with `wf policy edit`:

```toml
default_agent = "ada"
```

`wf session start <item> -a <name>` overrides it.

## Types

Every item has a type. A type says:

* `lane`: the lane its items go to, unless an item names another;
* `fields`: which of `priority`, `labels` and `requires` its items may have;
* `depends_on`: the types its items may depend on;
* `states`: which of the six states `draft`, `open`, `active`, `review`, `done` and `cancelled` it
  uses, and `initial`, the one items start in (`wf item add --draft` starts one as a draft instead);
* `review` and `recovery`: the levels its items get when they name none.

The built-in types are `task` (work an agent does), `decision` (a proposal you accept with
`wf item done` or reject with `wf item cancel`), and `distill` (meta work in its own lane). You can add
types of your own; the built-in ones cannot be removed.

## Lanes

A lane is a queue that agents take items from. An item is in one lane, its type's or its own
(`--lane`); an agent takes items from the lanes in its definition (`wf agent add --lane`). The
`supervision` lane holds no items: agents in it answer the blockers of other agents, or escalate them
to you.

## Review levels

A review level is a list of entries the submitted work goes through, in order. An entry is a step, or
a list of steps that run at once and must all approve before the next entry. A step is an agent that
has the capabilities it `requires`, or `"principal"`: you. A level without entries is not reviewed:
the work is merged as soon as the session submits. Levels are listed from least to most strict; an agent
may raise the level of the item it works on, never lower it.

| Level | Steps |
|---|---|
| `none` | merged as submitted |
| `light` | an agent with the capability `review` |
| `standard` | an agent with `review` and `strong-model` |
| `critical` | an agent with `review` and `strong-model`, then you |
| `manual` | you alone; the default for tasks |

### Reviews at once

For important work you may want several reviews at once, each with its own instructions, such as one
on style and one on security. An agent step can have a `name` and `instructions`; the agents that take
them are the regular agents whose capabilities match, and an agent never takes two steps of one entry.

```toml
[[review]]
name = "critical"
steps = [
  [
    { name = "style", requires = ["review"], instructions = """
Naming, idiom and readability against AGENTS.md.
""" },
    { name = "security", requires = ["review", "strong-model"], instructions = """
Injection, sandbox escapes, secrets in logs, unchecked input from peers.
""" },
    { requires = ["review", "strong-model"] },
  ],
  "principal",
]
```

The instructions go into the brief of the reviewer beside the instructions of its agent. They live in
the policy, which is signed, and never in a file of the repository, which the reviewed work could
change. Steps of one entry with the same name, or both without one, have to be identical. Any
reviewer requesting changes sends the work back; your approval satisfies the whole entry.

The daemon starts one session for each step of the entry, so this level needs three agents that can
review; the others finish their reviews after one asked for changes, and their approvals count for
nothing. `wf item show` says `awaiting security, style; general approved`, and `wf status` names the
steps no agent can take. See [steps at once](review-and-merge.md#steps-at-once).

See [review-and-merge.md](review-and-merge.md) for how a review goes.

## Recovery levels

A recovery level says what happens when a session ends without finishing (it released the item, ran
out of time or nudges, or failed): one step per unsuccessful attempt, and after the last one the item
waits for you.

| Step | Does |
|---|---|
| `"retry"` | the item may be started again, by any agent that can take it |
| `{ agent = { requires = [...] } }` | an agent with these capabilities says what went wrong, in a diagnose session, before the next step |
| `"principal"` | the item waits for you in `wf inbox` |

`standard`, the default for tasks, tries twice more and then asks you.

## Merge

`[merge] target` is the branch approved work is merged into, `main` by default. `mode = "local"`
merges in this repository; see [review-and-merge.md](review-and-merge.md#the-merge).

### Checks

`[[merge.check]]` names a command that has to pass on the merge result before the target moves. Each
has a `name`, unique in the policy, a `run` (the program and its arguments, without a shell) and a
`timeout`, 30 minutes by default. Without checks, work merges as soon as it is approved. These are the
checks of wf's own repository:

```toml
[[merge.check]]
name = "fmt"
run = ["cargo", "fmt", "--all", "--check"]

[[merge.check]]
name = "clippy"
run = ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]

[[merge.check]]
name = "test"
run = ["cargo", "test", "--workspace"]
timeout = "20m"

[[merge.check]]
name = "examples"
run = ["cargo", "build", "--workspace", "--examples"]
```

A check gets the agents' environment and no secrets, and runs in the agents' sandbox at submission and
in the sandbox the machine config gives checks at the merge, by default the agents' (see
[machine-config.md](machine-config.md#checks)). Being in the
policy, it is out of reach of the work it checks: a branch cannot turn its own checks off. `wf check`
runs them on your checkout. The daemon runs them when an agent submits, in the agents' sandbox, so
the agent hears what fails before anyone reviews, and again before the merge; what happens when one
fails is in [review-and-merge.md](review-and-merge.md#checks).
