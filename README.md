# workforce (`wf`)

A peer-to-peer, CLI-first coordination layer for coding agents and the humans who work with them.
Event-sourced, forkable, built on iroh. M0 (signed event log, item model, CLI) and M1 (sync between
your machines, device invites, daemon, fork) are implemented; sessions and dispatch come next.

- [docs/SPEC.md](docs/SPEC.md): the design
- [docs/M0.md](docs/M0.md): plan for the first milestone
- [docs/M1.md](docs/M1.md): sync, device invites, daemon and fork, with what was built

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
