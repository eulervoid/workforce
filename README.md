# workforce (`wf`)

A peer-to-peer, CLI-first coordination layer for coding agents and the humans who work with them.
Event-sourced, forkable, built on iroh. M0 (single peer: signed event log, item model, CLI) is implemented; sync and sessions come next.

- [docs/SPEC.md](docs/SPEC.md): the design
- [docs/M0.md](docs/M0.md): plan for the first milestone
- [docs/M1.md](docs/M1.md): plan for sync, device invites, daemon and fork

## Try it
```sh
cargo build
export PATH="$PWD/target/debug:$PATH"
cd ~/src/some-repo
wf init --name demo
wf item add --title "Implement feed sync" --priority P1
wf item ready
wf log
```
`just check` runs formatting, clippy and all tests. Property tests default to 128 cases; for the
10k-case run use `PROPTEST_CASES=10000 cargo test --release -p wf-core`. The 100k-event startup check
is `cargo test --release -p wf-store -- --ignored --nocapture`.
