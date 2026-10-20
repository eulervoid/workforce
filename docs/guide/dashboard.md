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
 workforce            1 Now  2 Items  3 Agents  4 Log
 3 live · 14 open · 3 waiting · 1 blocked · 2 devices
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
| `2 Items` | the live items, grouped by state (drafts first), lane, priority or type, sorted by priority, creation or merge as `wf item list --sort` does, with their steps and priority |
| `3 Agents` | this week's numbers, and each agent with its load and its live and latest sessions |
| `4 Log` | the event stream, as `wf log --follow` prints it |

Each tab is a list with a detail. From 120 columns the detail sits right of the list and follows the
selection; narrower, `enter` opens it and `esc` goes back. An item's detail shows its body, steps,
comments and history; a session's detail is its live transcript; one further right is the diff of the
item's work, as `wf item diff` computes it, against the target or, once merged, as its merge brought
it in: a header for each file with what it adds and removes, then its hunks, each line with its
numbers before and after.

While an agent works on another of your machines, the diff is read from that machine: a line above
it says which device and how long ago it answered. The dashboard asks again every few seconds while
the diff is on screen, and when the machine cannot be reached it keeps the last diff, with its age and
the reason. Only what is committed shows from another machine, not what its agent has yet to commit.

While an agent works on the item on this machine, the diff follows what it has committed, as the
daemon copies its commits into wf's repository at the end of every turn. Only the committed part takes comments on
lines: the uncommitted lines are not in any commit to anchor to, and `c` and `r` do nothing on them.
`p` shows the committed part through git.

### When the daemon restarts

After `wf daemon restart` the dashboard stays open. While the daemon is down the status line says it
is restarting, and the dashboard connects again on every refresh until the daemon answers.

A daemon of a newer build speaks a newer protocol than the `wf` that is open. If the `wf` executable
was rebuilt since the dashboard started, the dashboard restores the terminal and runs it with the same
arguments, so the rebuilt `wf` takes over in the same terminal. If it was not, the status line says to
rebuild wf and start it again. A daemon older than the dashboard needs `wf daemon restart`. Only the
dashboard does this; other commands fail at once with the mismatch.

### Keys

`?` shows them all. The keys that act work on the selected entry of the Now tab, or on the item or
session that is open.

| Key | Does |
|---|---|
| `1`–`4`, `tab`, `shift-tab` | switch tabs |
| `j`/`k`, `↓`/`↑`, `g`/`G` | move, top, end (and follow a log or transcript) |
| `pgup`/`pgdn`, `space` | a page up, down |
| `enter`, `l`, `→` | open; on an item, its session; on a session, the diff of its item |
| `esc`, `h`, `←` | go back |
| `ctrl+o` | back to where you followed a link |
| `H`, `L` (or `shift ←/→`) | only the list, both, or only the detail, in a wide window |
| `a` / `d` | allow / deny a permission request |
| `r` / `R` | answer a blocker in the footer / in `$EDITOR` |
| `v` | review: then `a` approves, `r` writes change requests in `$EDITOR`, `D` opens the diff |
| `D` | open the diff of the item |
| `]` / `[`, `enter` | in the diff: the next, previous file; fold a file to its header, or unfold it |
| `v`, then `j`/`k` | in the diff: select lines, as in vim's visual line mode, within one hunk; `esc` ends the selection |
| `c` / `r` | in the diff: comment on the selected lines, or the line of the cursor, or request a change to them |
| `e` / `d` | in the diff, on a comment of the review being written: edit it, drop it |
| `R` | in the diff: review, as `v` does elsewhere, sending the comments written on lines with the verdict |
| `p` | in the diff: show it through git instead, with your pager and diff config, such as delta |
| `s` | start a session on the item (pick the agent), or release a draft |
| `x` | stop the session (asks `y/n`) |
| `m` | move the item to another state (pick it) |
| `c` | comment on the item |
| `e` | edit the item in `$EDITOR` |
| `i` | on a session or a diff: open its item |
| `f` | on a session: the full transcript, without the abridged tool output |
| `o` / `O` / `F` / `/` | on the Items tab: grouping, order, readiness filter (ready, blocked, stranded), search the items as you type, as `wf item search` does: words in the title, body or comments, or the start of an id (`#` optional); `esc` clears, and the search shows in the list header |
| `/` | on the Log tab: filter the events |
| `ctrl+g` | carry what you are typing in the footer to `$EDITOR` |
| `q`, `ctrl+c` | quit |

