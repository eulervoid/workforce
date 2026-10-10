# M1 fixes: plan

Fixes for the findings in `docs/M1-review.md`. They are grouped into work items. Each item is one or a
few small commits and ends with `cargo test` green. The order matters: items 1-3 change how a fork's
history is taken in, and the later items build on that.

## Decisions
* **History is admitted by descent, then folded all at once.** Admission keeps working newest first
  (an event is admitted once something already taken in names it). Admitted history is only recorded.
  When nothing is wanted any more, the whole history is folded in `(hlc, id)` order, which puts every
  event after its parents. Admission and folding are two separate steps.
* **History comes from the local parent store first.** Every device that was in the parent already
  has the events up to the fork point. The forker and the devices that accept the fork take in history
  with the same walk through the parent store (`Replica::history_behind`), and ask peers only for what
  is not on disk. Only a
  device without the parent, or one that was behind the fork point, fetches history over the network.
* **The child store keeps its own copy of the history.** Reading through to the parent store would
  save disk space, but the fork would break if the parent were deleted, and devices without the parent
  would need a second path. Projects are not expected to have so many events that the copy matters.
* **A fork's own events wait for its history.** Until the history is complete, every event of the fork
  except its genesis is parked, and nothing new can be written. Taking the history from a local copy is
  fast, so the wait is short in the common case. This replaces three separate fixes: the lost edits
  (H1), equivocating by writing too early (M6), and history parents being rejected when they are asked
  for (L3).
* **A replica knows which workforce it holds.** A replica is opened for a workforce id and refuses any
  genesis with a different id. The store path already names the id, so no caller has to guess.
* **One principal key per workforce**, at `<data>/<workforce>/principal.key`, next to `store.redb`. The
  device key stays machine-wide in `<data>/keys/device.key`, because the daemon runs one endpoint per
  machine.
* **The daemon owns every store while it runs.** This includes the stores that `wf fork`,
  `wf fork accept` and `wf join` create. These commands go through new socket calls when a daemon is
  running, and use the same `wf-store`/`wf-net` functions in-process when none is.
* **Naming:** "equivocation" for a device that wrote two events with the same `seq`, and "fork" only
  for `wf fork`. "History" for the parent events a fork takes in; drop "prefix".

## Work items

### 1. Rename before changing behaviour (L8, L9, L10, L11)
Only renames and small refactors, so the later diffs stay readable.
* `ApplyPrefix` becomes `ApplyHistory`, `take_in_prefix` becomes `take_in_history`, and
  `NotInForkPrefix` becomes `NotInForkHistory`. Update the M1.md wording to match.
* `AuthorError::OwnFeedForked` becomes `OwnFeedEquivocated`. The `wf device list` flag changes from
  "forked at seq N" to "equivocated at seq N", and the `Feed` docs say "equivocated" instead of
  "forked".
* `apply(.., prefix: bool)` and `fold_body(.., prefix: bool)` take an `Origin { Own, History }` enum.
* `Feed::record(head, prev, extended, prev_was_extended)` becomes a `State` method that reads
  `self.extended` itself. The two boolean parameters go away.
* Make the `EventBody` match in `admit` exhaustive (no `_ =>`).
* Small fixes: move `impl Workforce` next to `Workforce`, fix the `log_len` doc, and move the
  `sync_devices` helper in `network.rs` below the `use` lines.

### 2. Replicas open for a known workforce (H3)
* `wf-core`: `State::new()` becomes `State::expecting(WorkforceId)` plus `State::any()`. `any()` is for
  `wf init` before the genesis exists, and for tests. In `admit_genesis`, an expected id that differs
  from the event id is `Invalid::WrongWorkforce`. The state type itself is unchanged; `expected` is a
  plain `Option<WorkforceId>` field that is not serialized.
* `wf-store`: `Replica::open(store, id)` and `Replica::refold(store, id)`. A stored genesis with a
  different id is `StoreError::WrongWorkforce`. This takes over the checks that `Daemon::workforce` and
  `Workspace::open` each do now.
