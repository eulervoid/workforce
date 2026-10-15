# M1 review

Scope: `4015739..6408c0e` (sync, equivocation, device invites, daemon, gossip, fork), compared with
`docs/design/M1.md` and `AGENTS.md`.

State at review time: `cargo clippy --workspace --all-targets` is clean. `cargo test --workspace` passed
except for one flaky failure (see T1). Findings marked **verified** were reproduced, either with a
throwaway test or by running the CLI in a sandbox. The others come from reading the code.

Severity: **High**: loses data or leaves the user stuck. **Medium**: wrong behaviour in realistic
conditions. **Low**: polish, hardening, or consistency.

---

## High

### H1. Fork history drops every edit, transition, comment, link and archive (verified)
`take_in_prefix` folds history events in the order they arrive, and they arrive newest first
(`wf_net::replica::events_by_id`, and the local copy in `wf_cli::fork::fork`, which sorts by
`Reverse((hlc, id))`). `fold_body` applies `ItemEdited`, `ItemTransitioned`, `Comment`, `ItemLinked`
and `ItemArchived` only `if let Some(item) = workforce.items.get_mut(..)`. When they are folded the
item does not exist yet, so they are silently dropped. The `ItemCreated` that comes later then creates
the item as it was first written.

Reproduced with the CLI: `init`, `item add "Original"`, `item edit --title "Edited title"`, `comment`,
`fork`. In the fork, `item show` prints `Original` with no discussion. A throwaway test in
`tests/fork.rs` (the existing scenario already has a comment) showed the parent with 1 note and the fork
with 0.

So the fold is no longer "a function of the set of events" for history. That breaks the exit criterion
"with all prefix items intact", and the as-built claim in M1.md is wrong. The tests miss it because they
only count items (`items().count() == 4`, `== 40`, `== 11`).

Fix: keep *admission by descent* (newest first, which is what makes the security argument work) separate
from *folding*. Record history events in `known`/`feeds` as they are admitted, but fold an event's body
only once all of its causal parents have been folded. A pending-fold queue keyed by the missing parent
works, just like `Pending`. Then the order no longer matters. Add a test that compares the folded items
of parent and fork (titles, states, notes, links), not their count.

### H2. A join that fails after `DeviceBound` is written cannot be recovered
`network::join` authors `DeviceBound` and `DeviceProfile` locally, then pushes them in the second
`initiate`. If that push fails (connection drop, timeout):
- The next bootstrap in the loop runs the author closure again. The device's feed already has seq 1, so
  the new `DeviceBound` gets seq 2. `check_binding` rejects it with `BindingNotFirst`, which surfaces as
  `JoinAuthoring` and aborts.
- Running `wf join` again stops at "this machine has already joined the workforce", because the device
  is bound in its *local* state.
- `wf sync` and the daemon dial the members. They refuse the connection with `FORBIDDEN`, because the
  device is not bound in *their* state. Members never dial the device, because it is not in their
  `peers_of`.

The device is stuck for good, and the invite still looks unredeemed to everyone else. Options: (a) make
the join idempotent by reusing an existing local `DeviceBound` for this invite instead of writing a new
one, and let `wf join` retry while the binding is only local; or (b) let `wf/join/1` accept a device whose
log contains a valid binding for a known invite and take its feed in.

