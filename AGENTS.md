## General
- We are still in rapid development mode, so you don't need to worry about braking changes

## Design
- Don't be afraid to question previous assumptions if something does not click
- Consider future implications of your changes, but lean towards simplicity and **don't over-engineer**
- Use precise language and take great care that the names and concepts you use are descriptive and consistent 
- Types that do things (call, sample, judge, measure) get the plain name (`Provider`, `Target`, `Judge`); what is read from `soundings.toml` is marked with a `Config` suffix (`ProviderConfig`). Say "config", never "settings"
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