* `wf fork accept` bootstraps from the devices of the members named in the `ForkAnnounced` when there is
  one, and from every parent device otherwise. With the expected-id check, a lost device can no longer
  poison the store. At worst it wastes one connection attempt.
* Tests:
  - A replica expecting the child refuses the parent's genesis, and then still takes in the real one.
  - `wf fork accept` against a peer that sends the parent's genesis first ends in a usable store.

### 3. Fold the history once it is complete; the fork waits for it (H1, M6, L3)
* `ForkState` becomes an enum, so a fork that is still taking in its history and one that has finished
  cannot be confused:
  - `TakingIn { parent, heads, wanted, admitted: BTreeMap<EventId, Event> }`
  - `Complete { parent, heads }`
* When a history event is admitted, it is recorded in `known`, `feeds` and `extended`, `wanted` is
  updated as today, and the event goes into `admitted`. Its body is not folded yet.
* When `wanted` becomes empty, `admitted` is sorted by `(hlc, id)`, every body is folded in that order
  (with the same rules about what carries over as today), and the state becomes `Complete`.
* `ForkState` is serialized, so a snapshot taken in the middle carries `admitted`. Bump
  `SNAPSHOT_FORMAT`. Replaying the log in arrival order gives the same result, so a restart in the
  middle is safe either way.
* New `Waiting::History`. While the fork is `TakingIn`, `admit` parks every own event of the fork
  except the genesis, before it checks parents or items. When the history is folded, those events are
  released.
  - Events waiting for the history do not count toward `MAX_PENDING`. They are signed by bound
    devices, and the history they wait for is finite. Without this, a fork with more than 10k own events
    could not be taken in.
* `State::builder` returns `Result<EventBuilder, BuildError { NoWorkforce, HistoryIncomplete }>`. Add
  `AuthorError::HistoryIncomplete` with "the history of this fork is still arriving; run `wf sync`".
* `wf fork accept` fails with the same message while the history is incomplete. It does not switch
  `.workforce.toml` in that case.
* Tests:
  - Folded items of the parent and the fork are equal: titles, states, notes, links and archived state
    (`tests/fork.rs`, using the existing scenario plus an edit, a transition and a link).
  - The same equality when the history arrives in batches of any size, and across a restart in the
    middle (with and without a snapshot).
  - An own event of a kept device that arrives before the history is parked and then applied. It is
    not rejected.
  - Authoring while the history is incomplete is refused.
  - `wf-net/tests/fork.rs`: the items equal, not just their count.

### 4. Principal key per workforce (M1)
* `DataDir::principal(&WorkforceId, create)` reads and writes `<data>/<workforce>/principal.key`.
  Remove `replace_principal` and the machine-wide `principal.key`.
* `wf init` generates a fresh principal key for every workforce, so one person has a different key in
  each workforce. Membership is already per workforce, so nothing depends on sharing the key.
* `wf fork`:
  - Without `--new-principal`, copy the parent's key into the child's directory.
  - With `--new-principal`, write a fresh key there.
  - In both cases the key is written *before* the child store, and the child directory is removed if
    anything after that fails.
  - The parent's key stays where it is. It is still the key of the parent, and nothing in other
    workforces is touched.
* Dogfood migration (one-off, by hand, noted in the commit and README):
  `mv ~/.local/share/workforce/keys/principal.key ~/.local/share/workforce/<id>/principal.key`. No
  fallback code, in line with the rapid-development rule.
* Update M0.md (key file locations) and the README.
* Tests: two workforces on one machine have different principals, and `--new-principal` in one leaves
  the other able to `invite device` and `policy set`.

**About agents.** Keeping the key out of the repository protects it from what agents typically touch
(the working tree, commits, sandboxes that are scoped to the repo). It does not protect it from an agent
that runs as the same OS user and reads `~/.local/share/workforce` on purpose. Neither location does.
The real fix is the key-provider hook that M1.md already lists under *Risks*: the OS keychain via the
`keyring` crate, or 1Password or ssh-agent. Every principal-key access goes through
`DataDir::principal`, so that hook replaces a single function. It is not part of this plan.

