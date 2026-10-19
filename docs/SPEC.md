# workforce (`wf`): spec

> A peer-to-peer, CLI-first coordination layer for coding agents and the principals they work for.

This is the design of wf: its concepts, formats, rules and the reasons for them. It describes what
is built; what is not yet built is marked **Planned** with its milestone (section 18). How to use wf
is in the guides (`docs/guide/`), which this spec does not repeat.

## 1. Stages

wf grows through four stages, each trusting more parties than the one before.

| Stage | Who | Machines | Status |
|---|---|---|---|
| **1** | one principal | one machine | built (M0, M2 to M2.3) |
| **2** | one principal | several trusted machines | sync, invites and fork built (M1); sessions across machines next (M3) |
| **3** | a trusted team | several trusted machines | M5 |
| **4** | untrusted participants | several, possibly untrusted machines | M7, a possible end goal |

Stage 4 may never happen. Designs keep it in mind so they do not rule it out, but until then
members and their machines are trusted (section 5).

* **Stage 1.** One developer with agents on their own machine, every day. Nothing to run besides
  the daemon, no network. A session that fails, stalls or is interrupted never leaves an item stuck
  without saying so. A daemon restart costs no work, and no agent context where the harness can
  resume its sessions.
* **Stage 2.** A laptop (sleeps, roams), a cloud VM (always on), a desktop (GPU, behind a NAT), all
  of one principal, each running agents. No port forwarding or VPN; the laptop is often offline and
  merges back when it reconnects. Work goes to the machine that fits, and two agents never silently
  take the same item.
* **Stage 3.** Every change traces back to a principal; review by another principal is a useful
  default; lots of asynchronous "why did we decide X?"; not every transcript is for everyone.
* **Stage 4.** Donated agent work needs revocation, rate limits, sandboxing and probation;
  governance is advisory; prompt injection through item text is a real risk on donor machines.

Across all stages: the only thing peers must agree on is **project state** (items, sessions,
comments, membership, policy). Authorization replicates with it and is checkable offline. "No
infrastructure" means none the principal has to run: public relays and discovery are defaults that
can be swapped. Someone must be online to hold data; that role is the **anchor** (Planned, M3),
named, never required.

## 2. Properties

| # | Property |
|---|---|
| P1 | Zero ops: `init`, `invite`, `join`, work. No server. |
| P2 | Local-first: every peer has a full replica; all reads are local. |
| P3 | Event-sourced: truth is an append-only log of signed events; state is a deterministic fold. |
| P4 | Authorization is data in the log, verifiable offline. |
| P5 | Principals own delegated device keys; every action resolves to a principal. |
| P6 | Deterministic conflict resolution, with an optional coordinator (Planned: the anchor). |
| P7 | Git first, behind one crate, so another VCS can be added later. |
| P8 | Harness-agnostic through ACP. |
| P9 | Forkable: any member can start a new workforce from a frozen point of an old one. |
| P10 | Indirect communication: agents coordinate through items and comments, not messages. |
| P11 | Derivable: all workforce state is derivable from the log plus the repository. What a machine keeps besides (transcripts, check passes) is local and never needed by another. |

## 3. Non-goals for v1

Revocation (fork instead), a recovery key (fork instead), voting or binding governance, jj support
(kept possible, section 10), peer-to-peer repositories (a central origin first), a web UI.

## 4. Architecture

```
wf CLI, dashboard, agents ──unix socket──► wf daemon (one per machine, serves every workforce)
                                            ├─ replica per workforce: redb event store + folded State
                                            ├─ wf-core: admission, fold, policy, lifecycle (pure)
                                            ├─ runner: dispatcher, ACP sessions, merge task, checks
                                            ├─ permission deciders, notifications
                                            ├─ wf-vcs: worktrees, rebase, merge (the only crate that runs git)
                                            └─ iroh endpoint, started once a second device exists
                                                 ├─ wf/join/2   invites
                                                 ├─ wf/sync/3   feed sync
                                                 └─ iroh-gossip heads
```

| Crate | Role |
|---|---|
| `wf-core` | events, encoding, identity, admission, fold, items, policy, sessions, lifecycle, recovery, briefs. Pure: no IO, no async, no clock or randomness except through parameters |
| `wf-store` | the redb event store, snapshots, the `Replica` (store plus state), the questions a sync peer asks of it, binding a device to a workforce |
| `wf-net` | sync, join and gossip over generic streams and a `SyncLog` trait, so convergence is tested in memory; iroh only carries them |
| `wf-vcs` | git |
| `wf-harness` | the ACP client |
| `wf-daemon` | socket, runner, machine config, sandbox, deciders, the storage task that owns each replica and serves it to the network |
| `wf-cli` | the `wf` binary: commands, views, the dashboard |

**The daemon owns the store when it runs.** redb is single-process. The CLI reads and writes through
a `Workspace`, which asks the daemon or, when none runs, opens the store itself. In the daemon one
storage task owns each store and takes writes over a channel. Agents and the dashboard need the
daemon. The CLI fetches the whole folded `State` per command, the dashboard twice a second. The
daemon answers `Call::Events { from }` with a page of the log from a position, which the dashboard
polls with. *Planned:* when folding the whole `State` grows too slow, a local fold cache on top of
it, or a subscription.

### Files

`<data>` is `$XDG_DATA_HOME/workforce` (`~/.local/share/workforce`); `<wf>` is `<data>/<workforce id>`.

| Path | What |
|---|---|
| `<data>/keys/device.key` | the device key, one per machine (hex, 0600) |
| `<data>/keys/principal.token` | the principal token (section 5) |
| `<data>/keys/device-ssh` | the device key as an OpenSSH key, for signing merges |
| `<data>/bin/wf` | the daemon's copy of its own binary, first on the agents' `PATH` |
| `<data>/daemon.pid`, `<data>/daemon.log` | the daemon (`WF_LOG` sets the `tracing` filter) |
| `$XDG_RUNTIME_DIR/workforce/daemon.sock` | the socket (else `<data>/daemon.sock`), 0600 in a 0700 directory |
| `<wf>/principal.key` | the principal key for this workforce |
| `<wf>/store.redb` | the event store |
| `<wf>/repository` | where the main checkout is; every `wf` command run in it refreshes this |
| `<wf>/worktrees/` | worktrees of items and checkouts of sessions (section 10) |
| `<wf>/transcripts/` | ACP transcripts and resume records of sessions (local, never synced) |
| `<wf>/check/`, `check-logs/`, `checks-passed/` | the check worktree, logs, passes |
| `<wf>/sandbox/`, `judges/`, `announced.json` | sandbox profiles, agent deciders, notified inbox entries |
| `$XDG_CONFIG_HOME/workforce/config.toml` | the machine config (section 13) |
| `.workforce.toml` in the repository | only the workforce id, committed so clones find it |

### Socket

Length-prefixed postcard frames (u32 big-endian length, at most 32 MiB). The client sends
`Hello{version}` and a `Credential{token}`; the daemon answers `Ready{version, device, caller}`, or
refuses. The protocol version must match exactly: agents run the daemon's own copy of `wf`
(`<data>/bin/wf`), so rebuilding wf never changes what running agents speak. Requests name a
workforce and a call: read the state, events or a page of the log; author a body; sync now; fork;
join; start, end or stop a session; list and answer permission requests; machine faults.

## 5. Identity and authority

| Concept | Lifetime | Answers | Shown as |
|---|---|---|---|
| **Principal** | permanent | who is accountable | `@ada` |
| **Agent** | long-lived definition | what kind of worker: harness, model, capabilities, lanes, autonomy | `@ada/coder` |
| **Session** | minutes to hours | which agent works on which item, where | `@ada/coder/s4k2` |
| **Device** | long-lived | where things run; mostly invisible | `<handle>@<device name>` in signatures |

**Principal always means the accountable person**: who owns the keys, whom agents act for, who
takes a `principal` step of a review or recovery level, whom blockers escalate to. Code, CLI and docs
say *principal*, never human, user or person. Stages 1 and 2 have one principal.

* **Principal key** (ed25519): signs authority statements (genesis, policy, invites, fork
  announcements). One per workforce, at `<wf>/principal.key`: `wf init` generates it, `wf fork`
  copies it or, with `--new-principal`, makes a new one. A joined device never has it, so it cannot
  invite, set policy or fork. *Planned:* a key provider (OS keychain, 1Password, ssh-agent) behind
  the one function that loads it.
* **Device key** (ed25519): one per machine, never copied. Signs every event and is the iroh
  `EndpointId`.
* **Agents have no keys.** The device signs; every event names an `actor`:

```rust
enum Actor {
    Principal(PrincipalKey),
    Agent { agent: AgentId, session: SessionId },   // AgentId = (principal, name)
    Daemon(PrincipalKey),                            // what a daemon does by itself, for its principal
}
```

Attribution: principal → agent → session → device signature. Actors show as `@ada`, `@ada/coder`
and `@daemon`.

* **Workforce id**: the hash of the genesis event. It identifies but grants nothing, so it is
  committed in `.workforce.toml`.

### Who the daemon acts for

A request never names its actor; the daemon decides it once per connection from the token the client
presents:

* **A session token** acts as that session's agent, in the session's workforce only. The runner draws
  a fresh one each time it starts the session's harness (`WF_SESSION_TOKEN`, beside `WF_SESSION` and
  `WF_WORKFORCE`) and revokes it when the harness exits. It may read, author, sync and end its own
  session: nothing that is the principal's (answer permission requests, start or stop sessions,
  join, fork).
* **The principal token** acts as the principal of the machine's device. The daemon draws it at every
  start and writes it to `<data>/keys/principal.token` (0600), which the agent sandbox cannot read.
  `wf` presents it outside a session.
* No token, or an unknown one, is refused.

What the actor may then author is for `wf-core` to say (guards, below). Decision: a token the
sandbox keeps from agents, not peer credentials or process trees. The uid is the same for principal
and agents, and a pid only says where a process started (`setsid`, double forks and pid reuse defeat
it). The token puts the boundary where the keys already have it: in the sandbox. An agent without
one can read the token, as it can read the keys.

