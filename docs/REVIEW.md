# Workforce (`wf`) — Architecture, Design, and Implementation Review

Overall, `workforce` has an unusually high standard of engineering: the pure/impure boundary between `wf-core` and the outer crates is clean, the use of algebraic data types and exhaustive pattern matching is disciplined, and the test suite (golden tests, property-based convergence tests, network schedule simulations, and snapshot tests) is thorough.

However, looking beneath the surface reveals several **challenged assumptions**, **needless complexities**, **architectural bottlenecks**, and **subtle correctness/security bugs**. Below is a detailed review organized from high-level design assumptions down to concrete implementation bugs.

---

## 1. Challenged Assumptions & High-Level Design Tensions

### 1.1 "We are in rapid development, no need to worry about breaking changes" vs. Heavy Legacy Compatibility Plumbing
`AGENTS.md` and `README.md` explicitly state that `wf` is in early, rapid development and breaking changes to the event log and policy are expected. Yet `wf-core` carries a massive amount of backward-compatibility scaffolding for its own pre-v0 history:
- **Combinatorial explosion of event tags (`crates/wf-core/src/event.rs`):**
  - `Commented` has **4 wire tags** (`16`, `26`, `27`, `30`) and two shadow structs (`CommentWithoutLines`, `CommentWithoutHead`) with custom conditional encoding/decoding so older variants re-encode to identical bytes.
  - `SessionEnded` has **4 wire tags** (`21`, `25`, `31`, `32`), two shadow structs (`SessionEndedWithoutHead`, `SessionEndedWithoutComment`), and a separate `WrittenEndReason` enum (`event.rs:666-823`).
  - `Merged`, `Reviewed`, `SessionStarted`, `ItemCreated`, and `PolicySet` (`PolicySet` vs `BoundPolicySet`) all maintain dual wire formats.
- **Dual-format policy & state parsers (`crates/wf-core/src/policy.rs`, `item.rs`):**
  - `written_states` (`policy.rs:634-737`) parses TOML twice on every `Policy::parse` call just to support legacy `{ name, category }` state tables and `transitions = "any"`.
  - `written_steps` (`policy.rs:264-411`) supports legacy `"human"` aliases and `{ agent = { requires } }` wrappers.
  - `WrittenState` (`event.rs:160-189`, `item.rs:98-107`) keeps strings in `ItemState` registers so old state names like `"backlog"`, `"in_progress"`, `"ratified"`, and `"superseded"` still round-trip.

**Why this matters:**
Every new optional field on `Commented`, `SessionEnded`, or `Reviewed` requires minting a new tag, writing a new shadow struct, and writing custom rejection logic (`Err(EncodingError::malformed())`) to preserve canonical encoding. As noted in `SPEC.md §17.7`, this is already becoming painful.
- **Recommendation:** While in rapid development (`0.0.0`), either drop pre-release compatibility shims before cutting a real v1 (resetting to 1 tag per `EventBody` variant), or if dogfooding logs must be preserved, provide a one-time `wf migrate` / `wf export | wf import` re-signing tool rather than carrying permanent complexity in the core domain types and fold.

### 1.2 The `wf` Intermediate Bare Repository (`<wf>/git`) Hurts Single-Machine Ergonomics
To make multi-device git sync (`wf/git/1`) clean and avoid touching the principal's working tree, `wf` puts a bare repository `<wf>/git` between the principal's checkout and the agents (`crates/wf-vcs/src/bare_repository.rs`).
- On a single machine (Stage 1, the primary daily workflow), the principal's checkout is configured with `remote.wf`.
- When an agent's work is approved and merged, it merges into `<wf>/git`, **not** the principal's checkout. The principal must remember to `git pull` to see merged work, and `git push` to make their own local commits visible to agents.
- If the principal commits locally on `main` and adds an item without running `git push`, the agent branches off the stale `main` in `<wf>/git`! The dashboard (`checkout_target` warning) only warns *after* the fact.
- **Recommendation:** Question whether `<wf>/git` should automatically fast-forward from/to the local checkout when on the same machine:
  1. When starting a session or rebasing at submit, if a local checkout is known and ahead of `<wf>/git`'s target (fast-forwardable), `wf` could automatically fetch the target from the local checkout (or warn prominently on `wf item add` / `wf session start`).
  2. When a merge completes in `<wf>/git`, if the local checkout has `target` checked out and is clean (`is_clean`), `wf` could fast-forward the checkout automatically (or via a `git update-ref` + worktree update when clean).

