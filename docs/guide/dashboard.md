# Keeping an eye on it: the dashboard and friends

## The dashboard

Plain `wf` at a terminal opens the dashboard. It is meant to stay open, for example in a pane next to
your editor: it shows what waits on you, who works on what, and lets you answer right there. It needs
the daemon (`wf daemon start`) and refreshes twice a second.

```sh
wf                   # the dashboard
wf --item k3f7       # the dashboard, opened on an item
```

Piped, plain `wf` prints the help instead; `wf log --follow` prints the event stream there.

```
 workforce          1 Now·3  2 Items  3 Agents  4 Log
 3 live · 14 open · 1 blocked · 2 devices
──────────────────────────────────────────────────────
 WAITING ON YOU
▸? coder on #k3j1 wants: rm -rf target        a / d
 ⧗ #x7k2 blocker from coder
   "Should the span live on ParseError or Loc…"
 ◎ #ab4c review 3e8a1c  +84 −12

 WORKING
 ◆ coder     #k3j1 Parse config lazily         12m
   ⠋ cargo test -p wf-core
 ◇ coder     #x7k2 Fix error spans     parked  40m
 ◆ reviewer  #ab4c Archive by category          2m

 NEEDS ATTENTION
 ! #q9r1 stranded: #m2n3 was cancelled
──────────────────────────────────────────────────────
 07:04  #ab4c  @ada/coder/s4k2 · submitted 3e8a1c → review
```

### Tabs

| Tab | What it shows |
|---|---|
| `1 Now` | **Waiting on you**: permission requests (`?`), blockers (`⧗`), reviews (`◎`), and other entries of `wf inbox` (`•`). **Working**: live sessions (`◆`) and parked ones (`◇`), each with what it is doing right now and how much of its context it has used. **Needs attention**: the flags of `wf status`, such as stranded items. The tab label counts what waits on you |
| `2 Items` | the live items, grouped by state (drafts first), lane, priority or type, with their steps and priority |
| `3 Agents` | this week's numbers, and each agent with its load and its live and latest sessions |
| `4 Log` | the event stream, as `wf log --follow` prints it |

Each tab is a list with a detail. From 120 columns the detail sits right of the list and follows the
selection; narrower, `enter` opens it and `esc` goes back. An item's detail shows its body, steps,
comments and history; a session's detail is its live transcript.

### Keys

`?` shows them all. The keys that act work on the selected entry of the Now tab, or on the item or
session that is open.

| Key | Does |
|---|---|
| `1`–`4`, `tab`, `shift-tab` | switch tabs |
| `j`/`k`, `↓`/`↑`, `g`/`G` | move, top, end (and follow a log or transcript) |
| `pgup`/`pgdn`, `space` | a page up, down |
| `enter`, `l`, `→` | open; on an item, its session |
| `esc`, `h`, `←` | go back |
| `H`, `L` (or `shift ←/→`) | only the list, both, or only the detail, in a wide window |
| `a` / `d` | allow / deny a permission request |
| `r` / `R` | answer a blocker in the footer / in `$EDITOR` |
| `v` | review: then `a` approves, `r` writes change requests in `$EDITOR`, `D` shows the diff |
| `D` | show the diff of the item, through git and your pager |
| `s` | start a session on the item (pick the agent), or release a draft |
| `x` | stop the session (asks `y/n`) |
| `m` | move the item to another state (pick it) |
| `c` | comment on the item |
| `e` | edit the item in `$EDITOR` |
| `i` | on a session: open its item |
| `f` | on a session: the full transcript, without the abridged tool output |
| `o` / `F` / `/` | on the Items tab: grouping, readiness filter (ready, blocked, stranded), title filter |
| `/` | on the Log tab: filter the events |
| `ctrl+g` | carry what you are typing in the footer to `$EDITOR` |
| `q`, `ctrl+c` | quit |

Answering a blocker posts a comment that resolves it. Requesting changes in a review writes one change
request per line you write, then the verdict; the item goes back to the agent.

### Mouse

The wheel scrolls what is under the pointer. A click picks a tab or an entry, and a second click opens
it. `#item`, `@you/agent`, sessions and commits are links. The footer's key hints and the breadcrumb at
the top of a detail are clickable too. Allowing a request by click needs a second click on it within
three seconds; denying acts at once. To select text, hold shift while dragging.

### Focus

The place that has the keys is marked in blue; the rest is dimmed. The whole dashboard dims while its
terminal pane is not focused. In tmux this needs `set -g focus-events on`.

## `wf inbox`

What waits on you, as a table:

```
$ wf inbox
 WAITING           ITEM     TITLE                          WHAT
 principal_review  #rz7rl6  Disk budget for build caches   review of c0e32354: `wf item diff`, then `wf review`
 blocker           #x7k2ab  Fix error spans                blocker q4tz: answer with `wf comment x7k2ab "..." --resolve q4tz`
```

The `WHAT` column says what to do. The kinds:

| Kind | What waits | How to answer |
|---|---|---|
| `blocker` | a question or a wait that someone has to resolve | `wf comment <item> "<answer>" --resolve <comment>` |
| `proposal` | a decision to accept or reject | `wf item done <item>` or `wf item cancel <item>` |
| `principal_review` | your step of a review | `wf item diff <item>`, then `wf review <item> --approve` or `--request-changes` |
| `merge` | approved work that cannot be merged while the target branch is checked out with changes | commit or stash your changes; the daemon tries again by itself |
| `recovery` | sessions that did not finish, and the item's recovery level leaves it to you | comment with what to do differently, or `wf session start <item>` |
| `permission` | an agent waits for an answer | `wf session allow <session>` or `deny` |

`wf inbox` shows permission requests while the daemon runs. These are the entries the dashboard's Now tab
lists, and what [notifications](machine-config.md#notifications) tell you of.

## `wf status`

A summary of the workforce, and what to do next:

```
$ wf status
workforce "eulerforce" kjfllqz46iuxrcrez4e4kqnmvkuxdtbupjm7t3a5v5bno6qqniva
members: @eulervoid (owner)
events: 1274  heads: 1  pending: 0  agents: 4
devices: namioka (this)
daemon: running
sessions: 2 live, 0 parked
items: 6 ready, 2 waiting, 3 archived
next: 2 waiting on you: `wf inbox`
```

It also flags what needs someone: stranded items, review steps no agent can take, items in a state
the policy does not know.

## `wf log`

`wf log` prints every event of the log, verified. `wf log --follow` prints the whole log, then each
event as it arrives, one line per thing it did, until you interrupt it. `--item <item>` keeps only
the events of one item. With `--json`, `--follow` prints one JSON object per line.

## Sessions

```sh
wf session list                  # live sessions first; --live for only those
wf session show <session>        # one session
wf session watch <item>          # the transcript of the item's latest session, followed
wf session watch <session>       # or of one session
wf session stop <session>        # stop it; the item goes back to open
```

`wf session watch` renders the agent's markdown, shows thoughts dimmed, and shows tool results,
plans and diffs. Tool output and diffs are cut at 20 lines a block; `--full` shows everything.
`--raw` prints plain text without markdown or color, and streams text as it comes instead of a
paragraph at a time. `--no-follow` prints what is there and stops. `Ctrl-C` stops watching; the
session goes on.

Every command that takes an item or a session offers a picker at a terminal when you leave it out:
`wf session stop` offers the live sessions of this machine, `wf session allow` and `deny` the pending
requests, `wf session watch` the sessions with a transcript here.