Without a daemon the CLI opens the store itself and takes the actor from `WF_SESSION`, unchecked.

### Roles and guards

| Role | Can do |
|---|---|
| `owner` | everything, including policy and fork announcements |
| `member` | items, comments, sessions, reviews, invites of their own devices |

The founder (named in genesis) is the only owner. Membership comes from genesis seeds only; devices
are added by binding (section 14). Agents act for their principal with a member's rights, minus
**guards**, authoring checks against mistakes and shortcuts. Because the daemon decides the actor,
they hold against an agent in its sandbox, though not against members. An agent cannot:

* write authority or configuration: `Genesis`, `PolicySet`, `InviteCreated`, `DeviceBound`,
  `ForkAnnounced`, `AgentDefined`, `AgentRemoved` (so it cannot raise its own autonomy);
* start a session, end another session, or write `Merged`;
* resolve a blocker its own session wrote;
* lower the review level of an item its agent worked on (a supervisor may), or change a recovery
  level;
* move its item to a done state (it submits);
* give a verdict, unless it is the review session of that commit and its agent never worked on the
  item, or comment on a step from a review session.

A principal cannot `Escalate` (it decides instead).

**Planned (M5):** member invites (`MemberJoined`), more owners (`OwnerAdded`, `wf owner add|transfer`).
Membership then still only grows, so "was the author a member at a causally earlier point?" stays
independent of arrival order.

**Planned (M3): anchor.** An owner may designate one device as anchor. It may only assign work, so a
compromised anchor can reorder assignments, not change policy or roles.

## 6. Event log

Each device writes an append-only, gapless feed; events also name what their author had seen.

```rust
struct Event { content: EventContent, sig: Signature }   // device signature
struct EventContent {
    v: u16,                          // 1
    workforce: Option<WorkforceId>,  // None only in genesis
    author: DeviceKey,
    actor: Actor,
    seq: u64,                        // per author, from 1
    prev: Option<EventId>,           // the author's previous event; None exactly when seq == 1
    deps: Vec<EventId>,              // heads seen when writing, sorted, deduplicated
    hlc: Hlc,                        // wall_ms: u64, counter: u32; tie-breaks only, never authority
    body: EventBody,
}
// EventId = BLAKE3 of the encoded Event, signature included
```

* `hlc` is greater than that of `prev` and every dep, so `(hlc, EventId)` extends causal order.
  The next clock is `max(now, floor)` with the counter incremented; a full counter carries into
  `wall_ms` (checked arithmetic). No admitted `hlc` has `wall_ms == u64::MAX` or
  `counter == u32::MAX`, so every admitted event has a successor. A device whose floor leaves no
  successor refuses to author (`ClockExhausted`) instead of overflowing.
* `deps` records events, not item relations: those (`depends_on`, `supersedes`) are links in bodies.
* **Unknown body tags** are stored, relayed and ignored by the fold, so newer peers can add bodies.
* **Equivocation:** two events with the same `(author, seq)` prove misbehaviour, or a machine
  restored from a backup. Both are admitted, stored, relayed and folded, so peers converge whatever
  they saw first; the device is flagged. A device whose own feed is flagged refuses to author.

### Encoding: postcard

Event ids hash the encoding and signatures cover it, so one value must have exactly one byte string,
stable across releases, and decode fast (a 100k-event snapshot decodes in about 40 ms). We use
**postcard 1.x**, all of it in `wf-core::encoding`.

| Alternative | Why not |
|---|---|
| Deterministic CBOR / DAG-CBOR | The strongest one: standard, self-describing, what AT Protocol uses for signed data. Larger, slower, and needs a strict profile. Choose it if non-Rust readers of the log appear. |
| bincode | Uncertain maintenance; its wire format changed between majors. |
| Protobuf, Cap'n Proto | Not canonical by default; a schema toolchain for a Rust-only system. |
| JSON | Not canonical without JCS; bulky. Used only for `--json` and export. |

Consequences: postcard is not self-describing and field order is part of the format, pinned by
golden tests. A decoder cannot skip an unknown variant, so bodies are wrapped as `(tag, payload)`.
**A body whose layout changes gets a new tag**; the old tag stays decodable, and a payload that lacks
its tag's fields is rejected, so one value keeps one encoding. Ids are always recomputed by
re-encoding, never taken from received bytes (the decoder does not insist on minimal varints).

| Type | Encoding |
|---|---|
| integers | varints |
| strings, bytes, `Vec`, `BTreeMap`, `BTreeSet` | varint length, then elements; maps and sets in key order; hash maps are never encoded |
| `Option<T>` | `0` or `1`, then `T` |
| enums | varint variant index, then fields; variants are append-only |
| ids (`EventId`, `WorkforceId`, `ItemId`, `SessionId`, `CommentId`) | 32-byte BLAKE3 hash, length-prefixed; shown as lowercase base32 |
| `PrincipalKey`, `DeviceKey` | 32 bytes, length-prefixed, not checked as curve points (a bad point never verifies) |
| `Signature` | 64 bytes, no prefix |
| `Duration` | `(secs: u64, nanos: u32)` |
| `CommitId` | `Git(oid)`: variant 0, then the 20-byte oid, length-prefixed |
| names (`Handle`, `AgentName`, `TypeName`, `LaneName`, `HarnessName`, `Label`, `Requirement`, `ReviewLevelName`, `RecoveryLevelName`, `CommandName`) | strings validated on decode: `a-z 0-9 - _` (labels and requirements also `:` `.`), starting with a letter or digit, at most 64 characters |

**Signatures** have four domains: the device signature covers `wf/event/1\0` + `EventContent`; a
principal signature in an authority body covers `wf/authority/1\0` + `(workforce, statement)`, so a
statement cannot be replayed into another workforce; an invite proof covers `wf/invite/1\0` +
`(workforce, invite, device)`; a gossip announcement covers `wf/heads/1\0` + `(topic, digest, time)`
(section 14). The key types are distinct in code for the same reason.

### Bodies

| Tag | Body | Payload |
|---|---|---|
| 1 | `Genesis` | `Signed<Genesis>`: `name`, `founder: MemberSeed`, `members: Vec<MemberSeed>`, `policy` (TOML text), `forked_from: Option<ForkPoint{workforce, heads}>` |
| 2 | `PolicySet` | `Signed<PolicySet>`: `policy` (TOML text, verbatim); not bound to its event, no longer authored (see tag 28) |
| 3 | `AgentDefined` | `name`, `harness`, `model: Option`, `capabilities`, `lanes`, `autonomy{start, nudges, park_timeout, max_duration: Option, permissions}`, `instructions: Option` |
| 4 | `InviteCreated` | `Signed<InviteCreated>`: `kind` (`Device`), `invite: InviteKey`, `expires_ms` |
| 5 | `DeviceBound` | `invite: EventId`, `proof` by the invite key |
| 6 | `DeviceProfile` | `name`, `hardware` (requirements in one string, comma or space separated), `agents: {agent → max_sessions}`, `max_sessions` |
| 7 | `ForkAnnounced` | `Signed<ForkAnnounced>`: `to: WorkforceId`, `members` |
| 8 | `AgentRemoved` | `name`: gone until defined again; its sessions stay valid |
| 10 | `ItemCreated` | `item_type`, `title`, `body`, `priority`, `lane`, `labels`, `requires`, `review` (older, without a state) |
| 11 | `ItemEdited` | `item`, `changes`: `Title`, `Body`, `Priority`, `Lane`, `Labels`, `Requires` (set changes), `ReviewLevel`, `StepDone{step, done}`, `RecoveryLevel` |
| 12 | `ItemMoved` | `item`, `to` (a state name as written), `by_session: Option<SessionId>`, `because: Option<CommentId>` |
| 13 | `ItemLinked` | `item`, `link`: `DependsOn(SetChange<ItemId>)` or `Supersedes(ItemId)` |
| 14, 15 | `ItemArchived`, `ItemUnarchived` | `item` |
| 16 | `Commented` | `item`, `text`, `kind` (`Plain`, `Blocker`, `ChangeRequest`), `resolves: Vec<CommentId>`, `step: Option<StepId>` |
| 17 | `ItemCreated` | as tag 10, plus `state` |
| 20 | `SessionStarted` | `agent`, `item`, `role`: `Work`, `Review{commit: Option}`, `Supervise`, `Diagnose` |
| 21 | `SessionEnded` | `session`, `reason` (section 9) |
| 22 | `Reviewed` | `item`, `commit: Option` (none: submitted without commits), `verdict` (`Approve`, `RequestChanges`, `Escalate`), `because: Option<CommentId>` |
| 23 | `Merged` | `item`, `reviewed`, `commit` |
| 24 | retired | (`Report`, now a comment) |
| 25 | `SessionEnded` | as tag 21, plus `because: CommentId`, the comment that says why |
| 26 | `Commented` | as tag 16, plus `lines: LineAnchor`, the lines of the diff it is about (section 11); a comment without lines is written under tag 16 |
| 28 | `BoundPolicySet` | `Signed<BoundPolicySet>`: `policy`, and the `hlc` of the event that carries it, so the signed statement cannot be written into a newer event to roll the policy back |

* `Signed<T>` = `statement`, `signer: PrincipalKey`, `signature`.
* `MemberSeed` = `principal`, `handle`, `devices`, `previous: Vec<PrincipalKey>` (keys the member
  used in the parent of a fork, so history resolves to the same handle). The founder is `owner`,
  seeds are `member`.
* `SetChange<T>` = `Add(T)` or `Remove{value, observed: Vec<EventId>}`: a removal lists the adds its
  author saw, so a concurrent add survives (add wins) without an ancestry index.
* An item id is the id of its `ItemCreated`; a session id that of its `SessionStarted`; a comment id
  that of its `Commented`.
* Policy travels as TOML text, so the stored policy is exactly what the owner wrote. Peers do not
  parse it to admit it: a policy that does not parse (from a newer version) is flagged in `wf status`,
  and the workforce keeps running on the last policy that parsed, or the default.