### 1.3 In-Memory Full Log & `State` Cloning Over IPC
Every CLI invocation and every dashboard refresh (every 500 ms) serializes and sends the **entire** `State` over the Unix socket (`Call::State`), and the dashboard also loads **every `Event` since genesis** into `Feed::events: Vec<Event>` (`crates/wf-cli/src/dashboard/mod.rs:309, 363`) and keeps it in memory forever!
- Even worse, inside the daemon (`crates/wf-daemon/src/runner/mod.rs:358`), `daemon.snapshot(loaded)` clones the entire `State` (all items, all bodies, all sessions, all known event metadata `BTreeMap<EventId, Known>`, all CRDT stamps) on the storage thread before returning it to the async caller. This happens multiple times per session start, turn end, comment, and merge.
- And in `wf item show` / dashboard `InView::take` (`crates/wf-cli/src/dashboard/snapshot.rs:563, 603`), rendering an item detail or diff scans the entire `events: &[Event]` slice linearly.
- **Recommendation:**
  1. Wrap `State` inside `Replica` in an `Arc<State>` (using `Arc::make_mut` on mutation in `apply`). Then `daemon.snapshot()` is an `O(1)` `Arc::clone` instead of deep-cloning tens of megabytes of `BTreeMap`s on the single storage thread!
  2. The dashboard does not need `self.events: Vec<Event>` holding the entire history of the workforce in RAM. It only needs the last `BACKLOG` (200) events for the Log tab, and `workspace.item_events(id)` (or an index in `RedbStore`) for the currently viewed item.

---

## 2. Protocol, Event Log & Convergence Issues

### 2.1 `target::chain` Can Silently Drop the Entire Target Branch History on a Fork (`crates/wf-core/src/target.rs:46-79`)
Look closely at how `target::chain` computes the canonical target branch chain from `TargetMove` events:
```rust
let made: BTreeSet<CommitId> = moves.values().map(|step| step.commit).collect();
let mut by_onto: BTreeMap<CommitId, &TargetMove> = BTreeMap::new();
for step in moves.values() {
    by_onto
        .entry(step.onto)
        .and_modify(|earliest| {
            if step.stamp < earliest.stamp {
                *earliest = step;
            }
        })
        .or_insert(step);
}
let Some(start) = moves
    .values()
    .filter(|step| !made.contains(&step.onto))
    .min_by_key(|step| step.stamp)
else {
    return Vec::new();
};
```
Consider three problems with this algorithm:
1. **Unconnected root / pre-chain push:** Suppose Device A initializes the repo at commit `C1`, and merges an item `C1 -> C2` at `t=10`. Later, Device B (whose `<wf>/git` was initialized from an older checkout at `C0`, an ancestor of `C1`, before `C1` was pushed to `wf`) records a move `C0 -> C1` at `t=20`, OR suppose a device records a move from an unrelated commit `X -> Y` at `t=5`. Because `!made.contains(&step.onto)` only picks the single global minimum by `stamp`, an earlier stray move `X -> Y` becomes `start`, and the entire real chain `C1 -> C2 -> C3...` is treated as `lost_moves`!
2. **Reverting target to a previous commit breaks `made` & cycle detection:** If the principal ever reverts `main` to a commit tree/oid that was previously on `main` (e.g., resetting a bad merge outside `wf` and pushing, or if two moves form a diamond), `made.contains(&start.onto)` can become true for the root, causing `start` to be `None` and `chain()` to return `Vec::new()`!
3. **`follow_target` (`crates/wf-daemon/src/runner/target.rs:112-117`) when pushing on top of a losing merge:**
   In `decide()`, if `reading.tip` is `Some(tip)` and the principal pushed `P1` on top of a *losing* merge `M_lost` (`tip -> M_lost -> P1`), `merge_base(P1, tip)` is `tip`. `follow_target` then records `TargetMoved { commit: P1, onto: tip }`, even though `P1` contains `M_lost`! If `P1` happens to have an earlier HLC than the winning move from `tip` (or if `P1` is chained onto `tip`), it misrepresents the causal git history of `P1`.

