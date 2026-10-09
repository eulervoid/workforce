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

## Error Handling
- `thiserror` enums in library crates, `anyhow` only at the CLI boundary

## Commits
- Work in small commits, one feature at a time
- Use one line commit messages; if you'd need more than one line, your commit is too big

## Tracking work with `wf`
This repo tracks its own work in `wf` (M0 commands; build with `cargo build -p wf-cli`, binary at `target/debug/wf`, or `just wf <args>`). Ids are base32 prefixes of at least 4 characters, written `#k3f9` in text.

- `wf item ready` lists what can be worked on now; `wf item list` and `wf item show <id>` give detail and show what an item is blocked by.
- Start work with `wf item move <id> in_progress`, finish with `wf item move <id> done --note "..."`. Questions use `answered`, decisions use `ratified`.
- When you need an answer, `wf item add --title "..." --type question`, then `wf item link <task> --depends-on <question>` and stop; the task is ready again once the question is answered.
- Split big work as "the big item depends on its parts": `wf item link <big> --depends-on <part>`. There is no parent/child.
- Discuss with `wf comment <id> "text"`. `#id` references and `@handle` / `@handle/agent` mentions are found automatically (not in code). `wf mentions @handle` lists them.
- Use `--json` for machine-readable output. It has a versioned schema (`schema` field).
- `wf status` shows counts and anything flagged (unknown states, ignored cyclic dependencies). Do not edit `.workforce.toml`.