* Tags are never reused. *Planned (M3):* `Assigned`, `Claim`, `Heartbeat`.

### Store

One redb file per workforce, `<wf>/store.redb`: pure Rust, one file, one writer with MVCC readers.
The log is the only durable truth; state is folded into in-memory Rust structs and queried as typed
Rust, so changing its shape needs a refold, not a migration. The store sits behind an `EventStore`
trait. Debugging needs no SQL: `wf export | jq`.

| Table | Key → value |
|---|---|
| `events` | event id → encoded event |
| `feeds` | `(author, seq)` → event ids (multimap: several only after equivocation) |
| `log` | position → event id, in the order events were applied |
| `pending` | event id → event waiting for its parents |
| `meta` | `snapshot` → `(format, ref parser version, State)` |

The snapshot is a cache, written every `max(256, events / 8)` events in the same transaction as the
appends it covers. Any mismatch of format or parser version, or with the log, discards it and the
state is refolded. Comment text stays in the log and loads on demand.

**Export** (`wf export`): one JSON line per event in log order, `{id, kind, content, event}`, where
`event` is the signed encoding in base64. **Import** (`wf import`) reads only `event`, so it
verifies everything again; it creates or extends the store (daemon stopped), and does not cover keys
or forks.

### Admission and fold

A rule is safe for peers to enforce only if its answer cannot depend on arrival order. So rules come
in two kinds.