### 2.2 `State::known` Grows Unboundedly and Stores Full Parent Vecs in RAM (`crates/wf-core/src/state.rs:171-177, 423`)
In `State`:
```rust
pub(crate) struct Known {
    pub(crate) hlc: Hlc,
    pub(crate) parents: Vec<EventId>,
    pub(crate) author: DeviceKey,
    pub(crate) seq: u64,
}
```
Every applied event in the history of the workforce stays in `State::known: BTreeMap<EventId, Known>`, and is serialized into every redb snapshot and every `Call::State` IPC message over the Unix socket!
- At 100k events, `State::known` alone is ~10–15 MB of serialized data sent over the Unix socket twice a second to the dashboard.
- Why is `parents: Vec<EventId>` kept in `State::known` for every event ever applied? Only for one single check: `in_causal_past` (`crates/wf-core/src/validate.rs:773-793`), which checks that `Commented.resolves` is in the causal past of the event!
- Removing `parents: Vec<EventId>` from `Known` (or moving `known` out of the IPC `State` payload, since the CLI never calls `State::receive` when connected to a daemon!) would dramatically shrink `State` in memory and over the socket.

### 2.3 Sync Protocol (`wf/sync/3`) Re-Encodes Events Repeatedly and Loads All Missing Feed Events into Memory at Once
In `crates/wf-store/src/exchange.rs:24-38`:
```rust
pub fn events_after(&self, theirs: &Versions) -> Result<Vec<Event>, StoreError> {
    let mut events = Vec::new();
    for outgoing in sync::ahead_of(&self.state().versions(), theirs) {
        events.extend(
            self.store()
                .feed_since(&outgoing.author, outgoing.after_seq)?,
        );
...
```
And then in `crates/wf-net/src/sync.rs:251-268`:
```rust
fn batches(events: &[Event], max_events: usize, max_bytes: usize) -> Vec<&[Event]> {
    ...
    for (index, event) in events.iter().enumerate() {
        let length = event.encode().map_or(0, |encoded| encoded.len());
```
1. When syncing a 100k-event log to a new peer, `events_after` loads and decodes all 100,000 `Event`s from `redb` into a single `Vec<Event>` inside a single `StorageHandle::with` closure—blocking the storage thread for the entire duration!
2. Then `batches()` encodes every `Event` to a `Vec<u8>` just to measure its byte length, drops the `Vec<u8>`, clones the slice `batch.to_vec()`, and then `write_frame_within` encodes the `Event`s **a second time**!
3. And when `Event::id()` is called (in `causal_order`, `receive_events_unchecked`, `commit`), `event.encode()` is called **again** (3–4 times per event!).

### 2.4 `Reservations` in `wf/join/2` Never Expires Unfinished Joins (`crates/wf-net/src/join.rs:48-59`)
```rust
pub(crate) struct Reservations(Arc<Mutex<BTreeMap<EventId, DeviceKey>>>);

impl Reservations {
    fn reserve(&self, invite: EventId, device: DeviceKey) -> bool {
        let mut held = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *held.entry(invite).or_insert(device) == device
    }
}
```
`Reservations` never removes entries when invites expire or are redeemed, growing for the lifetime of the daemon. And if a principal runs `wf join` on a temporary/wrong machine and interrupts it before `author()` completes, that invite can *never* be used on another machine without restarting the inviting daemon.

### 2.5 `BoundPolicySet` Authoring Race Condition (`crates/wf-cli/src/commands/policy.rs` & `crates/wf-store/src/replica.rs:295-301`)
To author a `BoundPolicySet`, the CLI calls `state.next_hlc(now_ms)` (`state.rs:765`), signs `BoundPolicySet { policy, hlc }` with the `PrincipalSigner`, and sends `Call::Author { body }` to the daemon.
- If **any** event is authored by the daemon or received via sync between the CLI reading `state` and sending `Call::Author` (for example, an agent commenting, starting, or ending a session), the daemon's `floor` HLC advances past the `hlc` signed in `BoundPolicySet`!
- Then `replica.author` sets `builder = builder.with_hlc(hlc)`, signs the event, and calls `self.state.receive(event)`, which fails with `Invalid::HlcNotAfterParents` ("hlc is not greater than the hlc of the events it follows")!
- On an active workforce where agents are working or syncing, `wf policy apply` / `wf policy edit` can spuriously fail with `"hlc is not greater than the hlc of the events it follows"`.
- **Fix:** Either retry signing with a fresh `next_hlc` in the CLI when `HlcNotAfterParents` occurs, or only require in `validate.rs` that `signed.statement.hlc <= event.content.hlc` and `signed.statement.hlc > previous_bound_policy.hlc` (so the signature timestamp acts as a monotonic nonce rather than having to equal the exact envelope `hlc`).