### 5. History from the local parent store; `wf fork` and `wf fork accept` through the store's owner (M2)
* `wf-store`: `Replica::history_behind(&self, ids) -> Vec<Event>` on the parent, and
  `Replica::take_in_history(&mut self, parent: &Replica)` as the in-process shorthand (call
  `history_behind` with the fork's wanted ids, then `receive_batch` on the child).
  - The walk starts from the ids the fork still wants and follows causal parents by id through the
    parent's store. That walk is the descent rule itself, so events written in the parent after the fork point
    are never reached. The forker no longer takes `events()`, which removes the race with events the
    daemon receives in the meantime.
  - It takes the events in newest first with `receive_batch` in chunks, as `Trust::Stored`: they were
    verified when the parent stored them.
  - Ids that the parent store does not have stay wanted, for the network.
* The forker: `Replica::create_fork(parent, store, genesis)` takes in the genesis, then calls
  `take_in_history(parent)`, and requires the fork to be `Complete` at the end. The forker has every
  event behind its own heads.
* Accepting is an ordinary sync into an empty replica that expects the fork's id:
  - The genesis arrives in the first `Events` batch, together with the fork's own events, which are
    parked (`Waiting::History`).
  - `SyncLog::missing()` takes in history from the local parent store before it returns any ids. This
    applies to both the daemon's storage task and `SharedReplica` in-process. The `Want` phase that
    follows only asks the peer for what is not on disk: history the device lacked because it was behind
    the fork point.
  - When the history is complete it is folded, and the parked events are released.
  - Nothing in the protocol changes. Every later sync goes through the same `missing()`, so history
    that is on disk is never downloaded.
  - The child finds the parent through `fork_point()`. In the daemon, the parent and the child are
    separate storage tasks, so `missing()` asks the child what it wants, reads the walk from the
    parent's task (opening the parent if it is stored but not loaded), and hands the events to the
    child's `receive_batch`. `take_in_history` is therefore split into a read half on the parent
    (`history_behind(ids)`) and the usual write half on the child.
* Socket: `Call::Fork { genesis }` and `Call::Follow { workforce, bootstrap }`.
  - For `Fork`, the daemon runs `create_fork` against the parent's replica, creates the child directory,
    and starts serving the child.
  - `Follow` creates an empty store expecting the id, then syncs with the bootstrap devices; the local
    history comes in as described above. It replaces the `drop(Replica::open(..))` and `SyncWith` pair
    in `accept`.
* The CLI still builds and signs the genesis, because the principal key never goes to the daemon. Then it
  calls `Fork` (with a daemon) or `create_fork` (without one), writes `ForkAnnounced`, and only after
  that switches `.workforce.toml`.
* On failure the child directory is removed, so "nothing was changed" becomes true.
* Tests:
  - `wf fork` while a daemon runs and is receiving events.
  - Accepting a fork with the parent on disk transfers no history over the network (count the events
    in the `SyncReport`).
  - Accepting with a parent that is behind the fork point fetches exactly the missing tail.
  - Accepting without a parent (a device joined to the fork later) still gets the full history.
  - `wf fork` and `wf fork accept` of a 10k-event workforce stay within a time bound (release-only,
    ignored like `scale.rs`).

### 6. `wf join` through the daemon, and join retries (H2, M5, L6)
* Joins become idempotent for the same device:
  - `check_join` allows an invite whose `redeemed_by` is exactly the joining device. Only another
    device makes it `Redeemed`.
  - In the join closure, if this device already has a `DeviceBound` for this invite, nothing new is
    written. The first sync of the join pushes the binding that is already there.
  - `wf join` no longer bails with "already joined" when the device is bound with *this* invite; it
    reruns the join. A binding through another invite still bails.
* Reservations become "served to": after a member has streamed its log to a device, it keeps
  `invite -> device` in memory until the invite is redeemed or expires. Other devices are refused with
  `Reserved`, and the same device can retry. This closes L6 while the daemon runs. A restart forgets the
  entry, which is acceptable for a 24 h code.
* With a daemon running, `Call::Join { code, profile }` runs the join on the daemon's endpoint with its
  device key, and creates the store itself (expecting `code.workforce`). Without a daemon, the current
  in-process path is kept. That is the normal case on a fresh machine, which has no device key yet.
* Tests:
  - Drop the stream after `DeviceBound` is written, then a second `wf join` succeeds and both sides
    agree.
  - A second device with the same code is still refused.
  - `wf join` while a daemon runs.

### 7. Large states and logs over the socket (M3, M4)
* `Call::Events` is answered with a stream: `Response::Events(chunk)` frames (one per 256 events), then
  `Response::EventsEnd`. Add `Call::Log` in the same form, in log order (`log_since(0)`).
* `wf log` replays in log order, into `State::expecting(id)`. With items 2 and 3 this verifies a fork's
  own events and its history. Rows are still shown in `(hlc, id)` order.
* `Call::State`: measure the encoded size at 100k events in `scale.rs`. If an oversized response is
  possible at all, return `Response::Failed("the state is too large for the socket: …")` instead of
  dropping the connection. The typed query API stays the long-term answer (M1.md *Risks*) and is out of
  scope here.
* Tests:
  - `wf log` on a fork shows all events verified (regression for M4).
  - A daemon log larger than one frame arrives whole.

### 8. Network robustness (L1, L2, L5, L4, L12, T1)
* **L1:** `Session::receive` reads each frame with a timeout (30 s), so a stalled peer ends the session
  with `SyncError::TimedOut`. A large sync is not cut off as long as frames keep coming.
* **L2:** in `keep_in_sync`, apply the `RETRY_ON_NEWS_AFTER` guard only when the last attempt failed. A
  heads change during a successful sync leads to one more round, which is what forked feeds need. That
  round changes nothing, so it stops there.
* **L5:** keep an `AbortHandle` per device task, and abort the tasks of devices that are no longer in
  `peers_of`.
* **L4:**
  - `Replica` remembers a failed commit (`needs_reopen`). After each job, the storage thread reopens the
    replica from the store when that is set.
  - When the storage thread ends, `Daemon::workforce` removes the entry from `loaded`, so the next
    request opens the store again.
* **L12:** give `NetError` and `JoinFailure` their source errors (`#[source]` with iroh's connect error
  types, `SyncError`, `FrameError`) instead of `String`.
* **T1:** raise `DIRECT_CONNECT_TIMEOUT` to 10 s. Loopback connects either way, and 3 s is too tight when
  the CPU is saturated.

### 9. Smaller items (L7, L13)
* Write a manual `Debug` for `InviteCode` that redacts the secret.
* Correct the "one feed across both workforces" sentence in M1.md: the fork's genesis continues the
  device's feed, and the parent's `ForkAnnounced` uses the same `seq` in the other workforce.

### 10. Documentation
* M1.md *As built*:
  - Record the history fold (item 3), history from the local parent store (item 5), the
    per-workforce principal key, the new socket calls, and idempotent joins.
  - Replace the "a request for history brings up to 4096 events behind it" detail: the network path
    stays, but it is now only for devices without the parent.
  - Remove the "Not done" entry about believing the first genesis.
  - Correct the "all prefix items intact" exit criterion once the content-equality test passes.
* Delete or archive `docs/M1-review.md` and this file when the work is done.

## Considered and rejected
* **A snapshot in the fork's genesis instead of history.** It would remove the history path entirely,
  but it freezes a state format into the event format, carried-over content would rest on the forker's
  word instead of the original signatures, and provenance before the fork would be lost.
* **Folding history as soon as each event's parents are folded.** Order-independent too, but it needs
  a per-event cascade. With the history taken from a local copy, waiting for all of it is short, so
  folding everything once at the end is simpler.
* **Reading history through to the parent store instead of copying it.** See *Decisions*.