**Admission** (`State::receive`, every peer; depends only on the event's causal past): version;
device signature; workforce; all parents known; `prev` is the author's `seq - 1`; `hlc` above every
parent's and neither part at its maximum; the author is a bound device and the actor belongs to its
principal (a `DeviceBound` as the author's first event, with its invite among its deps and a valid
proof, binds it); an agent actor is a defined agent and its session belongs to that agent on that
device; authority bodies carry a valid principal signature of the event's actor (an owner's for
`BoundPolicySet`, `PolicySet` and `ForkAnnounced`, a member's for `InviteCreated`); a
`BoundPolicySet` names the `hlc` of its own event; an unbound `PolicySet` (tag 2, written before the
bound tag existed) is still admitted, so stored logs keep loading; the fold, not admission, keeps it from
rolling a bound policy back (below); genesis is signed by its founder
from a founder device, with unique principals, handles and devices; referenced items, sessions and
agents exist; no item links to itself.

* **Clock tolerance:** a peer's event whose `hlc.wall_ms` is more than one hour ahead of the
  receiving device's clock (`State::receive_at` takes `now_ms`; wf-core reads no clock) is
  **refused, not pending**, before its parents are looked at: it is not stored, nor is any event
  that builds on it, and nothing wakes it as time passes. Sync offers it again, and it is admitted
  once the clock has caught up. An event already waiting is never refused by a later clock. This
  check depends on the receiver's clock, so it is the one admission rule that is not a function of
  the causal past; the maxima above are, and are refused for good. Replay from the local store
  applies no tolerance.
* Events with missing parents wait in a **pending set** (at most 10,000, including events waiting
  on a fork's history, which first need a bound device of their actor's principal, or a
  `DeviceBound` at `seq` 1; a device bound only later is refused, and sync offers its events again once
  the history is complete; history events need consistent `seq`/`prev`) and are applied when their parents arrive. Any arrival order and duplication
  give the same state, tested with generated event sets and a simulation of 1,000 sync schedules.

**Authoring checks** (`check_policy` and `check_actor`, run by the author before signing; peers do
not repeat them, because a concurrent policy change would make acceptance order-dependent): the guards
of section 5; the type exists and uses the state; a new item starts in its type's initial state or as
a draft; fields allowed for the type; the lane exists; `depends_on` target type allowed; no dependency
cycle; archive only from `done` or `cancelled`; `supersedes` only between decisions; verdict rules
(section 10). The fold keeps whatever a peer wrote and flags what no longer fits (an unknown type, a
state its type no longer uses).

**Fold:**
* Concurrent writes order by `(hlc, EventId)`.
* Last-writer-wins registers: title, body, state, priority, lane, review level, recovery level, each
  step's tick, `supersedes`, archived.
* Observed-remove sets, add wins: labels, requires, `depends_on`.
* `depends_on` cycles from concurrent links are resolved at query time: edges in `(hlc, EventId)`
  order, an edge that would close a cycle is ignored and flagged.
* Comments keep author, kind, resolutions, step and the tokens parsed from the text; the text stays
  in the log. Resolving is grow-only, so concurrent resolutions converge. `#refs` and `@mentions` are
  resolved at query time, since resolution depends on what is known.
* Handles come from genesis seeds; a device bound twice resolves to its earliest `DeviceBound`.

**Older events and policies** are read as they were written, since events are signed: an
`ItemCreated` without a state starts in its type's initial state under the current policy; old state
names map to the fixed states (`backlog` → draft, `in_progress` → active, `in_review`/`proposed` →
review, `ratified`/`superseded` → done, `dropped`/`rejected` → cancelled; an unknown name reads as
draft and is flagged); a `supersedes` that wrote `superseded` into its target's state leaves the target
in that state; policies with `{name, category}` states, `transitions` and `"human"` steps are read,
but `wf policy apply` and `edit` refuse them.

### Planned: `seen` and compaction (M6)

History matters most for recent and active items. Compaction lets a device drop old events without
changing the folded state; keeping everything stays valid. Every merge rule is commutative,
associative and idempotent, and the state keeps the stamp of every register and set tag, so an item's
folded state is a valid base for any later event, even one arriving months late.

* **`seen`.** `deps` becomes `seen: Vec<EventRef{author, seq, id}>`, so a peer can tell whether a
  missing parent lies below its frontier. This changes every event id and lands first.
* **Item checkpoints** (a new tag): an ordinary event carrying an item's folded state with every
  stamp, add tag and comment text. It covers every event of the item in its causal past, a version
  vector since feeds are chains. Applying it merges by stamp; a set tag the checkpoint covers but
  lacks is removed. A peer holding the covered events refolds and compares, flagging a mismatch
  (a flag, not admission: a joiner cannot check). The anchor writes checkpoints for items quiet for
  the policy's `[compaction] after`; duplicates are harmless. Equivocating feeds are never covered.
* **Frontier**: a version vector below which a device dropped events. It may rise when everything
  below is covered or kept: authority bodies, the newest checkpoint per item, uncovered events and
  unknown tags. A parent at or below the frontier counts as known; equivocation below it goes
  unnoticed (accepted). Sync sends kept events plus the frontier for the gap, and a joiner starts from
  the frontier of the peer it joins through. Each device decides how far to compact; one that never
  does is an archive, and makes forks from before a frontier possible. P11 then reads: the log means
  the kept events plus everything above the frontier.
* Tested: for any cut, checkpointing and arrival order, checkpoints plus later events fold to the
  same state as the full log.

## 7. Forking

A fork is a **new genesis that points at a frozen point of the parent's history**
(`forked_from: ForkPoint{workforce, heads}`). It replaces revocation and recovery keys in v1.

* The genesis continues the forker's device feed (`seq` = its head + 1), and the fork point is the
  forker's current heads. The policy text is copied.
* **History is admitted by descent.** A parent event is admitted unchecked when its id is wanted:
  named by the fork point, or a parent of history already admitted. The id is the proof. Once nothing
  is wanted, the history is folded at once in `(hlc, id)` order. Parent events after the fork point
  never get in. The fork's own events wait until the history is complete, and the device refuses to
  author before that, so it cannot equivocate against its own history.
* What carries over: items, comments, links, the agents of kept members and the profiles of kept
  devices. What does not: policy, invites, bindings, sessions, reviews and merges.
* Excluded members and devices are absent from the new seeds, so their events are not admitted and
  their sync connections are refused once they name the fork. A lost machine is excluded as a device;
  the forker cannot exclude itself.
* `--new-principal` founds the fork with a fresh principal key, for when the lost machine held it;
  the seed lists the old key as `previous`.
* Any member that holds the principal key on the machine can fork and becomes the founder. An owner
  forking without `--new-principal` writes `ForkAnnounced{to, members}` in the old workforce. Devices
  follow by hand: `wf fork accept <id>` fetches the history from the kept devices, then rewrites
  `.workforce.toml`.

*Planned:* following a fork automatically once a fork is not a security event; asking first for
non-owner forks; archiving the old workforce locally; importing a fork from an export.

## 8. Items

The item is the single universal document: tasks, decisions and meta work are items, and differ only
in their type's configuration, so comments, references, mentions and the inbox work the same for all.
A question is not an item but a blocker on the item it is about (section 11).

### States

Every item is in one of six fixed states. Built-in behaviour keys on them; a type lists which it uses.

| State | Meaning | Behaviour |
|---|---|---|
| `draft` | held back while it is thought through | nothing starts it |
| `open` | actionable | ready once dependencies are done and no blocker is open |
| `active` | a work session holds it | returns to `open` when the session ends without submitting |
| `review` | submitted work waits for its review steps, or a proposal waits for a decision | not claimable |
| `done` | finished | terminal; satisfies dependents |
| `cancelled` | finished without success | terminal; open dependents become stranded |

Items show as `task:open`, `decision:review` and so on. The daemon moves items into `active`,
`review` and, after the merge, `done`. A principal moves them with verbs: `wf item open` (release a
draft, reopen), `draft`, `done` (finish without a session), `cancel`. Nothing moves an item by hand
into `active` or `review`: an `active` item without a session would never return to `open`, and a
review needs a submitted commit. `wf item add --draft` creates a draft in the one `ItemCreated`, so the dispatcher never sees
it open.

**Readiness** is computed for open, unarchived items, never stored:

| Readiness | When |
|---|---|
| `stranded` | some dependency is `cancelled`: waiting will not free it; someone removes the dependency or cancels the item |
| `blocked` | some dependency is not done, or a blocker on the item is open |
| `claimed` | otherwise, and a live work session holds it |
| `ready` | otherwise |

`wf status` flags stranded items, items in a state their type no longer uses, and unknown types.

### Types

A type is configured in the policy (section 15) with its `lane`, its `fields` (any of `priority`,
`labels`, `requires`), the types it may `depends_on`, its `states` and `initial` state, and default
`review` and `recovery` levels. The `states` list keeps a decision from being moved to `open`, where
an agent would take it. A type that uses `active` must use `open` to return to; one with a review
level with steps must use `review`. If a policy change drops a state items are still in, they keep it,
`wf status` flags them, and only moves into it are refused.

Built in, shipped in the default policy, names reserved:

| Type | Initial | States | Purpose |
|---|---|---|---|
| `task` | `open` | all six | work for agent sessions; `review = "manual"`, `recovery = "standard"` |
| `decision` | `review` | review, done, cancelled | a proposal, accepted with `wf item done`, rejected with `wf item cancel`; objections are change requests |
| `distill` | `open` | open, active, done, cancelled | meta work in the `meta` lane |

**Planned (M4):** only decisions that are `done` and not superseded enter agent briefs; distilling
threads into decisions is ordinary work in `distill` items, so there is no separate scribe.

### Priority

A field only of types that list it (by default `task`): `P0` (drop everything), `P1` (this cycle),
`P2` (normal, the default), `P3` (when nothing else waits). The author check requires one where the
type has the field and refuses one where it has not; an item whose type gained the field later sorts
last until one is set. `item list` orders by priority, then creation, then id (`--sort`, section
16). *Not decided:* inheriting the highest priority of the items an item blocks.

### Relations

| Relation | Source | Semantics |
|---|---|---|
| `depends_on` | `ItemLinked`, many, acyclic | readiness; allowed target types per type |
| `supersedes` | `ItemLinked`, one, decisions only, not removable | the target keeps its state and shows as superseded |
| references | `#k3f7a2` or `#k3f7a2/c7f2` in body and comments | backlinks to an item or a comment |
| mentions | `@ada`, `@ada/coder` in body and comments | `wf mentions`; a blocker goes to the first one it names |

* No parent/child: decomposition is "the big task depends on its parts", which makes it ready for
  integration exactly when the parts are done. `item show` lists dependents as "required by".
* `depends_on` is never derived from text: a stray `#ref` must not block work.
* References and mentions are parsed with `pulldown-cmark`, skipping code, by a versioned parser in
  the fold; a better parser means a refold. Item ids take any unique prefix of at least 4 characters.
  A handle wins over an agent name; a short `@coder` resolves when exactly one agent has the name.

**Planned (M3):** mentions are delivered: `@ada` to the principal's inbox, `@ada/coder` on an item to
the agent's live session there or else the agent's inbox, `@ada/coder/s4k2` to a session.

### Archiving

`archived` is a flag beside the state, allowed only in `done` or `cancelled`, so an archived item
keeps its outcome. Archived items are hidden from lists and briefs (`--archived` shows them);
dependencies on them still resolve. The fold keeps them whole: dropping bodies at fold time would make
the state depend on the order of archive, unarchive and edits. Reclaiming their space is compaction.

### Stored and computed

| Stored (events) | Computed in the fold | Computed per query |
|---|---|---|
| type, title, body, state, priority, lane, labels, requires, review and recovery level, step ticks, `depends_on`, `supersedes`, archived, comments | author, created, updated, superseded by, references, mentions, open blockers and change requests, sessions, flags | readiness, claim holder, session liveness, review status, recovery step, inbox |

The fold never reads the clock; whatever depends on "now" is computed per query.

### Routing

| Axis | Question | Mechanism |
|---|---|---|
| scope | who can *see* it | *Planned (M5):* replication filter; v1 has one scope |
| lane | who may *take* it | an item is in one lane (its type's unless set); an agent takes items from the lanes it lists |
| requires | who is *able* to | the item's or the review step's `requires` against the agent's capabilities plus the device's hardware |

Lanes route; they are not security. The `supervision` lane holds no items: its agents answer
blockers. Judgment about what to work on lives in items, not the dispatcher: planner and supervisor
agents create, split, order and unblock items, and the dispatcher follows the data.

## 9. Sessions

A **session** is one attempt by one agent on one device at **exactly one item**, in one role:

| Role | Does | Runs in |
|---|---|---|
| `Work` | does the item; holds its claim | the item's worktree, on branch `wf/<item>` |
| `Review{commit}` | takes one review step on one submitted commit | a detached checkout of that commit |
| `Supervise` | answers or escalates blockers waiting on its agent | an empty directory |
| `Diagnose` | says why earlier attempts failed (a recovery step) | an empty directory |

Items are sized for a session: steps inside one are a checklist in the body. Work is split into
items only where parts can run in parallel, need another review level, or other capabilities.

### Agents and devices

An agent is defined once per principal in the log (`AgentDefined`), with a harness, an optional
model, capabilities, lanes, instructions told in every brief, and its **autonomy**:

| Field | Default | Says |
|---|---|---|
| `start` | `manual` | `auto`: the dispatcher starts it; `manual`: only `wf session start` |
| `permissions` | `allow` | `allow`, `ask` or `deny` the harness's permission requests (section 12) |
| `nudges` | 3 | turns in a row without a new commit before the session is released |
| `park_timeout` | 2h | how long a parked session waits |
| `max_duration` | none | the longest a session runs |

Agents are named by role, not harness; names are unique per principal and immutable, so mentions
keep resolving. A new `AgentDefined` replaces the definition (last writer wins); `AgentRemoved`
tombstones it until it is defined again. A device signs definitions, but agents cannot write them.

A device publishes a `DeviceProfile`: its name, hardware (`os:linux, arch:aarch64, gpu`), how many
sessions it runs at once (`max_sessions`, default 1, parked ones included), and which agents it hosts
with how many sessions each. Defining or editing an agent with `start = auto` makes this device host
it with one session.

### Lifecycle

* `SessionStarted{agent, item, role}`, written by `@daemon`. A work start moves the item to
  `active` (`ItemMoved{by_session}`).
* `SessionEnded{session, reason, because}`. `because` names the comment that says why, written just
  before (the agent's note, or how it failed).

| Reason | Ends when |
|---|---|
| `Submitted{commit}` | the agent ran `wf work submit` |
| `Reviewed`, `Supervised`, `Diagnosed` | the agent gave a verdict, resolved a blocker, wrote its diagnosis |
| `Released` | `wf work release`, or the nudges ran out |
| `TimedOut` | `park_timeout` or `max_duration` passed |
| `Failed` | the harness failed or went silent |
| `Cancelled` | the principal stopped it, or its item moved on (cancelled, done, drafted, archived) |
| `Interrupted` | the daemon stopped and could not resume it |
| `MachineFault` | `LoginLapsed`, `NoSpace` or `HarnessUnreachable`: the machine, not the attempt |

**Ending moves the item first.** `after_end` in `wf-core::lifecycle` says what happens to the item:
back to open, to review, merge then done, or nothing. The daemon writes that move while the session
still holds the item (as `ItemMoved{by_session}`), then `SessionEnded`, so the dispatcher never sees
an open item without its session in between. Only work sessions move items, and a stale end never
reopens an item that moved on. Ends are split by who decides them: a `Finish` (submit, release,
verdict, answer, diagnosis) is the agent's, and waits up to 20 s for its turn to end, since the
command it ran waits for the answer; an `Interruption` (stop, moved on, timeout, failure, giving up,
machine fault) is the daemon's and acts at once.

**Claims.** On one machine the claim of an item is its earliest live work session; starts take a
per-item lock and refuse an item with a live work session. Across devices (*Planned, M3*) liveness
comes from a lease: a session without renewal counts as dead, its claim expires and the item can be
assigned again; of two concurrent claims, the earliest by `(hlc, EventId)` wins and the other cancels
itself.

One branch per item: every work session on it (after a release, a crash or requested changes)
continues `wf/<item>`. Context carries over within a session in the conversation, and across sessions
through comments. Reassignment carries only what was committed, which is why agents commit small.

### Dispatch

Each daemon dispatches for the agents its device hosts, woken by new events, ended sessions and a
30 s tick. A round first queues approved work for the merge task and finishes approved work without
commits. Then, for each agent with `start = auto` and a free slot, it starts the first of:

1. a **review** of an item waiting on a step the agent can take: its capabilities meet the step's
   `requires`, and it never worked on the item;
2. a **supervise** session for a blocker waiting on the agent (section 11), on an item it never
   worked on and without a live supervise session;
3. a **diagnose** session for an item whose recovery level asks for one the agent can take;
4. a **work** session on the first ready item in its lanes whose `requires` the agent and device
   meet, in dispatch order.

Every start also needs the item's recovery level to allow another attempt of that role. Agents whose
harness is at fault wait, all agents wait while the disk is full, and a start that failed is not
retried for that item and agent for 10 minutes.

**Dispatch order** finishes started work before starting new work, so branches do not go stale.
Priority first (`P0` first, none last); within a priority: work sent back by review with change
requests still open, then other started work, then work never started; within each, fewer unchecked
steps first (more checked steps first when equal), then oldest, then id. A plain sort key, no weights.
`wf item list --ready` keeps its own order; `--for <agent>` applies the dispatcher's filter.

`wf session start <item> [--agent]` starts a work session by hand, for any agent (by default the
policy's `default_agent`). Reviews, supervision and diagnosis need hosted `start = auto` agents.

**Planned (M3): across devices.** The dispatch target stays an agent; any device that hosts it with a
free slot may run it. A device with capacity asks the anchor, which signs `Assigned{item, agent,
device, lease}`; the device starts the session and renews it with `Heartbeat`. Without a reachable
anchor, a policy `fallback` decides: `optimistic` (start with an unconfirmed `Claim`) or `wait`. Until
then, two daemons do not coordinate, so each agent should be hosted on one machine.

### Briefs

The first prompt is the brief (`wf work brief` prints it again): the agent's instructions, the item
with its numbered steps, and every earlier comment with its kind. By role it adds:

* **Work:** dependencies with their last comment; open change requests, each to resolve; the rules:
  commit small on the branch, tick steps (`wf item check`), report per step (`wf comment --step
  [--done]`), run `wf check` before submitting when the policy has checks, ask with a blocker and end
  the turn, and end with `wf work submit --note` or `release --note`.
* **Review:** the commit and its diff (inline up to 60 kB), the step in words, change nothing,
  request changes even when small, give exactly one verdict. Tests the sandbox cannot run are left to
  the checks.
* **Supervise:** the blockers waiting on the agent: answer with `--resolve`, or escalate with a
  blocker mentioning the principal that resolves the old one.
* **Diagnose:** the attempts that did not finish, with their reasons and last words.

### Turn end

When a prompt turn returns, the runner checks, in order (`State::turn_outcome`):

| Situation | Next |
|---|---|
| the session ended | close the harness |
| a work session's item moved on (review, done, cancelled, draft, archived) | end `Cancelled` |
| the machine is at fault | end `MachineFault` |
| a work session's item has an open blocker or an unfinished dependency | **park**: keep the harness and its conversation; once the item is ready, prompt again with the comments written meanwhile; end `TimedOut` after `park_timeout` |
| `nudges` turns in a row without a new commit on the branch (for other roles: without a verdict, answer or diagnosis) | end `Released`, by `@daemon`, with the agent's last words |
| otherwise | nudge: a role-specific "continue"; with a rebase stopped by a refused submission, name its files |

### Recovery levels

What happens when sessions end without finishing is configured like review: an ordered list of
levels in the policy, each a list of steps, one taken per unsuccessful attempt. A type names its
default level; an item can name another (`--recovery`).

| Step | Does |
|---|---|
| `"retry"` | any agent that can take the item may start again |
| `{ agent = { requires = [...] } }` | a diagnose session says what went wrong before the next step (for work; for other roles it acts as a retry) |
| `"principal"` | the item waits in `wf inbox` with how the last attempt ended |

* **Attempts:** a session ending `Released`, `TimedOut` or `Failed` takes the next step, as does a
  finished diagnosis. `Interrupted`, `Cancelled` and `MachineFault` never do: they say nothing about
  the attempt.
* **Computed, not stored** (`State::recovery(item, role)`): the step is the number of unsuccessful
  attempts of the role since the item last moved on (work: since the last submission; review: since
  the submission and its latest verdict; supervise: since the oldest open blocker), and always since
  the principal last acted on the item (created, edited, moved, linked, commented on or reviewed it).
  So the level can change at any time and applies at once, and the principal's action starts it over.
* After the last step, or without a level (decisions), the principal decides. Starting a session by
  hand is not held back by recovery.

### Resume and faults

* **Restart.** A daemon that stops cancels its sessions' turns and closes their harnesses, but leaves
  the sessions live. It keeps, beside each transcript, the agent's ACP session id, the time of the
  last prompt and the model. The next daemon resumes each session in the agent's own conversation
  (ACP `session/resume`, else `session/load` with the replayed history left out of the transcript): a
  working session is told that wf restarted and what was written on its item since its last prompt,
  a parked one parks again, and `max_duration` counts from the resume. A session whose item moved on
  ends `Cancelled`; one the agent cannot resume ends `Interrupted`, which costs no recovery step.
* **No silent stalls.** A harness has 3 minutes to start, else `HarnessUnreachable`; a turn silent for
  20 minutes with no permission request pending ends the session `Failed`; ACP `AuthRequired` is
  `LoginLapsed`; a write that fails for lack of space is `NoSpace`. A machine fault ends the session
  without using a recovery step, holds the harness's agents for 10 minutes (all agents while the disk
  is full, probed each round), and shows once in `wf inbox` as `machine_fault` rather than as a
  comment on every item. A session end that cannot be written is retried.
* **Storage.** Daemon start removes stray worktrees. With a `[storage]` budget in the machine config,
  the build caches of the least recently built idle worktrees are deleted until the rest fits.

## 10. VCS, review and merge

* Git only, through the `git` CLI (2.38 or later), run by `wf-vcs` alone, always with
  `core.hooksPath=/dev/null`, `core.fsmonitor=false`, `LC_ALL=C` and no terminal prompt. Commit ids
  are a `vcs` enum with one variant, so jj can be added without a format change.
* One worktree and branch per item: `wf/<item>`, where `<item>` is the first 12 characters of the item
  id. Worktrees live in `<wf>/worktrees/<item>`, outside the repository, and are removed after the
  merge, when the item is cancelled, or at once when a session submits without commits. A review
  gets a detached checkout in `<item>-<commit prefix>` (an empty `<item>-answer` for a submission
  without commits), removed when it ends.
* Whichever worktree it is given, the adapter works on the repository's main checkout, found through
  `<wf>/repository`.

### Review

Review levels are an ordered list in the policy, least strict first. Each is a sequence of steps; a
step is an agent matched by capabilities, or the principal. Default levels: `none` (no steps),
`light`, `standard`, `critical` (an agent, then the principal) and `manual` (the principal alone, the
default for tasks).

* An item's level is its own `review` or its type's; `--review default` goes back to the type's, so
  `default` is not a level name. A type or item without steps is not reviewed.
* **Submitting** (`wf work submit`) is refused while a change request is open, or with uncommitted
  changes. It first **rebases** the branch onto the target, so the reviewer sees the commits as they
  will land; nobody has approved them, so rewriting costs nothing (the rebased commits are signed as
  agents' commits are). A conflict refuses the submission with the files: the rebase stays stopped in
  the worktree, the agent resolves the files and submits again, and the daemon continues the rebase.
  A session that ends without submitting abandons the rebase. Once the rebase is through, the session
  ends `Submitted` and the item moves to `review`.
* Steps run in order. The status (`Awaiting{step}`, `ChangesRequested`, `Approved`, `Merged`) is
  computed from the latest submitted commit, the level and the verdicts on that commit. A new
  submission starts the level over; what was reviewed is what gets merged.
* **Verdicts** (`wf review <item>`; in a review session it also ends the session `Reviewed`) name the
  commit and are refused on any other one:
  * `Approve` moves on to the next step; refused while a change request is open. A principal step is
    satisfied only by a principal's verdict; a principal's `Approve` may also stand in for an agent step.
    An agent verdict that cannot satisfy the current step (an `Approve` or `Escalate` on a principal
    step, as from review sessions started concurrently on two devices) is ignored, not counted for a
    later step.
  * `RequestChanges` needs an open change request (written first, one comment each) and moves the item
    back to its open state in the same event. The next work session continues the branch and resolves
    them; a resolution the reviewer does not accept gets a new change request.
  * `Escalate` (agents only) inserts a principal step after the current one.
  * A verdict may carry a plain comment as its `because`.
* Before a review step is offered the daemon builds the merge of the work into the target
  (`git merge-tree`). Work that conflicts goes back to its open state with a change request from
  `@daemon` naming the files. If the work merged when offered and stops merging while the principal
  waits, it stays in the inbox, with the same change request; the entry says since when it is stale.
* Principal steps wait in `wf inbox` as `principal_review`. `wf item diff` shows the work through git
  (the principal's pager and diff settings apply); `wf item checkout` puts the main checkout on it,
  detached, since the branch is checked out in the item's worktree.
* When the last step approves, the item is merged and then done, which unblocks its dependents. An
  item someone moved after the submission is left where it is.

*Planned:* `wf review --annotate`, turning annotations made in a diff tool (such as lumen) into change
requests; a comment naming a file and lines as data.

### Merge

`[merge] mode = "local"` merges approved work into `[merge] target` in this repository. One **merge
task** per workforce merges one item at a time, outside the dispatch loop; the dispatcher and session
ends only queue work for it. Work without a review level waits in `review` while queued.

1. Build the merge with `git merge-tree --write-tree`, touching no checkout.
2. Run the policy's checks on that tree (below).
3. Commit it (`Merge #<item>: <title>`) with `git commit-tree`, and advance the target: fast-forward a
   clean checkout of it, or compare-and-swap with `git update-ref`. A target that moved meanwhile is
   tried again (3 times, then one comment).
4. Write `Merged{item, reviewed, commit}`, remove the branch and worktree, move the item to done.

* A **dirty checkout** of the target makes the merge wait: one comment naming the files, a `merge`
  entry in `wf inbox`, and a retry on every change and tick. Untracked files do not count.
* A **conflict** sends the item back to its open state with a comment naming the files; approved work
  is never rewritten. The next session's submission rebases it.
* Any other failure comments once and leaves the item in `review` to retry. A reviewed commit that
  already landed only moves the item to done.

**Checks.** `[[merge.check]]` commands (section 15) must pass on the merge result before the target
moves. They run in the workforce's one check worktree (`<wf>/check`, cleaned before each run, build
caches warm), one after another in the policy's order, all of them even after a failure, each without
a shell, for its timeout at most (capped by the machine config's `[check] max_timeout`). They get the
agents' environment without `CARGO_TARGET_DIR` (a shared target could give a false pass), the global
git config with signing off, and the check sandbox of the machine config (section 13). Each run's
output stays in `check-logs/<name>/`. A pass is kept per tree and command on this machine, so a check
does not run twice on the same tree.

* **All pass:** the merge commit is made from that tree; if the target moved, the new merge is
  checked again.
* **A check fails:** it runs on the target's own tree too. If it passes there, the work broke it: a
  change request by `@daemon` per such check (name, command, tree, the end of the output, the log's
  path) and the item goes back to open. If it fails there too, or timed out, it is not the work's
  fault: a blocker to the principal, the item stays in `review`, and resolving the blocker checks the
  merge again. Mixed results send the item back for the work's failures and tell the principal of the
  others.
* After two failed runs since the principal last acted on the item, or when its type has no open
  state, failures go to the principal instead of back to work.
* A check's result is about one machine's toolchain, so nothing else goes into the log.
* `wf check [<name>...]` runs the checks on the checkout it is run in, as it is, and prints a mark per
  check with the end of each failure's output: in a session in the session's sandbox, otherwise in
  the check sandbox. Agents are told to run it before submitting; the daemon's run is the one that
  counts.

**Planned:**
* `mode = "origin"` (M3): wf pushes approved branches and opens pull requests; the origin decides
  merging (branch protection), and wf writes `Merged` when `wf/<item>` lands in the target.
* `[[merge.after]]`: a command after a merge (release, deploy), run only by the daemon that merged,
  outside the sandbox with secrets from the broker. Its failure goes to the inbox and never reopens the
  item. wf does not become a CI system: no jobs, artifacts or matrices.
* Git bundles over iroh-blobs for peer-to-peer repositories (M6); `git-remote-iroh` as prior art.

### Commit identity and signatures

* An agent commits as itself: the daemon sets `GIT_AUTHOR_NAME` and `GIT_COMMITTER_NAME` to
  `@principal/agent`, with the email of the git config. `SessionEnded` says which session made a
  commit.
* Agents commit unsigned by default (`[commits] agent_signing`), so they never wait for a key the
  principal has to unlock; `as_configured` lets the git config decide. Git config reaches agents only
  through `GIT_CONFIG_*` variables.
* The merge commit (`[commits] merge_signing`) is signed as the git config says (the default; with
  signing configured, the principal's key says "I accepted this"), not at all, or with the device key
  (`device`: "this device of the workforce merged it", carrying out an approval the log records). The daemon signs through
  `ssh-keygen` with `<data>/keys/device-ssh`; `wf device allowed-signers` prints every bound device as
  `<handle>@<device name>` for `git verify-commit`. An SSH signature covers a blob starting with
  `SSHSIG` in namespace `git`, so it never collides with an event signature.
* *Planned:* a signing key per session, readable only by its sandbox, with its public key in
  `SessionStarted`, so a commit's signature names the session, not just "some agent of the machine".

## 11. Comments

All text about an item is a comment: discussion, questions, change requests, the reasons for a move
or a verdict, and what the working agent reports. There is no second kind of note.

```rust
struct Comment {
    item: ItemId,
    text: String,
    kind: CommentKind,              // Plain | Blocker | ChangeRequest
    resolves: Vec<CommentId>,       // blockers and change requests this comment resolves
    step: Option<StepId>,           // the step it is about: the hash of the step's text
    lines: Option<LineAnchor>,      // the lines of the diff of the item's work it is about
}

struct LineAnchor {
    commit: CommitId,               // whose diff against the target the lines are of
    path: String,                   // the file, by its path after the change
    side: DiffSide,                 // Old (lines removed) | New: where `first` and `last` count
    first: u32,                     // from 1, `last` no smaller
    last: u32,
    quote: String,                  // the lines as the diff showed them, each after its + - or space
}
```

### Blockers and change requests

Both stay **open** until a later comment resolves them.

| Kind | Says | While open | Goes to |
|---|---|---|---|
| **Blocker** | the work cannot start or go on until someone answers or acts | the item is not ready; a work session on it parks | the first principal or agent it mentions; else a supervisor; else the writer's principal |
| **Change request** | something the work must do before it is submitted | `wf work submit` and `Approve` are refused | whoever works on the item next; the brief lists it |

* An agent asks by writing a blocker on its own item and ending its turn; its session parks and
  continues, with what was written meanwhile, once the blocker is resolved. The same holds for waits
  outside the model (credentials, a vendor).
* A blocker that mentions no one goes to the agents in the `supervision` lane, if any: one that never
  worked on the item gets a supervise session with the item's history in an empty directory. It
  either answers (a comment resolving the blocker) or escalates (a new blocker mentioning the
  principal that resolves the old one), which ends the session `Supervised`. A blocker that mentions
  an agent gets a supervise session for that agent. Blockers waiting on a principal are in `wf inbox`.
* **Resolving:** any comment can resolve open blockers and change requests of its item, including a
  blocker or change request (an escalation), and may have empty text. `--resolve-blockers` and
  `--resolve-change-requests` expand to the ids open when the command runs, and the event stores those,
  so a blocker written concurrently elsewhere is never resolved by a reply that did not see it.
  Resolution is grow-only; a resolution someone does not accept gets a new comment.

### Comments on lines

A comment can be about lines of the diff of the item's work, as the dashboard's diff writes them. The
anchor names the commit the diff was of, so it means the same once the branch has moved on, and it
quotes the lines, so it reads without git: everywhere a comment's text is shown (`wf item show`,
`wf log`, a brief) the anchor goes above it, as `src/lib.rs:12-14 at a8d49484` and the lines indented
under it. The dashboard's diff shows each under the last line it is about. One about lines of a commit other
than the one on screen is outdated: it shows under the header of its file with the lines it quotes,
not under lines that may now say something else. While a review waits, comments written in the diff
are drafts of it, sent just before the verdict; otherwise they are posted at once. One about lines is
written under its own tag (`COMMENT_ON_LINES`), one without them as before, so earlier comments keep
their bytes.

### Comment ids

A comment's id is its event id, printed as the first **4** characters of its base32 hash, scoped to
the item: `--resolve c7f2` on `#rpdhor`, or `#rpdhor/c7f2` elsewhere (a backlink). Longer prefixes are
accepted, ambiguous ones refused. Fixed length, not the shortest unique prefix, which would grow when a
later comment collides and break ids already printed; not sequence numbers, which two devices would
write concurrently.

### Reports and steps

A report is a comment by a work session: progress, what was chosen and rejected, and why. It belongs
to its session, so an item's activity shows which attempt said what. Reports are asked for, never
required; nothing depends on commit messages.

* Steps are the `- [ ]` lines of the body. `--step <n>` takes the number `wf item show` prints,
  checked against the body when written, and stores the step's id (the hash of its text). If the text
  changes later, the comment keeps pointing at a hash no longer in the body and shows without a step.
* Ticks are their own edit (`StepDone{step, done}`), written by `wf item check` or `--step <n> --done`,
  so a comment never becomes invalid because a step changed. Review sessions do not comment on steps.
* *Planned (M3):* comments of agent sessions pass redaction before signing (section 13).

### Activity

`wf item show` ends with the item's **activity**: one timeline of everything that happened to it, in
fold order, replacing a separate history.

* Comments get a header (time, actor, kind, what they resolve, id) and their text; change requests
  written with a verdict are listed under it.
* Events get one line: moves (with their `because`), edits, links, sessions, verdicts, merges,
  archiving. A move carried out for a session (`by_session`) shows in the session's line
  (`submitted 3e8a1c → review`).
* Other items appear where they change this one: a dependency done or cancelled, a new dependent, a
  reference.
* A body edit that lost to a concurrent edit is shown with its text, so nothing disappears silently.
* The header lists what is open: blockers with whom they wait on, and change requests.

`wf item show` and `wf stats` also count each item's sessions, submissions (review rounds), change
requests and the time from its first work session to done.

### Planned: project decisions and search (M4)

| | Item decisions | Project decisions |
|---|---|---|
| Where | reports on the item | `decision` items |
| Written by | the working agent | agents, distill sessions, principals |
| Ratified | no | yes: `done` |
| In agent briefs | when working on that item | always, once ratified and not superseded |

A ratified decision changes only through a new decision that `supersedes` it. Search is local per
peer: a derived, rebuildable index (tantivy for keywords, a local vector index with a local embedding
model), fused by reciprocal rank: `wf search "..." --kind decision,comment --since 30d`.

## 12. Harnesses

The daemon is an ACP client (protocol v1, behind a `Harness` trait). Each session drives one harness
subprocess, in the agent sandbox (section 13).

| Harness | ACP command |
|---|---|
| `claude-code` | `claude-agent-acp` |
| `opencode` | `opencode acp` |
| `codex` | `codex-acp` |
| `gemini-cli` | `gemini --experimental-acp` |

wf never handles harness credentials. `wf init` lists the harnesses on `PATH`; `wf harness models`
starts one and lists its models. An agent's model is set as the ACP `model` config option, and a
model the harness does not offer fails the session with the list.

| ACP | Use |
|---|---|
| `session/new` | start in the session's directory with `WF_SESSION`, `WF_SESSION_TOKEN`, `WF_WORKFORCE` |
| `session/resume`, `session/load` | resume after a daemon restart (section 9) |
| `session/prompt` | the brief, nudges, "what you were waiting for is resolved", "wf restarted" |
| `session/update` | the transcript (local, `wf session watch` and the dashboard) |
| `session/request_permission` | answered by the agent's `permissions` (below) |
| `session/cancel` | `wf session stop`, `max_duration`, the item moved on, daemon halt; not when the agent ended its own session, which gets 20 s to finish its turn |
| `terminal/*`, `fs/*` | not offered; harnesses use their own tools |

The CLI and the dashboard are thin layers over the same daemon calls.

*Planned:* a `wf` MCP server attached at `session/new` (M3); a `raw-cli` fallback that starts any
agent with a prompt file and watches commits (M3); `wf attach <session>` over an iroh ALPN `wf/acp/1`
(M5); probing harnesses with ACP `initialize`.

### Permission requests

An agent's `permissions` decide its harness's requests: `deny` refuses, `allow` allows (on a machine
without a sandbox it is treated as `ask`), and `ask` passes the request through a chain of
**deciders**, in the machine config's order, until one allows or denies:

* **`rules`** (built in): a command that only runs `wf item show|list|check|path`, `wf comment|work|
  review|inbox|status|log|mentions`, or asks any `wf` command for `--help`, alone or joined by `;` or
  `&&`, optionally piped into `head`, `tail`, `grep`, `rg`, `wc`, `sort`, `uniq`, `cut` or `jq`. A
  filter takes only flags from a short list per filter and, for `grep`, `rg` and `jq`, one pattern or
  program: no file, and no flag that reads, writes or runs (`-o`, `-f`, `--pre`, `-r`, `-e`; a jq
  program that imports or reads the environment). `wf item checkout` and `wf item diff` are not
  among them. Nothing that changes
  devices, agents, policy or permissions, so an agent cannot widen its own rights. Whatever it does not
  fully understand it passes on.
* **`hook`**: a program, run without a shell in the session's directory, gets the request as JSON
  and answers `{"permission": "allow" | "deny" | "ask", "reason": ...}`.
* **`model`**: a decision model behind the System One API at a configured `url`, asked either for the
  probability that the call should be allowed (`noul`: allow above one threshold, deny below another)
  or to choose (`choice`, taken above a minimum confidence). Its key comes from a daemon environment
  variable that agents do not get.
* **`agent`**: an agent of the workforce, in a harness session the daemon keeps per agent, replaced
  after `turns` requests, answering like a hook. It runs outside Fence and every permission request of
  its own is denied. The model decider is preferred: one judge session serializes requests, and an
  injection stays in its context for later decisions.

A decider that fails, times out or answers something unparseable passes the request on; it never
decides by accident. What every decider passes on goes to the principal (`wf inbox`, the dashboard,
`wf session allow|deny`); only a principal connection may answer, never a session. Without a config
the chain is `rules`, then the principal. Each decision, its decider and reason go into the session's
transcript. `wf permissions test` runs the chain on a request without a session.

Deciders live in the machine config, never in the signed policy: policy that syncs must not make
other machines run programs or call paid APIs.

### Hooks

Every program wf asks for a decision speaks one protocol: one JSON object on stdin naming the hook and
its version (`{"hook": "permission", "version": 1, ...}`), one object on stdout, unknown fields
ignored on both sides, and failure passes on. Only the permission hook is built.

**Planned: command steps.** A review or recovery step may name a command, `{ command = "ci-gate" }`;
the machine config's `[commands.<name>]` says what it runs there (argv, timeout), and a machine without
it cannot take the step (flagged like an unstaffed step). The command gets the item, the submission
and earlier steps (`"hook": "review"`) or the ended attempts (`"hook": "recovery"`) and answers a
verdict with a note and change requests, or `retry | principal | cancel` with a note. wf validates the
answer and writes the events, attributed to `@daemon` with the command's name; a command needs no key,
session or socket. `wf hook schema` prints JSON Schema generated from the Rust types (`schemars`), from
which bindings (a TypeScript library first) are generated; `wf hook test` runs a command on a request.
Open: whether review commands run in Fence; progress output for long gates; whether a scheduler hook
for dispatch and turn-end decisions uses the same protocol.

### Notifications

With `[notify] command` in the machine config, the daemon runs it, without a shell, once for each new
inbox entry that waits on the principal: a blocker, a proposal, a principal review step, a merge
waiting on a dirty checkout, a recovery left to the principal, and a held permission request.
`{kind}`, `{item}`, `{title}` and `{what}` in its arguments are replaced. Entries are announced after
each dispatcher round, so work about to merge is not announced. What was announced is kept in
`<wf>/announced.json`, so a restart does not repeat it, and an entry that goes and comes back (a
resubmission) is announced again. A command that fails or takes over 20 s is logged and stopped.

## 13. Machine config, sandbox and secrets

What makes sense on one machine only lives in its **machine config**,
`$XDG_CONFIG_HOME/workforce/config.toml`, never synced: another machine can never make this one run a
program or call a paid API. Unknown keys are errors; without a file every default applies. The daemon
reads it anew for each session and permission request; a broken config sends permission requests to
the principal with the reason. A mistake in `[theme]` only warns and keeps the built-in colours.

| Section | Sets |
|---|---|
| `[sandbox]` | `kind = "fence"` (default) with `allow_write` and `allow_domains`, or `"off"` |
| `[environment]` | `inherit` from the daemon's environment, `set` values (with `~`, `$NAME` expanded) |
| `[agent.<name>]` | more `environment` and `sandbox` for one agent |
| `[commits]` | `agent_signing`, `merge_signing` (section 10) |
| `[check]` | `max_timeout`, and the checks' own `sandbox` (default: `[sandbox]` without per-agent additions) |
| `[storage]` | `budget` and the rebuildable `caches` paths inside worktrees (section 9) |
| `[notify]` | `command` (section 12) |
| `[permissions]` | `guidance` text for models and agents, and the `[[permissions.decider]]` chain |
| `[theme]` | colours of the CLI and dashboard, in git's colour syntax |

### The agent sandbox

The daemon runs every harness in **Fence**, with a profile it writes per session from Fence's `code`
template. Without Fence installed, sessions fail; there is no silent fallback. `kind = "off"` runs
agents with the principal's privileges, and then `allow` is treated as `ask`.

* **Write:** the session's directory, the parts of the repository's `.git` that commits need (`objects`, remote and tag refs, the
  session's own branch, the admin directory of its own worktree as the `gitdir:` line of the worktree's
  `.git` file names it, and the lock of `packed-refs`, which git takes to delete the pseudorefs it
  removes after each commit; the file itself stays closed), temporary files,
  `~/.cache` and the caches and state of agent tools and package managers, the daemon's socket, and
  `allow_write`. Not `.env*` or key files.
* **Network:** model APIs, git hosts and package registries, plus `allow_domains`; cloud metadata and
  telemetry are denied.
* **Commands denied:** pushing, resetting, rebasing and merging with git, publishing packages, `sudo`,
  and `gh` commands that change anything.
* **Always denied, whatever the config adds:**
  * reading or writing the device key, principal keys, the principal token and the machine config,
    with which an agent could sign as the device or the principal, have the daemon act for the
    principal, or turn its sandbox off;
  * writing `.git/config`, `.git/hooks`, the worktree's `.git` file, the `commondir` it leads to and the
    worktree's `config.worktree`, and git's own config, with which git would run the agent's code outside the sandbox;
  * writing the target branch, any other item's branch, `packed-refs`, `HEAD` and the admin
    directories of other worktrees, with which an agent would move the target past review and checks;
  * reading SSH and GPG keys and the credentials of clouds, registries and docker.

`wf` in a session reads no key: the daemon signs what it writes.

**Repository maintenance** (`gc`, `pack-refs`, repacking) is not the agents': they cannot write
`packed-refs` or the repository's config, and git's automatic maintenance fails in the sandbox. The
git the daemon runs outside it (merges, rebases, commits of the merge) keeps the default
`maintenance.auto`, so git packs and prunes the repository as it always does when the principal or
the daemon works in it. Nothing relies on an agent doing it.

### Environment

An agent's environment is only: what wf sets (`WF_SESSION`, `WF_SESSION_TOKEN`, `WF_WORKFORCE`,
`PATH` with `<data>/bin` first, `GIT_AUTHOR_NAME`, `GIT_COMMITTER_NAME`, signing through
`GIT_CONFIG_*`); what every program expects (`HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `LANG`,
`LC_*`, `XDG_*_HOME`); and what the machine config inherits or sets, for all agents or one (the
agent's value wins). wf's own variables cannot be set there. A variable allows nothing in the sandbox
by itself: a directory to write to also goes in `allow_write`.

### Planned: secrets and transcripts (M3, M7)

* `wf` brokers secrets, it does not store them: sources such as `secretspec.toml`, 1Password `op://`
  references and the system keyring; values are injected into the harness environment and kept in
  memory only.
* Those values are used to scrub: exact match plus base64, URL and hex encodings, then gitleaks-style
  pattern scanning. Comments of agent sessions pass this redaction before signing.
* Transcripts are local and never synced today. A per-workforce `publish = none | redacted | full`
  comes with stage 3 and 4 (M7).
* Real isolation of donor agents is OS-level, beyond what ACP routes through the client (M7).

## 14. Connectivity

### Creating

`wf init --name <name> [--handle] [--device-name]` inside a git clone writes genesis with the default
policy and the principal as founder (handle `$USER` by default), a fresh principal key, a
`DeviceProfile` (host name; hardware `os:…, arch:…`) and `.workforce.toml`. It lists the harnesses
found on `PATH` with the `wf agent add` command for each, and defines no agent; `wf init -i` asks for
the name and which agents to define.

### Joining a device

```
wf invite device [--expires 24h]       # on a machine with the principal key; prints wfi1...
wf join <code> [--name] [--max-sessions] [--agent NAME=N] [--hardware]   # on the new machine, in a clone
```

1. The code carries the workforce id, the id of the `InviteCreated` event, the invite secret key and
   the endpoint ids of the inviting device and other devices of the principal. Long codes, no
   rendezvous infrastructure.
2. **Invites are self-authorizing.** The principal signs `InviteCreated{kind, invite key, expires}`.
   The joiner writes `DeviceBound` as the first event of its feed, with the invite among its deps and
   a proof by the invite key over its own device key. Any peer verifies it offline; no owner needs to
   be online, the new device never sees the principal key, and nothing in the log could bind another
   device.
3. **Expiry and single use are enforced by the member serving `wf/join/2`**, not by admission: they
   need a clock and coordination, and admission must depend on neither. The member reserves an invite
   for the first device that proves it (in memory). A leaked code redeemed through two members that
   cannot see each other binds both devices, on every peer alike, and both are flagged; the fix is
   `wf fork --exclude`.
4. The joiner receives the log, writes `DeviceBound` and its `DeviceProfile`, and pushes them back on
   the same stream. Joining is idempotent for the same device, and goes through the daemon when one
   runs.

*Planned:* member invites (M5), an interactive join that offers to host agents and install a service,
mDNS for LAN-only use.

### Sync

* **`wf/sync/3`**: the initiator opens a workforce; both sides exchange version vectors
  `{author → (max seq, tips)}` (tips for equivocated feeds); each streams what the other lacks, in
  `(hlc, id)` order, in frames of at most 256 events and 8 MiB; then missing parents and wanted history are fetched by id, answered in `Have` frames and an `EndOfHave`.
  One side writes at a time. A peer that neither sends nor accepts a byte for 30 s ends the session, and `Open`, `Versions`, wants and the join messages have small size limits. An event of more than 1 MiB is refused at admission, so every event fits a frame. The ALPN is bumped whenever the message format changes (`2`: `EndOfWants` replaced an empty `Want`; `3`: `Have` is ended by `EndOfHave`), so mismatched peers refuse at the handshake.
* **Gossip** on a topic derived from the workforce id announces a digest of each device's heads after
  writes and every 60 s; a digest that differs from its own makes a device sync with that peer. One
  task per peer device syncs on announcements and every 60 s, backing off from 5 s to 5 minutes.
  Each announcement carries the device key, the digest and the time (seconds since the epoch),
  signed by the device key under the `wf/heads/1` domain over the topic, the digest and the time. A
  receiver drops an announcement whose signature does not hold, whose time is more than 5 minutes
  from its own, or whose device is not bound in the workforce of that topic; this is why presence
  ("last seen", kept in memory) can be trusted.
* **Members only**: each protocol handler rejects endpoints that are not bound devices of a workforce
  the node serves; sync checks again once the workforce is named. Join is open, and checks the invite.
* **The network is off until it is needed.** A daemon starts no endpoint and no gossip for a
  workforce with one device and no open invite; `wf invite` turns it on.
* `wf sync` syncs once without a daemon (`--serve` keeps listening). `--relay <url>` replaces n0's
  public relays; address lookup is n0's.
* *Not tested yet:* real NATs and relay fallback.

### iroh

| Need | Piece |
|---|---|
| connectivity, NAT traversal, relays, encryption | `iroh` endpoint, dialed by `EndpointId` |
| announce heads | `iroh-gossip` (digests only; events go over `wf/sync/3`) |
| large artifacts (*Planned*: bundles, transcripts) | `iroh-blobs` |

**Not iroh-docs:** it is a mutable key-value store, not a log. Each `(author, key)` keeps only its
latest value by wall clock, and a write removes the author's older entries under a prefix, so a device
could erase its own events on every replica. That breaks the append-only log, equivocation evidence
and admission against the causal past. Its namespace secret is a bearer credential shared by every
writer. A state-based model on it would be simpler but gives up history, offline authorization and
causal conflict resolution (P3, P4, P6, P9), while removing only `wf/sync/3`. We write sync ourselves,
which also keeps the log independent of transport. Keep the shape compatible with Willow ideas (author
as subspace, `kind/id` as path) in case that matures.

Built by us on top: authorization, members-only gossip, ordering, and (*Planned*) store-and-forward
through the anchor, compaction, search and blob access control.

## 15. Policy

The policy is TOML text in the log, signed by an owner's principal key (`PolicySet`), so every machine
follows the same one. A copy in the repository has no effect. `wf policy show | edit | apply <file>`;
`edit` and `apply` need the principal key. The default is `crates/wf-core/src/default_policy.toml`,
reproduced in `docs/guide/policy.md`.

The policy is the latest readable `BoundPolicySet` by stamp (an unreadable one is flagged and the unbound policy runs). An unbound `PolicySet` (and the genesis policy) only
counts until the first `BoundPolicySet` is folded and never again after it, whatever its parents or stamp,
so an old owner-signed policy written into a new event cannot roll the policy back. This is a rule of the
fold, so it does not depend on arrival order.

| Key | Holds |
|---|---|
| `default_agent` | the agent `wf session start` uses without `--agent` |
| `[lanes.<name>]` | the lanes (`default`, `meta`, `supervision` by default) |
| `[types.<name>]` | `lane`, `fields`, `depends_on`, `states`, `initial`, `review`, `recovery` (section 8) |
| `[[review]]` | `name`, `steps`: `{ agent = { requires = [...] } }` or `"principal"` (section 10) |
| `[[recovery]]` | `name`, `steps`: `"retry"`, `{ agent = { requires = [...] } }` or `"principal"` (section 9) |
| `[merge]` | `mode = "local"`, `target` (default `main`) |
| `[[merge.check]]` | `name` (unique), `run` (argv, no shell, not empty), `timeout` (default 30m) |

Validation: the built-in types must be present; level and check names are unique, and `default` is
reserved; types name existing types and levels; the state rules of section 8. A policy that does not
parse is refused when written, and flagged if it arrives (section 6).

Checks are the one place the policy names programs. They run what the branch contains, as agents
already do, but in the sandbox this machine's config gives checks and with no secrets, so each
machine's principal decides what they may do. Being in the policy puts them out of reach of the work
they check.

*Planned:* `[compaction] after` (M6), `[assignment]` with `mode`, `fallback` and `lease` (M3),
`[transcripts] publish` (M7), `[governance]` (advisory, M7), `wf policy diff`. *Open:* whether
`default_agent`, a convenience of one principal, belongs in the machine config instead.

## 16. CLI

```
wf [--item <item>]                                  # the dashboard at a terminal; piped, the help
wf init | status [--refold] | stats | members | log [--follow] [--item] | export | import <file>
wf policy show | edit | apply <file>
wf item add [--draft] | list [--sort priority|created|merged] [--group-by state|readiness|label|type|lane|priority] | show | edit | link | check | archive | unarchive | diff | checkout | path
wf item open | draft | done <item>... | cancel <item>       # [--note]
wf comment <item> [<text>] [--block | --request-change] [--resolve <c>... | --resolve-blockers |
           --resolve-change-requests] [--step <n> [--done]]
wf mentions <@handle | @handle/agent>
wf review <item> --approve | --request-changes | --escalate [--note]
wf check [<name>...]
wf inbox
wf agent add | list | show | edit | remove;  wf harness models <harness>
wf session start [--watch] | list [--live] | show | stop | watch | allow | deny
wf work brief | submit | release                            # agents, inside a session
wf permissions test [--command <cmd>]
wf daemon start | stop | restart | status | log | run
wf sync [--serve] | invite device | join <code> | device list | edit | allowed-signers
wf fork [--exclude <device|@member>]... [--new-principal] | fork accept <id> | fork list
```

* **Ids** print as 6 characters of base32 for items (any unique prefix of at least 4 is accepted,
  with or without `#`) and 4 for comments. Missing item or session arguments open a picker at a
  terminal; a missing text opens `$EDITOR`.
* **`wf item add` and `edit`** without flags open a draft in `$EDITOR`: TOML front matter between
  `+++` fences (`title`, `priority`, `lane`, `labels`, `requires`, `review`, `recovery`; `type` and a
  commented `state = "draft"` for new items), then the body verbatim. Unknown keys are refused. State
  changes only through the verbs.
* **`wf inbox`** lists what waits on the principal: `blocker`, `proposal` (an item in `review` without
  submitted work, such as a decision), `principal_review`, `merge`, `recovery`, `permission` and
  `machine_fault`, each with the command that answers it. `wf status` summarises the workforce, flags
  what needs someone (stranded items, steps no agent can take, unknown states, an unreadable policy)
  and says what to do next.
* **`--json`** gives `{"schema": 8, "data": ...}` for every command with output to read (not the
  dashboard or `session watch`; `log --follow` prints one object per line). The number is bumped on
  incompatible changes, and the shapes are snapshot-tested.
* **Plain output**: lists are borderless tables built by one function in `wf-cli`; times are local;
  styling only at a terminal, in the colours of `[theme]`.

### The dashboard

Plain `wf` at a terminal opens a dashboard (ratatui) meant to stay open beside the editor. It needs
the daemon and polls it every 500 ms. It shows only view models the commands show too, and its
actions write the same events as the commands. Four tabs, each a list with a detail beside it (from
120 columns) or opened with `enter`:

* **Now:** what waits on the principal (the inbox), the live and parked sessions with what each is
  doing and how much context it used, and the `wf status` flags.
* **Items:** grouped by state, lane, priority or type, filtered by readiness or title.
* **Agents:** this week's numbers, and each agent's load and sessions.
* **Log:** `wf log --follow`.

An item's detail is `wf item show`; a session's is its live transcript (this machine's sessions
only). Keys answer what waits in place (allow or deny a request, answer a blocker, approve or request
changes, start, stop, move, comment, edit), the mouse selects and follows links, and a footer warns
when the daemon sends approved work back after a conflict. Keys are in `docs/guide/dashboard.md`.
*Planned:* a push subscription instead of polling; transcripts of other devices (M3).

## 17. Not yet decided

1. Should merges into the main branch need a principal key once origin-side protection is not enough?
   (v1: a review level with a principal step in `local` mode, the origin in `origin` mode.)
2. Do heartbeats belong in the log (M3)? One per minute for three sessions is about 130k events a
   month, the main source of log growth. Liveness is computed at query time anyway, so a lease renewed
   over gossip plus a final `SessionEnded` might do. Settle this before M3; it decides how urgent
   compaction is.
3. With several principals (M5), can a review step require a principal other than the author?
4. Merging without the checks: `wf item merge --unchecked`, or edit the policy for a moment?
5. Should the limit of two failed check runs be part of the recovery level?
6. Batching several approved items into one checked merge, if checks get slow. Not before it hurts.
7. A version byte per body instead of a new tag per layout change, before bodies change often.
8. Prior art to read before M7: Radicle, p2panda, Keyhive, Matrix state resolution.

## 18. Milestones

| M | Scope | Status |
|---|---|---|
| M0 | Signed event log, identity, item model, fold, redb store with snapshots; one peer | built |
| M1 | iroh sync, equivocation, device invites and `wf join`, device profiles, daemon, gossip, `wf fork` | built |
| M2 | Agents on one machine: sessions, worktrees, ACP runner, turn end and parking, dispatcher, blockers, change requests, supervisors, review levels, local merge, permission modes, inbox, steps, reports, activity | built |
| M2.1 | Machine config, permission deciders, `wf item diff`, `checkout`, `path` | built |
| M2.2 | Daily use on one machine (**stage 1**): recovery levels, machine faults, resume after restart, daemon management, export and import, network off for one device, notifications | built |
| M2.3 | The dashboard | built |
| — | Since: merge checks and `wf check`, rebase at submit, fixed states with verbs, commit signing, storage budget, `[theme]`, `wf stats` | built |
| M3 | Sessions across devices (**stage 2**): anchor dispatch, claims and leases, `raw-cli` fallback, `wf` MCP server, mention delivery, overlap notifications from touched files, secret brokering, redaction, merge mode `origin` | next |
| M4 | Decision ratification, distill items, hybrid search | |
| M5 | Several principals, member invites, threads, scopes, `wf attach` (**stage 3**) | |
| M6 | Git bundles over iroh-blobs, peer-to-peer repositories, `seen` and compaction | |
| M7 | Donors (**stage 4**): revocation, rate limits, sandboxing, transcript publishing, governance | |
