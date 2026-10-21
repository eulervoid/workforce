## General
- We are still in rapid development mode, so you don't need to worry about braking changes

## Design
- Don't be afraid to question previous assumptions if something does not click
- Consider future implications of your changes, but lean towards simplicity and **don't over-engineer**
- Use precise language and take great care that the names and concepts you use are descriptive and consistent 
- Types that do things (call, sample, judge, measure) get the plain name (`Provider`, `Target`, `Judge`); what is read from the policy or the machine config is marked with a `Config` suffix (`ReviewLevelConfig`, `MachineConfig`). Say "config", never "settings"
- *Principal* always means the accountable person (SPEC §5); never say human, user or person for it in code, CLI or docs
- When two concepts compete for one name, don't separate them with a synonym or a prefix (`Settings` vs `Config`, `Raw*`, `Ready*`). Step back, state the purpose of each in one sentence and name it for that purpose; if you can't state two distinct purposes, they should be one concept
- Practice type driven design and make illegal states unrepresentable
- Model domain states with algebraic data types (enums) so each variant carries only the data valid for that state
- Avoid boolean flags and stringly-typed values when an enum describes the options; avoid `Option<T>` pairs that must be both `Some` or both `None`
- Match exhaustively: avoid `_ =>` wildcard arms on enums we own, so adding a variant produces compiler errors at every place that needs updating
- Encode state machines in types, either as enum variants or typestate (distinct types per state), so invalid transitions fail to compile
- Wrap primitives in newtypes for domain concepts where mixing up or corrupting a value would be costly or hard to notice (e.g. `UserId` vs `OrderId`) 
  - If a type from the ecosystem exists (`url::Url`, `std::num::NonZero*`, `non-empty-string`, `jiff`), use it; don't hand-roll one
  - Before adding a newtype, name the bug it prevents and the places that rely on its invariant; if there is no concrete bug, use the plain type
  - Keep fields private and validate once in a constructor (`TryFrom`/`parse`) for the types that do pass this test
- **DON'T OVERDO IT**: if you can't clearly justify why the type level encoding is worth it, go with the simpler option


## Code
- Write idiomatic, readable code, avoid overly clever tricks and unnecessary abstractions
- Use clear and descriptive variable names, avoid single letters and overly short abbreviations
- Before planning an implementation, check for published crates, don't reinvent the wheel
- `wf-core` is pure: no IO, no async, no clock or randomness except through parameters
- `wf-net` protocols are written against generic streams and a `SyncLog` trait, so convergence is tested in memory; iroh only carries them
- The daemon owns the store when it runs: CLI code reads and writes through `Workspace`, never through the store directly
- `wf-vcs` is the only crate that runs `git`. What happens to an item when a session starts, ends or finishes a turn is a pure function in `wf-core::lifecycle`; the daemon only carries it out

## Error Handling
- `thiserror` enums in library crates, `anyhow` only at the CLI boundary

## Commits
- Work in small commits, one feature at a time
- Use one line commit messages; if you'd need more than one line, your commit is too big

## Tests
- A test guards a real scenario, not itself: its setup (sizes, concurrency, timing, load) matches what can happen in use. When a test fails because of its environment (open files, time limits, load), first ask whether it models anything real; if not, change the test, not the system or the machine to fit it
- A test that needs more than Fence allows (the network for iroh) starts with `if wf_core::testing::skip_in_fence("...") { return; }`, so `cargo test` in a session passes or fails for real and runs everything outside Fence; a test that needs git signing, which a session's `GIT_CONFIG_*` would override, runs `git` without them (`wf_vcs::testing`, `Sandbox`)
- The `wf-cli` test `Sandbox` removes `WF_SESSION`, `WF_SESSION_TOKEN` and `WF_WORKFORCE` (which the daemon sets in sessions) from itself and from every `wf` it spawns; a new helper that spawns `wf` must go through `Sandbox::command`, or `cargo test` fails inside a session with "the local log for workforce ... is missing or does not match"
- Any other failing test is yours to explain: never report a failure as "sandbox-only", and fix or explain it before you submit
- The principal, or a check outside the sandbox, runs the marked tests with `cargo test --workspace -- --ignored` (the slow ones among them are marked "slow" and run in release mode)

## Sharing the machine
- Several sessions build and test on one machine at once: while you work, run the tests of the crate you change (`cargo test -p <crate>`), and `cargo test --workspace` once before you submit
- Never load the machine on purpose (`yes`, stress tools, many test runs at once) to reproduce a race; make the race deterministic instead: paused tokio time, an injected delay or clock, a hook that holds one side
- Nothing you start may outlive the command that started it: no background processes left behind by a turn, and a test that spawns a process stops it also when the test fails (a process that ends by itself, such as `sleep 120`, rather than one that runs forever, such as `yes`)
- New tests wait for the event they expect, not for a fixed time; a deadline only bounds a wait so that a hang fails instead of running forever