---

## 3. Daemon Concurrency & Lifecycle Bugs

### 3.1 Per-Item Start Lock Only Covers `SessionRole::Work` (`crates/wf-daemon/src/runner/start.rs:161-168`)
In `Daemon::start_session`:
```rust
let _starting = match role {
    SessionRole::Work => Some(
        self.starting
            .take(item)
            .ok_or(SessionError::Starting(item))?,
    ),
    SessionRole::Review { .. } | SessionRole::Supervise | SessionRole::Diagnose => None,
};
```
Why is `self.starting` only acquired for `SessionRole::Work`?
- If a manual `wf session start` is called on the socket handler (`server.rs:379`) while the dispatcher is starting a `Review` session on the same item (or if two callers start a review concurrently), the first yields at `ensure_bare_repository`, `require_commit`, `GitCli::open` (`spawn_blocking`), or `take_over` before writing `SessionStarted`.
- Both see `state.claimable(...) == Ok` and **both spawn review sessions on the same device** for the same review step!
- **Fix:** Key `self.starting` by `(ItemId, SessionRole)` (or simply `ItemId`) for **all** `SessionRole` variants, not just `Work`.

### 3.2 `ParentHistory` Can Deadlock Two Storage Threads (`crates/wf-daemon/src/lib.rs:200-221`)
Look at `ParentHistory::history_behind`:
```rust
impl HistorySource for ParentHistory {
    fn history_behind(
        &self,
        parent: WorkforceId,
        wanted: &std::collections::BTreeSet<wf_core::id::EventId>,
    ) -> Vec<Event> {
        ...
        loaded
            .storage
            .with_blocking(move |replica| HistorySource::history_behind(&*replica, parent, &wanted))
            .unwrap_or_default()
    }
}
```
`ParentHistory::history_behind` is called *inside* `Replica::fill_history()` on Workforce B's dedicated storage thread, and it blocks waiting for Workforce A's storage thread via `with_blocking`.
- If Workforce A was forked from Workforce B and Workforce B was forked from Workforce A (e.g., via a crafted `Genesis` event or mutual fork history), or if a chain of forks is syncing concurrently, storage thread B blocks on storage thread A while holding its own job queue. If A queries B, both storage threads deadlock permanently.

### 3.3 `turn_outcome` / `turns_without_commit` Counts Against Multi-Turn `Supervise` Sessions (`crates/wf-daemon/src/runner/drive.rs:372-377`)
In `drive.rs:372-377`:
```rust
let head = branch_head(run);
if head != last_head {
    turns_without_commit = 0;
    last_head = head;
} else {
    turns_without_commit += 1;
}
```
For `SessionRole::Review`, `Supervise`, and `Diagnose`, `branch_head(run)` is always `None` (because reviewers run on detached `HEAD` and supervisors/diagnosers run in an empty directory).
- If a `Supervise` session has **multiple** open blockers on an item and resolves one blocker in turn 1, intending to resolve the next in turn 2, `branch_head(run)` is `None == None`, so `turns_without_commit` increments even though the supervisor made progress.

### 3.4 `PassedChecks` Cache Key Uses `format!("{sandbox:?}")` and Never Prunes (`crates/wf-daemon/src/runner/check.rs:140`)
```rust
// What the sandbox allows is all of its debug form, so that a changed config is another
// sandbox.
key.update(format!("{sandbox:?}").as_bytes());
```
Relying on `Debug` formatting for a persistent on-disk cache key (`<wf>/checks-passed/<blake3>`) is fragile. In addition, `<wf>/checks-passed/` and `<wf>/check-logs/` are **never pruned** by `keep_caches_in_budget` or `remove_stray_repositories`, so check logs and pass markers accumulate indefinitely on disk.

---

## 4. VCS / Git & Multi-Device Sync Issues