Answering a blocker posts a comment that resolves it. Requesting changes in a review writes one change
request per line you write, then the verdict; the item goes back to the agent.

`p` keeps `less` open however short the diff is, so quitting it goes straight back to the
dashboard: it runs without the `F` of your `LESS`, or with `LESS=RX` when you have not set it. A
pager other than less has to stay open on its own, or a short diff closes at once.

### Comments on lines

In the diff, select lines with `v` and `j`/`k` or a drag, then `c` comments on them and `r` requests a
change to them; without a selection, they are about the line of the cursor. While the work waits for
your review, what you write is a draft of the review: it shows under its lines, `e` edits it and `d`
drops it, and the verdict (`R`, or `v` on the item) sends them all with it, so the agent gets one
review. Change requests on lines take the place of the ones written in `$EDITOR`; approving is refused
while there are some. With no review waiting, a comment on lines is posted at once.

A comment on lines names the commit, the file and the lines, and quotes them; `wf item show`, `wf log`
and the agent's brief show it as `src/lib.rs:12-14 at a8d49484` with the lines above its text. In the
diff, comments of the log show under their lines, or, when they are about an earlier commit, under the
header of their file, outdated, with the lines they quote.

### Mouse

The wheel scrolls what is under the pointer. A click picks a tab or an entry, and a second click opens
it. `#item`, `@you/agent`, sessions and commits are links. A link opens in place of the detail it is
in, so the breadcrumb at the top of a detail shows only the way along `l`: item, session, diff. `esc`
goes back along that way and `ctrl+o` back to where the link was followed. The footer's key hints and
the breadcrumb are clickable too. Allowing a request by click needs a second click on it within
three seconds; denying acts at once. In the diff, a click moves the cursor and a drag selects lines.
To select text, hold shift while dragging.

### Warnings

When the daemon moves an item from review back to open, as it does when approved work conflicts with
the target, the dashboard warns in yellow at the bottom right, on the line above the key hints:

```
⚠ #wjgenw back to open: merging into main conflicts in README.md
```

A click opens the item. The warning stays until you press a key or open the item; several at once show
the latest with ` · 2 more`. A tip under the pointer takes its place while it is hovered. Moves from
before the dashboard started do not warn.

### Focus

The place that has the keys is marked in blue; the rest is dimmed. The label of the tab on screen is
white. The whole dashboard dims while its terminal pane is not focused, that label included. In tmux this needs `set -g focus-events on`.

The blue, and every other colour of the dashboard and the CLI, can be changed with `[theme]` in the
machine config: see [Colours](machine-config.md#colours).

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
items: 6 ready, 3 archived
waiting on you: 2
next: 2 waiting on you: `wf inbox`
```

It also flags what needs someone: stranded items, review steps no agent can take, items in a state
the policy does not know.

## `wf log`

`wf log` prints every event of the log, verified. `wf log --follow` prints the whole log, then each
event as it arrives, one line per thing it did, until you interrupt it. `--item <item>` keeps only
the events of one item. With `--json`, `--follow` prints one JSON object per line.

With `--json`, each event names its item and the item's title. A review carries its `outcome`: the
commit it judged, the verdict and the comment that says why. A session's end carries the reason it
ended, the commit it submitted and its note.

## `wf stats`

`wf stats` lists each item that had sessions, archived ones included: its sessions, its submissions
(each a round of review), its change requests and the time from its first work session to done. The
items with the most rounds come first. `wf item show` prints the same for one item.

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
