# Proposal: fixed item states

Status: built (#y5whs6). SPEC §8, §15 and §17 describe it as built; this stays for the reasoning.

## Why
Each item type names its own states and maps each to one of six fixed categories (SPEC §8). Built-in
behaviour keys on the categories, so the names only show up in the CLI, and there they cost:

* No state can have a flag or a verb of its own. Holding an item back as a draft is
  `wf item add` and then `wf item move <id> backlog`, which takes two events. The dispatcher runs on
  every change to the log and can start an agent between them.
* `--state` and `wf item move` take any string; nothing can be completed or listed in `--help`.
* Help text has to hedge: "the cancelled state of its type, such as `dropped`", "every live item in a
  state of that category, whatever its type calls the state".
* Two concepts for one thing: `StateName` and `StateCategory`, kept apart by `type_config`,
  `first_state(category)` and `state_category_of` lookups at 44 places, and `Standing::Unknown` for an
  item whose state a policy change removed.

The flexibility is barely used. The states of the built-in types (`task`, `decision`, `distill`) are
fixed in v1 already (`PolicyError::BuiltinStatesChanged`), and no policy in use defines a custom type.

## Proposal
**The six categories become the states.** One enum, `ItemState`, replaces `StateName` and
`StateCategory`:

| State | Was (category) | Meaning, as now |
|---|---|---|
| `draft` | `inactive`; `backlog` | exists, not actionable: nothing starts it |
| `open` | `open` | actionable; ready once its dependencies are done |
| `active` | `active`; `in_progress` | a session works on it |
| `review` | `review`; `in_review`, `proposed` | submitted, waits for its review steps |
| `done` | `done`; `ratified`, `superseded` | finished; satisfies dependents |
| `cancelled` | `cancelled`; `dropped`, `rejected` | finished without success; strands dependents |

**Types stay configurable, except for the names of their states.** A type keeps its lane, fields,
allowed dependencies, review and recovery level, and lists which of the six states it uses, and which
one it starts in:

```toml
[types.task]
lane = "default"
fields = ["priority", "labels", "requires"]
depends_on = ["task", "decision"]
initial = "open"                                  # "draft" holds every new task back
states = ["draft", "open", "active", "review", "done", "cancelled"]

[types.decision]
lane = "default"
fields = ["labels"]
initial = "review"
states = ["review", "done", "cancelled"]

[types.distill]
lane = "meta"
initial = "open"
states = ["open", "active", "done", "cancelled"]
```

`distill` gains `cancelled`, which it lacks today, so a pointless one need not be marked done.
`transitions = "any"`, the only value ever supported, goes away. The list of states still matters:
it keeps a decision from being moved to `open`, where an agent would take it.

**Superseded becomes derived, like blocked.** A `supersedes` link no longer writes `superseded` into
the target's state. The target keeps its state (normally `done`), and `wf item show` says "superseded
by #x", as it does today. Agent context takes the decisions that are `done` and not superseded.

## The CLI

```sh
wf item add --draft -t "Rework the dispatcher"   # created as a draft, in one event
wf item open k3f a8c 2b1                         # release drafts, or reopen
wf item draft k3f                                # hold back again
wf item done k3f                                 # finish without the runner
wf item cancel k3f                               # as now
wf item list --state draft                       # values complete; --state draft,open for both
wf item list --group-by state                    # always in the order above
wf item archive --done | --cancelled             # as now, without the hedging
```

* `open`, `draft` and `done` take several ids, so a batch of drafts can be released at once.
  `active` and `review` get no verbs, because the runner moves items into them.
* `wf item move` goes away. Moved into by hand, `active` and `review` are states nothing leads out
  of: nothing ends an `active` item without a session, and `review` reviews a submitted commit. A
  stuck item is put back with `open`, held with `draft`, or finished with `done` or `cancel`. A
  proposed decision is ratified with `done` and rejected with `cancel`; the inbox says so instead of
  "`wf item move` it on".
* `wf item add --draft` sets the state on `ItemCreated`, so the item is never `open`, not even for
  a moment. The editor template gets a `# state = "open"` line like `lane` and `review`.
* Every item shows as `(task:draft)`, `(decision:done)`, and so on. The second half always means the
  same thing, whatever the type.

**Dashboard:**
* The Items tab groups by the six states, in order, so drafts sit at the top.
* `s` starts an item, as now, by picking an agent. On a draft the list of agents starts with
  "release: open it for the dispatcher", which is selected first, so `s enter` releases a draft.
  Picking an agent starts it on the item right away, as `s` does on an open item; the item goes
  straight to `active`.
* `m` picks one of the verbs: `draft`, `open`, `done` or `cancelled`, less those the type does not
  use and the state the item is in. A decision gets `done` and `cancelled`.

## What goes away
* `StateCategory` and `StateName`, and the `first_state(category)` and `state_category_of` lookups:
  the state is the category.
* Policy validation of state names: duplicates, an unknown initial state, `BuiltinStatesChanged`.
* `Standing::Unknown`: a fixed enum is always known. If a type stops using a state, items already in
  it keep it, and only moves into it are refused.
* The `superseded` special case in `fold.rs`.

## Migration
Events are signed and hashed, so old ones stay as they are. `ItemState` decodes from the names on the
wire, accepting the old ones:

* `backlog` becomes `draft`.
* `in_progress` becomes `active`.
* `in_review` and `proposed` become `review`.
* `ratified` and `superseded` become `done`.
* `dropped` and `rejected` become `cancelled`.

An unknown name decodes as `draft`, so nothing starts the item, and `wf status` flags it. The policy
parser accepts the old `{ name, category }` form for the policies already in the log. Writing an old
name is refused. These are breaking changes, which rapid development allows (AGENTS.md).

## Decided
* **`draft`, not `backlog`:** it says "not released yet", which is what holding items back before
  development starts means.
* **`active`, not `in_progress`:** one word like the others, and the same word the SPEC and the code
  use for the concept. The runner sets it, so it is read far more often than typed. "Live" stays
  the word for items that are not archived and for sessions that have not ended, so "active" only
  ever means that a session works on the item.

* **No `wf item move`:** the verbs reach every state a principal may move an item to; `active` and
  `review` are the runner's.
* **`s` releases a draft in the dashboard:** releasing and starting a session are both "start work
  on this", so `s` keeps one meaning on every row, and the chooser asks who will do it: whoever is
  free, or a given agent.

## Steps
1. wf-core: `ItemState` with decoding of the old names; the policy's `states` and `initial` as lists
   of it; `ItemCreated.state`; superseded derived. Fold, readiness, lifecycle, review and stats on the
   enum.
2. CLI: `wf item add --draft`, `wf item open|draft|done` taking several ids, no `wf item move`,
   `--state` with fixed values, help text, JSON schemas.
3. Dashboard: grouping in state order, "release" first in the `s` chooser of a draft, `m` from the
   type's states.
4. SPEC §8 (items), §15 (policy) and §17 (CLI), README, the default policy.

## As built
* Events keep their bytes. A move names its state as `WrittenState`, the name as written, and decodes
  it through the table above. `ItemCreated` with a state has a new tag (17); one without, as written
  before, keeps tag 10 and its layout, and starts in the type's initial state under the current policy.
* An unknown name reads as `draft`; the item remembers the name, and `wf status` and `wf item show` flag
  it. They also flag an item in a state its type no longer uses.
* The policy reads old `{ name, category }` lists, an old `initial` through the old list it names, and
  `transitions = "any"`. `wf policy apply` and `edit` refuse that form.
* New: an item is created in its type's initial state, or as a `draft` if the type uses it.
* A decision that was superseded while still proposed used to become `superseded` (done); folded now, it
  stays in `review`.
* `wf item open`, `draft` and `done` check every item before writing, and leave alone those already in
  the state. `--json` gives `{message, events, items, unchanged}`. JSON schema 5.