### 4.1 `GitCli::open` Runs Two Subprocesses Every Single Time (`crates/wf-vcs/src/git.rs:290-319`)
Every call to `GitCli::open(&path)` runs:
1. `git worktree list --porcelain -z`
2. `git rev-parse --path-format=absolute --git-common-dir`

And look at how `Daemon` uses `GitCli::open`:
- In `commits::reconcile` (`crates/wf-daemon/src/commits.rs:284, 360`), `GitCli::open(&bare)` is called twice per pass.
- In `runner/target.rs:150, 214, 226`, `GitCli::open(&bare)` is called every 5 seconds (`LOOK_FOR_PUSHES`) in `run_target`!
- That means an **idle daemon** spawns multiple `git` subprocesses every 5 seconds per workforce just to check if `main` moved (`git worktree list`, `git rev-parse --git-common-dir`, `git rev-parse --verify refs/heads/main^{commit}`), even when nothing is happening!
- **Recommendation:**
  1. Cache `GitCli` on `Loaded` once `<wf>/git` is created (its `repository` and `git_directory` paths never change).
  2. In `run_target`, read `<wf>/git/refs/heads/<target>` and `<wf>/git/packed-refs` directly from disk (like `agent_branch_head` already does in `agent_repository.rs:271-287`) before spawning `git` every 5 seconds. That reduces idle daemon subprocess spawning to **zero**.

### 4.2 `agent_branch_head` Suffix Matching on `packed-refs` (`crates/wf-vcs/src/agent_repository.rs:281-285`)
```rust
Err(_) => read_regular_file(&git_directory.join("packed-refs"), PACKED_REFS_LIMIT)?
    .lines()
    .find_map(|line| line.strip_suffix(reference.as_str())?.strip_suffix(' '))?
    .to_string(),
```
If an agent creates a ref whose name has `refs/heads/wf/<item>` as a suffix (e.g. `refs/remotes/origin/refs/heads/wf/<item>`), `line.strip_suffix(reference)?.strip_suffix(' ')` only checks the suffix of the line, not that the line is `<40-hex-oid> <reference>`.
- Using `let (oid, refname) = line.split_once(' ')?; (refname == reference).then_some(oid)` is both simpler and exact.

### 4.3 `ImportMode::Submission` Re-Imports From Agent Repo After Checks Pass (`crates/wf-daemon/src/runner/end.rs:138-146`)
In `finish_session`:
```rust
let mode = if state.rebases_at_submit(session)
    && self.submissions.passed(session).is_none()
{
    ImportMode::Submission
} else {
    ImportMode::InProgress
};
```
When `submission_passed` calls `finish_session` the second time after checks finish (`submit.rs:292`), `finish_session` calls `self.import_commits(..., ImportMode::InProgress)` again and reads `head` from the agent's repo.
- If `head` in the agent's repo changed while checks were running, `self.submission_checked(session, head)` (`end.rs:197`) will be `false`, starting a new check run—or if the agent broke its branch, `head` differs from what was checked.
- **Fix:** When `self.submissions.passed(session)` is `Some(checked_commit)`, `finish_session` should use `Some(checked_commit)` directly instead of re-importing from the agent's working directory.

---

## 5. Security, Sandboxing & Permission Boundary Issues

### 5.1 Sandbox `ALLOW_WRITE` Grants Write Access to Agent Harness Configs & Cargo Registry (`crates/wf-daemon/src/sandbox.rs:184-221`)
Look at `crates/wf-daemon/src/sandbox.rs:184-221`:
```rust
const ALLOW_WRITE: &[&str] = &[
    ".",
    "/tmp",
    "~/.cache/**",
    "~/.cache",
    // Claude Code
    "~/.claude*",
    "~/.claude/**",
    // Amp
    "~/.amp",
    // Codex
    "~/.codex/**",
    // Cursor
    "~/.cursor/**",
    // OpenCode
    "~/.opencode/**",
    "~/.local/state/**",
    // Gemini CLI
    "~/.gemini/**",
    ...
    "~/.cargo/registry/**",
    "~/.cargo/git/**",
```
1. **`~/.claude*`, `~/.opencode/**`, `~/.codex/**`, `~/.gemini/**`, `~/.cursor/**` are writable by every sandboxed agent:**
   - Those directories hold the user's **global harness configuration, MCP server definitions, hooks, custom commands, plugins, and `settings.json`** (e.g., `~/.claude/settings.json` which defines `hooks` and allowed tools for Claude Code, or `~/.opencode/opencode.json` which defines plugins/commands).
   - A sandboxed agent can edit `~/.claude/settings.json` or `~/.opencode/...` to register a malicious command hook or MCP server, and the **next time the principal runs `claude` or `opencode` outside the sandbox**, the agent's code executes with full principal privileges!
