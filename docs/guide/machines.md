# Several machines, the daemon, and backups

A workforce has no server. Each machine keeps the whole log in its own store and exchanges events
with the others directly, so you can add items on your laptop and have them picked up on a desktop
or a VM.

## Adding a machine

On a machine that holds your principal key (the one where you ran `wf init`):

```sh
wf daemon start                  # serves sync and join, follows the other devices
wf invite device                 # prints a wfi1... code
```

```
wfi1qk7m3...x9ta
valid until 2026-10-16T09:12:44Z; on the new machine, inside a clone of this repository, run:
  wf join <code> --name <machine>
this machine must be online: run `wf daemon start` or `wf sync --serve`
```

The code works once, for 24 hours by default (`--expires 30m`). On the new machine, inside a clone of
the repository (with its `.workforce.toml`):

```sh
wf join wfi1qk7m3...x9ta --name vm --max-sessions 2 --agent coder=2
wf daemon start
```

```
joined workforce kjfllqz46iux... as vm (@ada) with 1274 events
wf's repository is ~/.local/share/workforce/kjfllqz46iux.../git; the daemon keeps it current with the other devices. To work from a checkout of the project, in it run:
  git remote add wf ~/.local/share/workforce/kjfllqz46iux.../git
  git fetch wf
```

The new machine becomes a device of yours: same principal, its own device key. It never sees your
principal key. `--max-sessions` and `--agent` say how many sessions it runs and of which agents, as
`wf device edit` does later (see [agents.md](agents.md#where-agents-run)). `--hardware` says what it
has, such as `gpu, os:linux`; left out, its os and architecture.

Each daemon starts sessions for the agents its machine hosts, and one agent may run on several
machines. When more than one machine could start a session (a free slot for an agent that would
take it, what the item requires, and heard from within the lease), every machine ranks them alike:
first the machine the item's work last ran on, for a review the one that submitted the work, for a
blocker the one whose session asked; the rest by a hash that spreads items evenly. The first starts
at once, the second after 15 s, the third after 30 s, and each only if no other session started
meanwhile. So when the first cannot start (its harness needs a login, say), the next takes over
after its wait. With one machine nothing waits. When two machines still start a session on one item
before they sync, the later session ends `outclaimed` once they do, keeping its commits.

A session lasts as long as its machine is there. When a machine has not been heard from for the
policy's `[claims] lease` (3 minutes by default, see [policy.md](policy.md#claims)), another machine
of yours may take its sessions over: the dispatcher, or a start by hand (`wf session start` or the
dashboard's), ends the session `lease_expired` with a comment saying which machine was gone and
since when, then starts its own. While the machine is there, a start by hand refuses an item its
session holds; the dashboard leaves that to the daemon, which alone knows whom it heard from. When
the machine comes back, it stops the sessions that were taken over and names the last commit of
each in a comment, so that commit reaches every machine and the next session's brief names it.

`wf status`, `wf device list` and the Now tab of the dashboard show when each machine was last heard
from, and say `gone` once its lease ran out; only a running daemon knows. A session that ended
`lease expired` or `outclaimed` names the session that has its claim and the machine it runs on, in
the item's activity and under its transcript in the dashboard.

## Commits across devices

The log names every commit that matters: where a session's branch is when it comments or ends, and
what was submitted for review. Each device fetches the ones still needed from the device that wrote
the event (else from any other), over the same connection it syncs on, so work done on one machine
is there on the others without a remote or credentials. Everything in wf's repository is shared
with the workforce this way; your other branches never go there, and a plain `git push` to `wf`
pushes the target only. A session, a review or a merge that needs a commit that has not arrived
starts only once it has; `wf status` lists `waiting: ...` for each such commit, and so does the Now
tab of the dashboard. A commit no device could give is listed once as unavailable, with the device
that named it and since when, and asked for again every few minutes. When two sessions worked on one item and the
later one lost the claim, its commits are kept under `refs/wf/diverged/<session>` in wf's
repository, and the brief of the next session on the item names them.

## The target across devices

Each move of the target is in the log: a merge, and a push of yours to `wf`, which the daemon of
that machine notices within seconds. Every machine moves its `main` in wf's repository to the last
move the log names, so a merge made on one machine is in the others' `main` once its commit arrives.

Machines that cannot reach each other can both move `main` from the same commit. When they meet, the
earlier move wins on every machine, and the other machine puts its `main` back on the winner:

* **A merge that lost** goes back to review, with a comment from `@daemon`, and is merged again on
  the new `main`, checks included.
* **A push of yours that lost** waits in `wf inbox` as `lost_push`, and on the dashboard: rebase onto
  `main` with `git pull --rebase` and push again. The same happens to a push on top of a merge you
  pulled before it lost.

## Syncing

With the daemon running, devices find each other and exchange events by themselves. Without one:

```sh
wf sync                          # one exchange with every device that can be reached
wf sync --serve                  # and keep listening for the others until interrupted
```

```
$ wf sync
laptop: sent 3, received 12 events
1286 events known
```

Devices find each other through iroh, with the public relays and address lookup of n0. `--relay
<url>` on `wf daemon start`, `wf sync`, `wf join` and `wf fork accept` uses your own relay instead.

## Seeing the work of another machine

An item whose session runs on another machine has its agent repository only there. `wf item diff`
and the dashboard's diff ask that machine, through the daemon, for the work it has committed, so
you can follow an agent from your laptop while it works on a desktop. Both machines need to be
online and have synced; a machine that cannot be reached is said so, and the dashboard keeps the
last diff it had, with its age. The answer is asked for when you look and is not stored or synced.

## Devices and members

```sh
wf device list                   # name, principal, hardware, sessions, last seen or gone, flags
wf device edit --name desktop    # what this machine says about itself
wf members                       # members with their roles and machines
```

## The daemon

```sh
wf daemon start                  # in the background
wf daemon status                 # whether it runs, and where it logs
wf daemon log                    # its log, in a pager
wf daemon stop                   # stops it and its sessions
wf daemon restart                # stop and start again, with the binary that runs this command
wf daemon run                    # in this terminal, until interrupted
```

The daemon logs to `~/.local/share/workforce/daemon.log`, which `wf daemon log` shows; `WF_LOG` takes a `tracing` filter, such as
`WF_LOG=debug`. To run it as a service, have a systemd user unit or a launchd agent run `wf daemon
run`, which stays in the foreground and stops on SIGTERM.

The daemon raises its soft open-file limit to the hard limit (at most 65536) as it starts, and its
sessions and checks inherit it, so a daemon started from a macOS terminal does not keep its 256.

While the daemon runs it owns the store: `wf` commands ask it instead of opening the store
themselves. Without a daemon, commands open the store directly; agents and the dashboard need the
daemon.

Agents call the copy of `wf` the daemon made when it started, so rebuilding `wf` does not change what
running agents call; restart the daemon to give them the new one. Stopping the daemon does not end
its sessions: the next daemon resumes them in their agents' conversations, where the harness
supports that.

## Where things are kept

| What | Where |
|---|---|
| the store of a workforce, its worktrees and transcripts | `~/.local/share/workforce/<workforce id>/` |
| your principal key, one per workforce | `~/.local/share/workforce/<workforce id>/principal.key` |
| this machine's device key | `~/.local/share/workforce/keys/device.key` |
| the machine config | `~/.config/workforce/config.toml` |
| the workforce id, so `wf` finds the workforce | `.workforce.toml` in the repository |

`$XDG_DATA_HOME` and `$XDG_CONFIG_HOME` replace `~/.local/share` and `~/.config` when set. If you
created a workforce while the principal key was kept in `~/.local/share/workforce/keys/principal.key`,
move it next to the store.

## Losing a machine: fork

If a machine is lost or no longer trusted, start a new workforce without it. The items, comments and
history carry over; the lost device does not.

```sh
wf fork --exclude vm             # a device by name or the start of its key, or a member as @handle
wf fork --exclude laptop --new-principal   # when the lost machine held your principal key
wf fork list                     # what this workforce was forked from, and the forks announced in it
wf fork accept <id>              # on each remaining device: follow the fork
```

## Backups

```sh
wf export > backup.jsonl         # the log as JSON lines, one event per line
wf import backup.jsonl           # adds them to the store of their workforce, or creates it
```

Each line has `content`, the event readable with `jq`, and `event`, its signed encoding. The daemon
must not run during an import. For a `store.redb` that no longer opens, move it away and import a
backup. The keys are not part of an export: back them up yourself.