### H3. `wf fork accept` dials the excluded device, which can poison the new store
`fork::accept` bootstraps from *all* devices of the parent (`workspace.state().devices()`), including the
lost device the fork was made to exclude. The documented gap ("a peer that sends the genesis of a parent
before the genesis of the fork is believed") means a hostile peer can send any valid genesis, the
parent's included, into the freshly created empty store. `check_member` then fails, but the store at
`<data>/<child>/` now holds the wrong workforce. The daemon refuses to load it (`stored != id`), and every
later `wf fork accept` fails until the user deletes the directory by hand. The device this can come from
is exactly the one the fork treats as compromised.

Fix both halves:
- Bootstrap only from devices that are not excluded. The announcement lists the members the fork keeps,
  so prefer their devices.
- Let a replica know the id it expects (it is in the store path) and refuse a genesis whose id is
  different. That closes the "Not done" item in M1.md for every path, `wf join` included.

---

## Medium

### M1. `--new-principal` replaces the machine-wide principal key
`DataDir::replace_principal` renames `keys/principal.key`, which is shared by every workforce on the
machine. After a fork with `--new-principal`, every *other* workforce this person owns loses its
principal key: `invite device`, policy changes and forks fail with "the principal key on this machine
belongs to someone else". The order is also risky: the child store is written before the new key is
saved, so a crash in between leaves a fork whose founder key is gone. Store principal keys per principal
(for example `keys/principal-<key>.key`) or per workforce, and write the new key before the genesis.

### M2. `wf fork` writes the child store directly, and can abort half done
- AGENTS.md says the CLI never touches the store directly while a daemon runs. `fork` and `fork accept`
  both open the child's `RedbStore` directly. `fork` holds it open while it writes the history, which
  races with the daemon's 10 s rescan.
- The fork point (`state.heads()`) is taken from the state when the command starts, but the history comes
  from a later `workspace.events()` call. If the daemon took in events in between, they are not in the
  fork point's causal closure. `replica.receive(event)?` then fails with `NotInForkPrefix` and leaves a
  child store with a genesis and part of the history on disk. The daemon picks it up and serves it.
  "Nothing was changed" is also false for the `missing_parents` bail: the directory stays behind.
- The history goes in through `Replica::receive`, one commit per event and every signature checked again.
  At 100k events that is the 17 s path that `receive_batch` was added to avoid.

Suggested fix: send the fork through the daemon when one runs, as a socket call that builds the child
from its own replica. Take the history as the causal closure of the chosen heads, not `events()`. Write
it with `receive_batch`. Remove the directory on failure.

### M3. The daemon socket cannot carry a large state or log
`Call::State` returns the whole encoded `State` and `Call::Events` returns every event, each in a single
frame capped at `MAX_FRAME_LEN = 32 MiB`. The golden event is about 1.7 kB, so a few tens of thousands of
events already exceed the cap for `Events`, which `wf log`, `item show --history` and `wf fork` use. When
that happens the daemon's `write_frame` fails, the connection drops, and the CLI shows a bare I/O error.
`Workspace::author` also fetches the whole state again after every write. Either stream events in chunks,
or add the typed queries the plan mentions under *Risks*. At the least, answer with
`Response::Failed("too large")` so the user gets a clear error.

### M4. `wf log` reports a fork's own events as unverified (verified)
`log` replays `workspace.events()`, ordered by `(hlc, id)`, into an empty `State`. In a fork the oldest
event is the *parent's* genesis, so the replay becomes the parent workforce. The fork's genesis is then
`DuplicateGenesis`, and every event written in the fork is `WrongWorkforce`. Reproduced: `5/6 events
verified`, with the fork genesis unverified. Replay in log order (`log_since(0)`), as `Replica::refold`
does, or start with the fork's genesis.

### M5. `wf join` and `fork accept` start a second endpoint with the daemon's key
`network::join` never checks for a running daemon. On a machine that already runs `wf daemon` (joined to
another workforce), `wf join` starts a second iroh endpoint with the same secret key. Address lookup then
sees two endpoints with one id, and incoming connections can reach either one. Route the join through the
daemon, or refuse while one runs.

### M6. Writing in a fork before its history has arrived equivocates the device
`State::builder` takes `seq`/`prev` from `feeds[author]`. In a fork that is only filled in as history is
taken in. A kept device that runs `wf fork accept` and writes before *its own* prefix feed has arrived
(the sync was interrupted, or the history was larger than one round) writes `seq 1, prev None`. When the
history arrives, its feed has two roots, gets flagged `Equivocated`, and the device can never write in the
fork again. `accept` only checks `principal_of`, which comes from genesis. Refuse to author (and fail
`accept`) while `fork.wanted` is not empty.

---

## Low

### Protocol and robustness
- **L1. No timeout on a running sync.** Connects time out, but `initiate`/`respond` do not. A member
  peer that stops answering mid-session keeps `wf sync` (or one device task in the daemon) waiting until
  QUIC's idle timeout, if that fires at all. Add an overall timeout per session.
- **L2. A heads change during a sync is swallowed.** In `keep_in_sync`, a heads change within
  `RETRY_ON_NEWS_AFTER` of the attempt is consumed (`borrow_and_update`) without a new round. That is
  usually the change the sync itself caused, and also a local write made right after it. Gossip usually
  rescues this, but without gossip the next round comes after 60 s. That matters because convergence of
  forked feeds needs a *second* sync.
- **L3. History arrives in several sessions.** A parked fork event whose `prev` is a history event not
  yet in `wanted` gets that parent rejected with `NotInForkPrefix` when it arrives by `Want`. `asked`
  stops it from being requested again in the same session. It converges over later sessions, but
  `wf fork accept` makes one pass.
- **L4. Storage task failures are not handled.** If a job panics, the storage thread dies and the
  workforce answers `StorageClosed` until the daemon restarts. After a `StoreError` the replica's doc says
  "drop the replica and reopen", but the daemon keeps serving its in-memory state, which may now be ahead
  of the disk.
- **L5. Syncers are never removed.** `run_peer_loop` adds a `keep_in_sync` task per device but never
  removes one. A device whose binding changes keeps being dialed and refused.
- **L6. A leaked invite is not single-use until the binding lands.** The join reservation is released when
  the connection closes, and "redeemed" means "a `DeviceBound` is in the log". Whoever holds a leaked code
  can therefore fetch the full log any number of times until it expires, using a fresh device key each
  time and never writing `DeviceBound`. Consider marking the invite spent locally once its log has been
  served.
- **L7. `InviteCode` derives `Debug` with the secret in it.** Redact the secret in a manual `Debug` impl.

### Design and naming (AGENTS.md)
- **L8. "Fork" names two concepts.** It means equivocation (`OwnFeedForked`, "forked at seq N" in
  `wf device list`, "the feed has forked" in `Feed` docs) and also `wf fork` of a workforce. The plan
  calls the first one `Equivocated`. Following AGENTS.md ("when two concepts compete for one name"), use
  equivocation or "split feed" throughout for the first, and keep "fork" for workforces.
- **L9. "Prefix" and "history" are used for the same thing.** `ApplyPrefix`, `take_in_prefix` and
  `NotInForkPrefix` sit next to `fork_history`, `MAX_HISTORY_PER_REQUEST` and "history" in docs and
  output. Pick one.
- **L10. Boolean flags where an enum exists.** `apply(.., prefix: bool)` and `fold_body(.., prefix:
  bool)` turn `Admission::ApplyPrefix` back into a bool. `Feed::record(.., extended: bool,
  prev_was_extended: bool)` is hard to read at the call site.
- **L11. Wildcard arm on an enum we own.** `admit` matches `EventBody` with `_ if
  content.workforce.is_none()` / `_ =>`, against the exhaustive-match rule.
- **L12. Errors flattened to strings.** `NetError::{Bind, Connect.reason, Gossip, JoinAuthoring}` and
  `JoinFailure::Net(String)` lose the source error. Fine at the socket boundary (`Response::Failed`), but
  not inside `wf-net`.
- **L13. Small inconsistencies.**
  - `impl Workforce` sits between `Known` and `Binding` in `state.rs`.
  - The `EventStore::log_len` doc says the log is "a causal order"; `commit` (correctly) says it is not.
  - The `sync_devices` helper in `network.rs` is defined above the module's last `use` lines.
  - M1.md says "a device has one feed across both workforces", but the fork genesis and the parent's
    `ForkAnnounced` both use seq `N+1` under the same key. That is harmless, because the two never meet,
    but the sentence is not accurate.

### Tests
- **T1. Flaky transport test.** `two_endpoints_exchange_their_writes` failed at `transport.rs:50`
  (`sync_with(..).unwrap()`) in a full `cargo test --workspace` run. It passed 5 times out of 5 when run on
  its own. The likely cause is `DIRECT_CONNECT_TIMEOUT = 3s` while the minute-long proptest loads the CPU.
  Raise the timeout for tests, or run the transport tests serially.
- **T2. Missing coverage.**
  - Fork history content (H1).
  - A join interrupted after `DeviceBound` (H2).
  - `wf log` on a fork (M4).
  - `wf fork` while a daemon runs (M2).
  - `--new-principal` with a second workforce on the same machine (M1).
  - A daemon response over the frame cap (M3).

---

## What holds up well
- Equivocation handling: tips and the `Equivocated` flag are derived from the event set via `extended`
  and the root count, so they do not depend on arrival order. Convergence is property-tested over 1,000
  schedules, including restored snapshots, duplicated and scrambled delivery.
- Invite proofs are domain-separated and bound to `(workforce, invite, device)`. Binding at seq 1 with the
  invite in `deps` keeps "is bound" a question about the causal past. The earliest binding wins on every
  peer.
- `wf/sync/1` alternates who writes, so no buffer can deadlock, and `Want` sizes are capped.
- The member filter sits on every ALPN except join, and the workforce is checked again after `Open`.
- The store moved to an arrival-order log, so snapshot consistency (`log_id(last) ∈ state`) is simple and
  correct. `receive_batch` and the growing snapshot interval give the measured 6.3 s for 100k events.
- The socket is `0600` in a `0700` directory, and protocol versions are checked on both sides.