2. **`~/.cargo/registry/**` and `~/.cargo/git/**` are writable:**
   - A sandboxed agent can modify any unpacked crate source in `~/.cargo/registry/src/.../build.rs`. The next time the principal runs `cargo build` or `cargo test` in their own checkout outside the sandbox, the poisoned `build.rs` executes unsandboxed.
3. **`allowLocalOutbound: true` and `allowLocalBinding: true` (`sandbox.rs:102-103`):**
   - Every sandboxed agent can connect to any localhost port (`127.0.0.1:*`), including local services and the daemon's own local ports.

### 5.2 Built-in Permission Rules (`rules.rs`) Edge Cases (`crates/wf-daemon/src/decide/rules.rs`)
1. **`harmless_jq` uses substring matching (`crates/wf-daemon/src/decide/rules.rs:223-226`):**
   ```rust
   fn harmless_jq(program: &str) -> bool {
       !["import", "include", "env", "ENV"]
           .iter()
           .any(|word| program.contains(word))
   }
   ```
   A jq filter like `.data[] | select(.environment == "prod")` or `.data[] | select(.title | contains("import"))` is rejected because `"environment"` contains `"env"` and `"import"` contains `"import"`. Checking identifier tokens rather than raw substrings avoids false positives.
2. **`RIPGREP_CONFIG_PATH` interaction:**
   If `RIPGREP_CONFIG_PATH` is inherited or set in the environment and contains `-e <pattern>`, then `wf status | rg /path/to/secret_file` passes `rules.rs` because `/path/to/secret_file` is counted as the single `patterns == 1` positional argument, causing `rg` to search the file instead of stdin. Requiring `--regexp=<pattern>` or `-e <pattern>` (or appending `-` for stdin) would prevent a positional argument from ever being interpreted as a file path.

---

## 6. Codebase Complexity & Maintainability Opportunities

### 6.1 Golden Byte Hash Pinning Is Overly Brittle for Internal Snapshots & Socket Replies
In `crates/wf-core/src/event.rs`, golden tests pin the wire format of signed `Event`s. That makes sense because `Event` bytes are hashed for `EventId` and covered by cryptographic signatures.
However, `crates/wf-store/src/replica.rs:432` (`golden_snapshot_bytes`) and `crates/wf-daemon/src/proto.rs:246` (`golden_reply_bytes`, `golden_request_bytes`) **also** pin the exact byte length and BLAKE3 hash of:
- The internal `redb` cache snapshot (`State`), which is already guarded by `SNAPSHOT_FORMAT` and automatically refolds on mismatch;
- Every `Request` and `Reply` over the local Unix socket, which is already guarded by `PROTOCOL_VERSION` handshake!
Because `golden_state()` is included in both `replica.rs` and `proto.rs`, **any** change to any field of `State`, `Item`, `Session`, or `Workforce`, or any change to `golden_state()`, breaks tests across multiple crates and requires manually copying magic lengths and base32 hashes into `replica.rs` and `proto.rs`.
- **Recommendation:** Keep golden tests for `Event` / `EventBody` (permanent signed log data). For `State` snapshots and `proto.rs` IPC messages, test round-trip serialization (`encode` -> `decode` == original) instead of pinning exact byte hashes of `golden_state()`.

### 6.2 Double `/bin/sh` Wrapping When Spawning Agents on macOS (`crates/wf-harness/src/run.rs:235-256`, `crates/wf-daemon/src/sandbox.rs:56-67`)
When starting a sandboxed agent on macOS:
1. `sandbox::wrap` wraps the agent command in:
   `sh -c 'cd "$1" && shift && exec "$@"' sh <dir> fence --settings <profile> -- <agent>`
2. Then `wf_harness::run::spawned` wraps *that* in another shell on macOS:
   `/bin/sh -c 'for path in /dev/fd/*; do ... done; exec "$@"' sh sh -c 'cd "$1" ...'`
3. And `Fence` itself spawns `sandbox-exec` (on macOS) which spawns the command!
Combining the directory change and fd cleanup into a single wrapper avoids spawning redundant shell layers on every session and check invocation.

### 6.3 Duplicated Process & Git Config Plumbing in `wf-daemon/src/command.rs` and `wf-vcs/src/git.rs`
Both `crates/wf-vcs/src/git.rs:84-96` and `crates/wf-daemon/src/command.rs:277-285` define identical `unsafe fn inherit_only_stdio(command: &mut Command)` calling `close_fds::set_fds_cloexec(3, &[])`, and both construct `GIT_CONFIG_*` overrides.

### 6.4 Linear Scans in `wf-core` Queries That Will Degrade With Scale
While `State` is fast for small projects, several functions in `wf-core` do repeated `O(N)` or `O(N * M)` scans:
- `State::devices()` (`crates/wf-core/src/state.rs:551-567`): For every bound device, iterates over `workforce.invites.values().any(...)` to compute `invite_redeemed_twice`.
- `State::inbox()` (`crates/wf-core/src/inbox.rs:122-236`): Calls `self.pending_merges()`, `self.finished_reviews()`, `self.review_status()`, `self.recovery()`, and `self.open_blockers()`, each of which scans `live_items()` and `sessions_of(item)` again.
- `State::dependency_graph()` (`crates/wf-core/src/graph.rs:89-122`): Rebuilds the entire dependency graph from scratch (sorting all edges and running DFS `graph.reaches` per edge) on every call: `ready_items()`, `dispatchable()`, `turn_outcome()`, `Snapshot::take()`, `in_view()`. In a single `dispatch_once` pass with $K$ agents, `dependency_graph()` is rebuilt $K$ times.

---

## Summary of High-Priority Recommendations

| Priority | Area | Recommendation |
|---|---|---|
| **P0 (Security)** | `wf-daemon::sandbox` | Remove `~/.claude*`, `~/.opencode/**`, `~/.codex/**`, `~/.gemini/**`, `~/.cursor/**`, and `~/.cargo/registry/**` from `ALLOW_WRITE` (or isolate agent state/config directories while keeping user settings/hooks/registry sources read-only). Otherwise a sandboxed agent can trivially escape the sandbox by writing a hook/plugin or poisoning `~/.cargo/registry`. |
| **P0 (Bug)** | `wf-daemon::runner::start` | Acquire `self.starting` lock for **all** `SessionRole`s (or key by `(ItemId, SessionRole)`), not just `SessionRole::Work`, to prevent concurrent duplicate review/supervise/diagnose session starts. |
| **P1 (Bug)** | `wf-core::target` | Fix `target::chain` root selection and validate `onto` ancestry in `follow_target` so a push on top of a losing merge or a stray `onto` cannot corrupt or wipe the target chain. |
| **P1 (Bug)** | `wf-daemon::runner::end` | In `finish_session`, when `submissions.passed(session)` is `Some(commit)`, use that `commit` directly rather than re-importing from the agent repository after checks finish. |
| **P1 (Bug)** | `wf-core::validate` / `policy` | Fix the `BoundPolicySet` HLC race where any concurrent event between `next_hlc` and `Call::Author` causes `PolicySet` authoring to fail with `HlcNotAfterParents`. |
| **P1 (Perf)** | `wf-store` / `wf-daemon` | Store `Arc<State>` in `Replica` (`Arc::make_mut` on apply) so `daemon.snapshot()` is `O(1)` instead of deep-cloning the entire state on the storage thread. |
| **P2 (Perf)** | `wf-vcs` / `wf-daemon` | Cache `GitCli` paths and check ref file mtime/contents in `run_target` before spawning `git` subprocesses every 5 seconds while idle. |
| **P2 (Perf)** | `wf-core::state` / `wf-cli` | Stop keeping all historical `Event`s in memory in the dashboard (`Feed::events`), and avoid serializing `State::known` (`parents: Vec<EventId>`) over the Unix socket on every 500ms poll. |
| **P2 (Simplicity)** | `wf-core::event` / `policy` | Consolidate the multi-tag legacy wire encodings (`Commented` x4, `SessionEnded` x4, `WrittenState`, `written_states`, `written_steps`) while the project is still pre-v1 (`0.0.0`). |
