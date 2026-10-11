# workforce (`wf`): spec, draft v0

> A peer-to-peer, CLI-first coordination layer for coding agents and the humans who work with them.

Status: draft. Scope of the first implementation is **UC1** (one developer, many machines). Everything
marked *later* is designed for, but not built.

## 1. Use cases

### UC1: one developer, many machines (first target)
A laptop (sleeps, roams), a cloud VM (always on), a home desktop (GPU, behind a NAT). Each runs agents.

* Machines sit behind different NATs with changing IPs. No port forwarding, no VPN.
* The laptop is often offline. Work continues and merges back when it reconnects.
* All machines are the same human. One principal with several device keys.
* Work should go to the machine that fits (GPU, OS, harness, model).
* Two agents should not silently pick up the same item.

### UC2: several developers, humans and agents mixed (later)
* Every change traces back to a human principal.
* Review by a different principal than the author is a useful default.
* Async, multi-timezone: lots of "why did we decide X?" lookups.
* Not every transcript is meant for everyone.

### UC3: donated agent work (later)
* Untrusted participants: needs revocation, rate limits, sandboxing, probation.
* Donors can take part in discussions. Governance is advisory.
* Prompt injection through issue text is a real risk on donor machines.

### Cross-cutting
1. The only thing all peers must agree on is **project state**: items, claims, reports, decisions, membership.
2. Authorization data replicates with the project and is checkable offline.
3. "No external infrastructure" means none the user has to run. Public relays and discovery are
   defaults and can be swapped.
4. Someone must be online to hold data. This role is the **anchor**. It is named, never required.

## 2. Properties

| # | Property |
|---|---|
| P1 | Zero ops: `init`, `invite`, `join`, work. No server to deploy. |
| P2 | Local-first: every peer has a full replica of what it may read; all reads are local. |
| P3 | Event-sourced: truth is an append-only log of signed events; state is a deterministic fold. |
| P4 | Authorization is data in the log, verifiable offline. |
| P5 | Principals own delegated device keys; every action resolves to a principal. |
| P6 | Deterministic conflict resolution, with an optional coordinator (dispatcher). |
| P7 | Git first; VCS access behind a trait so another VCS can be added later. |
| P8 | Harness-agnostic through ACP, with a CLI fallback. |
| P9 | **Forkable:** any member can start a new workforce from a frozen point of an old one. |
| P10 | Indirect communication: agents coordinate through shared artifacts, not messages. |
| P11 | **Derivable:** all workforce state is derivable from the log plus the repo. No daemon-only state. After compaction, the log means the kept events plus everything above the frontier. |

## 3. Non-goals for v1
* Revocation of access (use `wf fork` instead).
* Voting or binding governance (advisory only).
* A recovery key (fork instead).
* jj support (kept in mind, see 9).
* Peer-to-peer repositories (central origin first).
* A web UI.

## 4. Architecture

```
wf CLI (humans, agents)  ──unix socket──►  wf daemon (one per machine)
                                            ├─ event store (per-author feeds, redb)
                                            ├─ state (in memory, folded from the log, snapshotted to redb)
                                            ├─ policy engine (validates events)
                                            ├─ git adapter (worktrees, origin observer)
                                            ├─ harness runner (ACP client, wf MCP server)
                                            └─ iroh endpoint
                                                 ├─ wf/join/1   invites
                                                 ├─ wf/sync/1   feed sync
                                                 ├─ iroh-gossip announce heads, presence
                                                 └─ iroh-blobs  (later) bundles, transcripts
```

Without a daemon, `wf sync` does a one-shot sync. Any always-on daemon can act as the anchor.

The log and its fold do not depend on sync or the network: `wf-core` is pure and `wf-net` only carries
the log. Sync can therefore be a cargo feature; without it, `wf` is a local tool for one machine with
the same store, log and item model.

### Storage
* **The event log is the only durable truth.** It is a key-value workload (append, get by `EventId`,
  range by `(author, seq)`, heads, pending) and lives in **redb**: pure Rust, one file, copy-on-write
  B+trees, one writer with concurrent MVCC readers, stable file format.
* **State is folded into in-memory Rust structs** by `wf-core`. Queries are typed Rust, not SQL. Changing
  the state shape needs no migration, only a refold.
* **Snapshots:** the folded state is serialized to redb, keyed by the heads it covers. Startup loads the
  latest snapshot and applies newer events. Large bodies (comments, reports) stay in the log
  and are loaded on demand by `EventId`.
* **Derived indexes** (search, M4) are rebuildable and never a second source of truth.
* redb is single-process. The daemon owns the store; the CLI talks to the daemon and opens the store
  directly only when no daemon runs. In the daemon, one storage task owns the database and receives
  writes over a channel.
* Access goes through a narrow `EventStore` trait so the backend (fjall, SQLite) can be swapped in one module.
* Debugging without SQL: `wf export --json` plus `jq`, or load the export into DuckDB.

## 5. Identity and authority

### Concepts

| Concept | Lifetime | Answers | Address |
|---|---|---|---|
| **Principal** | permanent | who is accountable (a human) | `@joshua` |
| **Agent** | long-lived profile | what kind of worker: harness, model, capabilities, lanes | `@joshua/coder` |
| **Session** | minutes to hours | who is working on which item right now, where, in which worktree | `@joshua/coder/s4k2` |
| **Device** | long-lived infrastructure | where things run; mostly invisible | not mentionable |

* **Principal key** (ed25519): one per human. Signs authority events: owner actions, invites,
  device bindings, policy. Kept on disk for M0 (`$XDG_DATA_HOME/workforce/keys/`); a 1Password or
  ssh-agent hook comes later.
* **Device key** (ed25519): one per machine. Signs all ordinary events. It is the iroh `EndpointId`.
  Never copied between machines.
* **Agents have no keys.** The device signs, and every event carries an `actor`:

```rust
enum Actor {
    Human(PrincipalKey),
    Agent { agent: AgentId, session: SessionId },
}
```

  The device is trusted to state the actor truthfully (members are trusted in v1). Attribution chain:
  principal → agent → session → device signature.
* **Workforce id**: BLAKE3 hash of the genesis event. Identifies, but is not a capability, so it may be
  committed to the repo.

### Agents
Defined once per principal, in the log, and runnable on any device that hosts them.

```toml
# AgentDefined, signed by a device of the owning principal
[agents.coder]
harness = "claude-code"
model = "..."
capabilities = ["rust", "typescript"]
lanes = ["default"]
start = "manual"          # auto: the dispatcher starts sessions; manual: only `wf session start`
nudges = 3                # "continue" prompts in a row without a new commit before giving up
park_timeout = "2h"       # how long a session waits on a dependency (its question) before it ends
max_duration = "4h"       # optional limit per session
permissions = "ask"       # allow | ask | deny: answers to the harness's permission requests
```

* Named by role, not by harness, so the harness can change later.
* Names are unique per principal and immutable in v1, so mentions keep resolving. The other fields can
  change: a new `AgentDefined` for an existing name replaces the definition (last writer wins).
* Agent definitions are work configuration, not authority, so a device key may sign them. Agent actors
  cannot write them, so an agent cannot raise its own autonomy.
* An agent whose `lanes` include `supervision` is a **supervisor**: it answers the questions of other
  agents or escalates them to the human. It has no other rights; lanes route, members are trusted.

### Devices
A device publishes a mutable, device-signed `DeviceProfile`:

```toml
name = "desktop"
hardware = ["gpu", "os:linux"]
max_sessions = 3          # total concurrent sessions (default 1)

[agents.coder]            # agents this device hosts
max_sessions = 2
[agents.reviewer]
max_sessions = 1
```

Device names only appear in `wf status`, `wf device list` and debugging.

### Sessions
A **session** is one attempt by one agent on one device at **exactly one item**. It is the unit that
holds a claim. Items are sized for a session: steps inside one are a checklist in its body, and work is
split into several items only when the parts can run in parallel, need a different review level, or
need different capabilities.

* `SessionStarted{agent, item, role}` (device-signed), `role` = `Work` or `Review{commit}`. Its hash
  prefix is the session id. `SessionEnded{reason}`, reason = `Submitted{commit} | Reviewed | Released |
  Blocked | TimedOut | Failed | Cancelled`.
* One branch per **item** (`wf/<item>`) in one worktree. A session is an attempt: the next session on the
  item (after a release, a crash or requested changes) continues the branch.
* A work session ends when the agent submits (`wf work submit`), releases (`wf work release`), escalates by moving
  its item to a blocked state, or runs out of nudges or time. Waiting on a dependency, such as its own
  question (`wf work ask`), **parks** it: the harness keeps its conversation and continues once the item is
  ready again, or ends after the agent's `park_timeout`.
* Context carries over within a session in the conversation, and across sessions through reports,
  comments and review notes.
* **Liveness:** on one machine (M2) a session is live from `SessionStarted` until `SessionEnded`; a
  restarting daemon ends its own leftover sessions as `Failed`. Across devices (M3) liveness is computed
  at query time from a lease: a session without a renewal within the lease counts as dead, even without
  `SessionEnded` (crash, sleeping laptop). Its claim expires and the item becomes assignable again,
  possibly to the same agent on another device.
* The claim belongs to the earliest live work session of an item by `(hlc, EventId)`; a later one
  cancels itself.
* Agents run `wf` with `WF_SESSION` set by the runner, so their events carry
  `Actor::Agent{agent, session}`.
* Location transparency stops at uncommitted work. Reassignment carries over only what was pushed, which
  is why agents commit small.
* wf session ids are separate from ACP session ids; the ACP id is a local detail of the runner.

### Roles (v1)
| Role | Can do |
|---|---|
| `owner` | everything: invites, policy, `ForkAnnounced`, naming other owners |
| `member` | items, claims, comments, reports, reviews, decisions |

Agent actors act for their principal with the member's rights, minus a few **guards** (authoring checks
against mistakes and shortcuts, not attacks): a session cannot answer a question it asked, cannot lower
the review level of an item its agent worked on, cannot move its own item to a done state (it submits),
and cannot write `AgentDefined` or authority bodies. A reviewer agent never worked on the item it reviews.

Hard rule: `policy.*`, `invite.*` and `owner.*` events are valid only when signed by a **principal key**
(owner principal where the role requires it). A device binding carries a proof from a principal-signed
invite instead (section 14). No device key, so no agent, can create them on its own.

The founder (named in genesis) holds `owner` and loses it only by explicitly giving it away.
Membership only grows in v1, so validation is: *was the author a member at any causally earlier point?*
That answer is stable regardless of arrival order.

### Anchor
The owner may designate one device as anchor. It gets `dispatch` only. A compromised anchor can reorder
assignments, not change policy or roles.

## 6. Event log

Each device writes an append-only feed. Events also name causal dependencies.

```rust
struct Event {
    v: u16,
    workforce: Hash,
    author: DeviceKey,
    actor: Actor,            // human or agent session acting through this device
    seq: u64,                // per author, gapless, starting at 1
    prev: Option<EventId>,   // author's previous event
    seen: Vec<EventRef>,     // heads seen when writing, with author and seq
    hlc: Hlc,                // tie-breaks only, never authority
    body: EventBody,
    sig: Signature,
}
// EventId = blake3(canonical encoding of everything incl. sig)
```

* **Encoding:** canonical postcard (deterministic, compact). Signature covers the encoding without `sig`. See *Encoding and data format* below for the decision and the exact layout.
* **Sync:** exchange version vectors `{author -> max_seq}`, stream missing ranges, then fetch missing
  parents by id until nothing is missing. Below a peer's compaction frontier only kept events are sent.
* **Equivocation:** two events with the same `(author, seq)` are proof of misbehavior, or of a machine
  restored from a snapshot. Both branches are admitted, stored and relayed, and the fold applies both, so
  peers converge whatever they saw first. The device is flagged; a device that sees its own key write an
  event it did not write stops authoring.
* **Unknown event types** are stored, relayed, and ignored by the fold, so newer peers can add types.
* **Fold order:** the causal order from `prev` and `seen`, ties broken by `(hlc, EventId)`.
* `seen` is about events, not items: it records what the author had seen. Relations between items
  (`depends_on`, `supersedes`) are item links in event bodies.

### Event bodies (v1)
| Group | Bodies | Signed by |
|---|---|---|
| Authority | `Genesis`, `InviteCreated`, `MemberJoined`, `PolicySet`, `OwnerAdded`, `ForkAnnounced` | principal key (carried in a device feed) |
| Binding | `DeviceBound` | invite key proof, the new device's first event |
| Identity | `AgentDefined`, `DeviceProfile` | device |
| Items | `ItemCreated`, `ItemEdited`, `ItemTransitioned`, `ItemLinked`, `ItemArchived`, `ItemUnarchived`, `Comment` | device |
| Work | `SessionStarted`, `SessionEnded`, `Review`, `Merged`, `Report` (M2); `Assigned`, `Claim`, `Heartbeat` (M3) | device (`Assigned` by the anchor) |

Decisions and questions are item types, not separate event families.

### Encoding and data format

#### Decision: postcard
Event ids are the hash of the encoded event and signatures cover the encoded content, so the encoding
must give one value exactly one byte string, stay stable across releases, and be cheap to decode (a
snapshot of 100k events decodes in about 40 ms). We use **postcard** 1.x (a serde binary format with a
documented wire format).

| Alternative | Why not (for now) |
|---|---|
| Deterministic CBOR / DAG-CBOR | The strongest alternative: a standard, self-describing format with implementations in every language, and what AT Protocol (Bluesky) uses for content-addressed, signed repository data. Larger and slower to decode, and determinism needs a strict profile. Choose it if non-Rust clients or third-party readers of the log appear. |
| bincode | Similar speed, but uncertain maintenance and a wire format that changed between major versions. |
| Protobuf, Cap'n Proto | Not canonical by default; adds a schema toolchain and code generation for a Rust-only system. |
| JSON | Not canonical without extra rules (JCS), bulky, needs base64 for hashes and keys. Used only for `--json` output. |

Consequences, accepted:
* Postcard is not self-describing and struct field order is part of the format. Any layout change
  changes every event id, so layout is pinned by golden tests and changes go through the envelope
  version `v`.
* Because a decoder cannot skip a variant it does not know, event bodies are wrapped as
  `(tag, payload)` (below), which is what lets peers relay bodies from newer versions.
* The decoder rejects trailing bytes but does not require minimal varints. Ids are always recomputed by
  re-encoding the decoded value, never taken from received bytes, so this is harmless today. A relay
  that trusts received bytes would need a stricter decoder.
* All encoding lives in one module (`wf-core::encoding`), so replacing the format is a contained
  change, but it must happen before sync ships (M1): it changes every event id.

#### Primitives
| Type | Encoding |
|---|---|
| integers | postcard varints (`u16`, `u32`, `u64`); `seq` and `Hlc` fields included |
| strings, byte strings | varint length, then bytes |
| `Option<T>` | one byte `0` or `1`, then `T` |
| enums | varint variant index, then fields. Variant order is part of the format: append only |
| `Vec<T>`, `BTreeMap`, `BTreeSet` | varint length, then elements; maps and sets in key order. Hash maps are never serialized |
| ids (`EventId`, `WorkforceId`, `ItemId`, `SessionId`) | 32-byte BLAKE3 hash, written as a length-prefixed byte string (`0x20` + 32 bytes). Shown as lowercase base32 without padding |
| `PrincipalKey`, `DeviceKey` | 32 raw bytes, length-prefixed, kept as bytes without validating the curve point on decode. A key that is not a point never verifies a signature |
| `Signature` | 64 raw bytes, **no** length prefix (ed25519, deterministic) |
| names (`Handle`, `AgentName`, `TypeName`, `StateName`, `LaneName`, `HarnessName`, `Label`, `Requirement`) | strings, validated on decode: lowercase `a-z 0-9 - _` (labels and requirements also allow `:` and `.`), starting with a letter or digit, at most 64 characters |
| `Hlc` | `wall_ms: u64`, `counter: u32` |
| `Priority` | enum `P0..P3` |

Human-readable formats (`--json`) write ids, keys and names as strings instead.

#### Event
```
Event        = EventContent, sig: Signature
EventContent = v: u16 (currently 1)
               workforce: Option<WorkforceId>     // None only in genesis
               author: DeviceKey
               actor: Actor                       // Human(PrincipalKey) | Agent{agent: AgentId, session: SessionId}
               seq: u64                           // per author, from 1
               prev: Option<EventId>              // None exactly when seq == 1
               seen: Vec<EventRef>                // sorted by id, deduplicated
               hlc: Hlc
               body: EventBody
EventRef     = author: DeviceKey, seq: u64, id: EventId
AgentId      = principal: PrincipalKey, name: AgentName
EventBody    = tag: u16, payload: bytes           // payload = postcard of the body struct for that tag
```
* **Event id** = BLAKE3 of the encoded `Event` (signature included). The **workforce id** is the id of
  the genesis event.
* **Device signature** covers `b"wf/event/1\0"` followed by the encoded `EventContent`.
* **Principal signature** (inside authority bodies) covers `b"wf/authority/1\0"` followed by the encoding
  of `(workforce: Option<WorkforceId>, statement)`. The workforce is `None` only for genesis, so a
  statement cannot be replayed into another workforce. The two domain prefixes keep a device signature
  from ever being valid as a principal signature, and the key types are distinct in code for the same
  reason.
* `hlc` of an event is greater than the `hlc` of its `prev` and every event in `seen`. The fold orders
  concurrent writes by `(hlc, event id)`, which therefore extends causal order.
* `seen` names the author and seq of each event, not only its id, so a peer can tell whether a missing
  parent lies below its compaction frontier (see *Compaction*). This replaces the id-only `deps` of
  M1 and changes every event id, so it lands before compaction does.

#### Body tags
Tags are explicit and never reused. An unknown tag decodes to `Unknown{tag, payload}`, is stored and
relayed unchanged, and is ignored by the fold. A known tag with an undecodable payload makes the whole
event invalid.

| Tag | Body | Payload |
|---|---|---|
| 1 | `Genesis` | `Signed<Genesis>`: `name`, `founder: MemberSeed`, `members: Vec<MemberSeed>`, `policy` (TOML text), `forked_from: Option<ForkPoint>` |
| 2 | `PolicySet` | `Signed<PolicySet>`: `policy` (TOML text, stored verbatim) |
| 3 | `AgentDefined` | `name`, `harness`, `model: Option<String>`, `capabilities`, `lanes`, `start`, `nudges`, `park_timeout`, `max_duration`, `permissions` (M2) |
| 4 | `InviteCreated` | `Signed<InviteCreated>`: `kind` (`Device`), `invite: InviteKey`, `expires` (M1) |
| 5 | `DeviceBound` | `invite: EventId`, `proof: Signature` by the invite key (M1) |
| 6 | `DeviceProfile` | `name`, `hardware`, `agents: {agent -> max_sessions}`, `max_sessions` (M1) |
| 7 | `ForkAnnounced` | `Signed<ForkAnnounced>`: `to: WorkforceId`, `members` (M1) |
| 10 | `ItemCreated` | `item_type`, `title`, `body`, `priority`, `lane`, `labels`, `requires`, `review: Option<ReviewName>` |
| 11 | `ItemEdited` | `item`, `changes: Vec<Change>` (`Title`, `Body`, `Priority`, `Lane`, `Labels`, `Requires`, `Review`) |
| 12 | `ItemTransitioned` | `item`, `to`, `note: Option<String>` |
| 13 | `ItemLinked` | `item`, `link`: `DependsOn(SetChange<ItemId>)` or `Supersedes(ItemId)` |
| 14 | `ItemArchived` | `item` |
| 15 | `ItemUnarchived` | `item` |
| 16 | `Comment` | `item`, `body` |
| 17 | `ItemCheckpoint` | `item`, `state`: the folded item with stamps, add tags and note text (M6, see *Compaction*) |
| 20 | `SessionStarted` | `agent: AgentName`, `item`, `role`: `Work` or `Review{commit: CommitId}` (M2) |
| 21 | `SessionEnded` | `session: SessionId`, `reason: EndReason` (M2, see *Sessions*) |
| 22 | `Review` | `item`, `commit: CommitId`, `verdict`: `Approve`, `RequestChanges` or `Escalate`, `note: Option<String>` (M2) |
| 23 | `Merged` | `item`, `reviewed: CommitId`, `commit: CommitId` (M2) |
| 24 | `Report` | `session: SessionId`, `note: String`, `step: Option<StepId>` (M2, see *Reports*) |

* `Signed<T>` = `statement: T`, `signer: PrincipalKey`, `signature: Signature`.
* `MemberSeed` = `principal`, `handle`, `devices: Vec<DeviceKey>`, `previous: Vec<PrincipalKey>`. The
  founder is `owner`; seeded members are `member`. `previous` lists keys the member used in the parent
  workforce of a fork, so prefix events resolve to the same handle.
* `SetChange<T>` = `Add(T)` or `Remove{value: T, observed: Vec<EventId>}`. A removal lists the add events
  its author had seen, so a concurrent add survives it (add wins) without any ancestry index.
* An item id is the id of its `ItemCreated` event. Policy and `Genesis` carry TOML text, not a parsed
  structure, so the stored policy is exactly what the owner wrote.
* `CommitId` = `Git(GitOid)`, the single `vcs` variant (section 9).
* Bodies for later milestones (`Assigned`, `Claim`, `Heartbeat`, ...) take new tags.

#### Local storage (redb)
One file per workforce at `$XDG_DATA_HOME/workforce/<workforce id>/store.redb`. Values are the postcard
encodings above; redb itself stores bytes.

| Table | Key | Value |
|---|---|---|
| `events` | event id (32 bytes) | encoded `Event` |
| `feeds` | `(author, seq)` | event ids (multimap: more than one only after equivocation). An append's `prev` must be stored |
| `heads` | author | tips `(seq, event id)` of the author's feed (more than one only after equivocation) |
| `pending` | event id | encoded `Event` still waiting for parents |
| `snapshots` | BLAKE3 of the covered heads | encoded `(covered heads, blob)` |
| `meta` | `latest_snapshot` | key of the current snapshot |

A snapshot blob is `(format: u32, ref parser version: u16, State)`. It is a cache: any mismatch of
format or parser version, or a snapshot that is ahead of the log, discards it and the state is refolded
from `events`. Appends and the snapshot that covers them commit in one transaction.

Principal-signed statements are wrapped in an ordinary device-feed event, so every feed stays gapless
and a principal key never needs its own sequence across machines.

### Admission and fold
Two kinds of rules, because a rule is only safe for peers to enforce if its answer cannot depend on the
order events arrive in.

**Admission** (`State::receive`, enforced by every peer; depends only on an event's causal past):
version; device signature; `workforce` matches (genesis names none); all parents (`prev` and `seen`)
are known or below the frontier; `prev` is the author's event with `seq - 1` (none for `seq == 1`); `hlc` is greater than
every parent's; the author is a bound device and the actor belongs to its principal, where a
`DeviceBound` as the author's first event, with its invite among its parents and a valid proof, binds
it; authority bodies carry a valid principal signature of an owner for this workforce; referenced items
exist; no item links to itself.
* Events with missing parents (or arriving before genesis) wait in a **pending set**, capped at 10,000
  events, and are applied when their parents arrive. Any arrival order and any duplication give the same
  state; this is tested with generated event sets.
* A second event with the same `(author, seq)` is admitted and the author flagged (see *Equivocation*).

**Authoring checks** (`State::check_policy`, run by the author before signing; peers do not repeat
them): type exists, target state exists, field allowed for the type (`priority`, `labels`, `requires`),
lane exists, `depends_on` target type allowed, no dependency cycle, archive only from `done`/`cancelled`,
`supersedes` only between decisions. Peers cannot re-check these deterministically: a concurrent policy
change would make acceptance depend on arrival order. Instead the fold keeps the event and flags the
outcome ("unknown state or type", visible in `wf item show` and `wf status`), and the item keeps its
state until moved.

**Fold:**
* Concurrent writes order by `(hlc, EventId)`. This extends causal order because authors stamp every
  event above everything it follows, and admission enforces it.
* Last-writer-wins registers: `title`, `body`, `state`, `priority`, `lane`, `supersedes`, archived.
* Add-wins sets (`labels`, `requires`, `depends_on`) are observed-remove: each add is tagged with its
  event id and a removal lists the tags its author saw, so a concurrent add survives.
* An item's `state` is empty until its first transition and means "the type's initial state under the
  current policy", so item creation does not depend on concurrent policy changes.
* `supersedes` also writes `superseded` into the target's `state` register with the link's stamp. It
  is the only link whose event changes another item, which compaction has to account for.
* `depends_on` cycles from concurrent links are resolved at query time, not stored: edges are taken in
  `(hlc, EventId)` order and an edge that would close a cycle is ignored and flagged.
* Comment and transition-note text stays in the log. The fold keeps who wrote it and the tokens parsed
  from it. `#refs` and `@mentions` are stored as unresolved tokens and resolved against the whole state
  at query time, because resolution depends on what is known.
* The first device and the members' handles come from genesis (`founder`, `members`); handles are what
  `@mentions` resolve against. `DeviceBound` and `MemberJoined` arrive with invites in M1.

### Compaction (M6)
History matters most for the last few months and for active items. Compaction lets a device drop old
events without changing the folded state, and keeping everything stays a valid choice. It has two
parts: **item checkpoints** carry the state, and a **frontier** drops events. Neither needs a format for
the whole `State`.

The merge rules make this possible. Every merge is commutative, associative and idempotent, and the
state keeps the stamp of every register and set tag. So the folded state of an item is a valid base
for any later event, including a concurrent event that arrives months late. And a write that lost a
register to a greater `(hlc, EventId)` can never win again.

#### Item checkpoints
`ItemCheckpoint{item, state}` (tag 17) is an ordinary device-signed event. It is synced, admitted and
relayed like any other.

* **Covers:** every event that affects the item and lies in the checkpoint's causal past (`prev` and
  `seen`, transitively). That includes a `Supersedes` link written on another item and earlier
  checkpoints of the same item.
* **Coverage is a version vector** `{author -> seq}`: feeds are chains, so a causal past holds each feed
  up to some seq. Events of an author flagged for equivocation are never covered and stay as events.
* **State:** type, author, `created`, `updated`, every register with its stamp, every set element with
  its add tags (stamp, author, seq), and notes including their text. An old item is then one event.
* **Applying** a checkpoint merges it into the item: registers by stamp. In a set, a tag absent from the
  checkpoint whose event the checkpoint covers is removed. Plain union would bring back elements that
  the checkpoint had seen removed. Concurrent checkpoints of the same item, written by different devices,
  merge the same way.
* **Verification:** a peer that holds every covered event refolds the item from them and compares. On a
  mismatch it flags the checkpoint and folds the events instead. This is a flag, not an admission rule,
  because a peer without the covered events cannot check. A joiner trusts checkpoints, which is fine
  while members are trusted (v1).
* **Writing** is an authoring decision, so it may read the clock. The anchor checkpoints items without
  events for the policy's `[compaction] after` (for example `90d`); without an anchor, any device may.
  Duplicates are harmless.
* Some events need no checkpoint: a newer `AgentDefined` or `DeviceProfile` replaces the whole value, so
  older ones are covered by it. `SessionEnded` closes a session the same way.

#### Frontier
A device's **frontier** is a version vector below which it has dropped events. It may raise its frontier
when every event below the new frontier is either covered or **kept**:

| Kept below the frontier | Why |
|---|---|
| authority (`Genesis`, `InviteCreated`, `DeviceBound`, `MemberJoined`, `PolicySet`, `OwnerAdded`, `ForkAnnounced`) | admission depends on them, and they are few |
| the newest checkpoint of each item | it carries the state |
| uncovered events | they still change the state |
| unknown body tags | their effect is unknown |

* **Admission:** a parent counts as known when it is stored or when its `(author, seq)` is at or below
  the frontier. This is why `seen` carries author and seq.
* An event at or below the frontier that is not stored is ignored as already covered. Equivocation below
  the frontier goes undetected: an accepted cost.
* **Sync:** a peer whose frontier is ahead sends its kept events and its frontier for the gap. The
  receiver raises its own frontier for the part it cannot get. A joiner starts from the frontier of the
  peer it joins through.
* **Local choice:** each device decides whether and how far to compact. A device that never compacts is
  an archive. It serves full history and makes forks from before a frontier possible.
* `known` and the pending set shrink with the frontier, so memory stays bounded as well as disk.
* P11 becomes: state is derivable from the log, where after compaction the log means the kept events plus
  everything above the frontier.

Tests: for any cut, any checkpointing and any arrival order, applying checkpoints plus later events
gives the same state as folding the full log.

## 7. Forking

A fork is a **new genesis that points at a frozen point in the parent's history**.

```rust
struct Genesis {
    name: String,
    founder: PrincipalKey,
    policy: Policy,
    members: Vec<MemberSeed>,        // pre-seeded principals, devices, roles
    forked_from: Option<ForkPoint>,
}
struct ForkPoint { workforce: Hash, heads: Vec<EventId> }
```

* Parent events up to `heads` are a read-only prefix, validated under the parent's rules. They are
  admitted only when fetched by id from a known descendant, so nothing after the fork point gets in.
  Item ids, comments, reports and decisions carry over.
* Parent events after the fork point are ignored.
* After compaction the prefix is the parent's kept events plus its frontier, which the fork takes over.
  A fork point below a frontier needs an archive device that kept the full history.
* Excluded members and devices are absent from the new member list, so their connections are rejected
  at the iroh handshake and their new events are not admitted. A lost machine is excluded as a device.
* `--new-principal` founds the fork with a fresh principal key, for when the lost machine held it. The
  member seed lists the old key as `previous`.
* Anyone can fork. The forker becomes founder of the new workforce.
* When an owner forks without `--new-principal`, they publish `ForkAnnounced{to, members}` in the old
  workforce. In v1 devices follow by hand (`wf fork accept`), which writes the new id to
  `.workforce.toml`. The old workforce is archived locally.

This replaces revocation and recovery keys in v1.

```
wf fork [--exclude <device|member>...] [--new-principal] [--name <name>]
wf fork accept <id>
wf fork list
```

## 8. Items

The item is the **single universal document**: tasks, questions, decisions, meta work, and later
discussions are all items. Types differ only in configured states and behaviour, so comments, mentions,
references, search and inbox work the same everywhere. `items` is a reference for the model, not a
compatibility target.

### States and categories
Each type configures its own states. Each state has a free name and one of six fixed **categories**.
Built-in behaviour keys on categories, never on state names.

| Category | Meaning | Built-in behaviour |
|---|---|---|
| `inactive` | exists, not actionable (backlog, draft) | not claimable |
| `open` | actionable | claimable once dependencies are done |
| `active` | being worked on | has a session/claim; when the session ends without submitting (or its lease expires) the item returns to the type's `open` state |
| `blocked` | waiting on something **outside** the model (review, a human, CI, a vendor) | not claimable; entering it ends the session's claim; `@mentions`/`#refs` in the transition note say what it waits on; listed in `wf inbox` unless under review |
| `done` | finished successfully | terminal; satisfies dependents |
| `cancelled` | finished without success | terminal; open dependents become **stranded** (flagged, not unblocked) |

Blocking on another item is **computed** from `depends_on`, never stored as a state.

\[\text{ready}(i) \iff \text{cat}(i) = \texttt{open} \wedge \forall d \in \text{depends\_on}(i): \text{cat}(d) = \texttt{done} \wedge \text{no live session on } i\]

An open item that is not ready is **blocked** while some dependency is not done, and **stranded** when
some dependency is `cancelled`: waiting will not free it, so someone has to remove the dependency or
cancel the item. `wf item list` filters with `--ready`, `--blocked` and `--stranded` (combined, they match
any), and `wf status` flags stranded items.

`wf item show` prints an effective status, e.g. `open · blocked by #q7x2 (question, open)`.

A type's **open, active, blocked and done state** is the first state in its list with that category.
The runner moves items with these (start, release, submit, merge), so built-in behaviour still keys on
categories, never on state names.

### Archiving
`archived` is a flag, orthogonal to state, so an archived item keeps its terminal outcome.

* `ItemArchived` / `ItemUnarchived`, allowed only in `done` or `cancelled`.
* The fold still processes all events, but readers only get a **stub** for archived items (id, type, title,
  final state, archived time). Body, comments and history load from the log on demand. M0 still keeps the
  full item in memory behind that stub view: dropping bodies at fold time would make the state depend on
  the order of archive, unarchive and concurrent edits, so real stubs wait for compaction (M6).
* Hidden from lists, search and agent context by default (`--archived`). Dependencies on them still resolve.
* Reclaiming disk space is compaction (M6): an archived item is the typical candidate for an item
  checkpoint.

### Type configuration (part of the signed policy)

```toml
[types.task]
lane = "default"
fields = ["priority", "labels", "requires"]
depends_on = ["task", "question", "decision"]   # allowed target types
initial = "open"
review = "standard"                              # optional default review level (section 9)
transitions = "any"                              # later: explicit lists, guards, on_enter hooks
states = [
  { name = "backlog",     category = "inactive" },
  { name = "open",        category = "open" },
  { name = "in_progress", category = "active" },
  { name = "in_review",   category = "blocked" },
  { name = "done",        category = "done" },
  { name = "dropped",     category = "cancelled" },
]
```

If a policy change removes a state that items are still in, they keep it and are flagged as
"unknown state" until moved.

### Built-in types
Types the code relies on. Shipped in the default policy, names reserved, states fixed in v1.

| Type | States (category) | Purpose |
|---|---|---|
| `task` | backlog (inactive), open, in_progress (active), in_review (blocked), done, dropped (cancelled) | default unit of work; what agent sessions are assigned |
| `question` | open, needs_human (blocked), answered (done), dropped (cancelled) | an agent that needs an answer asks with `wf work ask`: the question is created, its item depends on it, and its session parks until it is answered. Questions go to the `supervision` lane, where a supervisor answers or moves them to `needs_human`; `initial = "needs_human"` sends them straight to the human |
| `decision` | proposed (blocked), ratified (done), rejected (cancelled), superseded (done) | project decisions; only `ratified` enters agent context |
| `distill` | open, in_progress (active), done | system-created meta work in the `meta` lane |

Custom types (`bug`, `doc`, `spike`, ...) are fully configurable.

### Priority
`priority` orders work; it is a field only of types that list it in `fields`. By default that is `task`
and `question`. A decision records an outcome and `distill` is housekeeping, so ordering them means
nothing. A custom type opts in with `fields = ["priority", ...]`.

| Level | Meaning |
|---|---|
| `P0` | Drop everything: it blocks the team, or something is broken. Picked first. |
| `P1` | Important; do it this cycle. |
| `P2` | Normal. The default. |
| `P3` | Nice to have; when nothing else is waiting. |

* **Required where it exists.** An item of a type with priorities must have one: `wf item add` sets `P2`
  unless told otherwise, and the author check refuses to create such an item without a priority or to clear
  it. Items of other types have none, and setting one is refused. Peers do not repeat the check (see
  authoring checks), so an item whose type gained the field later, or an old one, can lack a priority:
  `item show` flags it and it sorts last until one is set.
* **Ordering** of `item list` and `item list --ready`: priority (`P0` first), items without a priority last, then
  oldest first, then id.
* **Not decided:** letting an item inherit the highest priority of the items it blocks, so `ready` puts what
  unblocks urgent work first. It would make the order depend on the dependency graph; if added, it should
  be shown separately from the item's own priority.

### Relations
Relations that change behaviour are explicit events. Informational ones are derived from text.

| Relation | Source | Cardinality | Semantics |
|---|---|---|---|
| `depends_on` | explicit `ItemLinked` | many, acyclic | readiness; allowed targets per type |
| `supersedes` | explicit `ItemLinked` | one, decisions only | moves the target to `superseded` |
| `references` | derived from body and comments | many, cycles fine | backlinks only |
| `mentions` | derived from body and comments | many | inbox entries |

* **No parent/child.** Decomposition is "the big task depends on its parts", which also makes it ready
  for integration exactly when the parts are done. `item show` lists dependents as "required by".
  A label on the edge can be added later if "part of" needs to be distinguishable.
* `depends_on` is never derived from text: a stray `#ref` must not block work.

### Derived references and mentions
* `#k3f9a2` for items (any unique prefix of at least 4 characters, like git).
* `@joshua` for principals, `@joshua/coder` for agents, `@joshua/coder/s4k2` for sessions. Short forms
  (`@coder`, `@s4k2`) work when unique in the workforce.
* Parsed with `pulldown-cmark`, skipping code spans and blocks. Tokens that do not resolve are plain text.
* Derived in the fold with a versioned parser, not stored as events. A better parser means a refold.

A handle match wins: `@joshua` is a principal even if an agent is also called `joshua`. A short name that
matches no handle resolves to an agent only if exactly one agent has that name. Sessions arrive in M2.

**Mention resolution:**

| Written | Goes to |
|---|---|
| `@joshua` | the principal's inbox |
| `@joshua/coder` in a comment **on an item** | the live session of that agent on this item; otherwise the agent's inbox, picked up by its next session |
| `@joshua/coder` elsewhere | the agent's inbox |
| `@joshua/coder/s4k2` | that session; if it has ended, the agent's inbox with a note |

So `@coder please also handle the empty case` on an item reaches the session working on it, which checks
its inbox after each commit.

### Fields, events and merge rules

| Field | Merge rule |
|---|---|
| `type` | set at creation, immutable |
| `title`, `body`, `priority`, `lane`, `review` | last writer wins per field; losing concurrent body edits stay visible in `item show --history` |
| `state` | last writer wins. Not validated by peers: the author checks it against the policy before signing, and a state the policy does not know is flagged, not rejected (see *Admission and fold*) |
| `labels`, `requires` | set with add/remove; add wins over concurrent remove |
| `depends_on` | same set semantics; concurrent adds that form a cycle: the edge with the greatest `(hlc, EventId)` is ignored and flagged |

Events: `ItemCreated` (id = short base32 prefix of the event hash), `ItemEdited{changes}`,
`ItemTransitioned{to, note}`, `ItemLinked{kind, target, add|remove}`, `ItemArchived`, `ItemUnarchived`,
`Comment{item, body}`.

### Stored vs computed

| Stored (events) | Computed in the fold | Computed at query time |
|---|---|---|
| type, title, body, state, priority, lane, review, labels, requires, depends_on, supersedes, archived | category, author, created, updated, references, mentions, backlinks, `blocked_by`, cycle and unknown-state flags, sessions per item, claim holder, checklist progress (`- [ ]` items in the body) | session liveness, `ready`, review status, overlap notifications |

The fold is a pure function of events and never reads the clock. Everything that depends on "now" is
computed per query.

### Routing work: three separate axes
| Axis | Question | Mechanism |
|---|---|---|
| **scope** | who can *see* it | replication filter (*later*, v1 has one scope) |
| **lane** | who may *claim* it | lane policy, matched by role and kind |
| **requires** | who is *able* to | item `requires` vs. agent capabilities plus device hardware |

* Every item is in exactly one lane. The lane may derive from the item type (`distill`, `triage` go to
  `meta`, `question` to `supervision`). An agent takes items from the lanes in its definition.
* In v1 lanes are **routing, not security**: members are trusted.
* `wf item list --ready --for <agent>` and the dispatcher apply the same lane and requires filter.
* Judgment about what to work on next lives in items, not in the dispatcher: planner and supervisor
  agents create, split, order and unblock items, and the dispatcher follows the data.

### Assignment
**On one machine (M2)** each daemon dispatches for the agents its device hosts, woken by new events,
ended sessions and a slow tick. For each agent with `start = "auto"` and a free slot (device and
per-agent `max_sessions`, parked sessions included) it starts, in this order:

1. a review session for an item awaiting a review step the agent can take (section 9);
2. a work session for the first ready item in the agent's lanes that its capabilities and the device's
   hardware meet, in `wf item list --ready` order.

Agents with `start = "manual"` run only through `wf session start <item> --agent <name>`.

**Across devices (M3)** `dispatch` is the default; `pull` is configurable per workforce. The dispatch
target is an **agent**; the dispatcher picks any device that hosts it and has a free slot.

1. A device with free capacity requests work for an agent it hosts.
2. The anchor picks an item and signs `Assigned{item, agent, device, lease}`.
3. The device starts a session (`SessionStarted`) for that item.
4. If the anchor is unreachable, policy `fallback` decides: `optimistic` (the session starts with an
   unconfirmed `Claim`; conflicts resolve to the earliest causal claim, then lowest hlc, then lowest id)
   or `wait`.
5. Sessions renew with `Heartbeat`. A dead session's claim expires and the item is assignable again.

## 9. VCS

* Git only. A `VcsAdapter` trait is a code boundary with one implementation. Events carry a `vcs` enum
  with a single variant, so jj can be added later without a format change.
* One worktree and branch per **item**, `wf/<item>`, continued by every session on the item. Worktrees
  live outside the repo in `$XDG_DATA_HOME/workforce/<id>/worktrees/<item>` and are removed after the
  merge. A review session gets a detached checkout of the commit it reviews.
* Git is called through the `git` CLI behind the adapter.

### Review
Review levels are an ordered list in the policy, from least to most strict. Each level is a sequence of
steps; a step is an agent matched by capabilities or the human:

```toml
[[review]]
name = "light"
steps = [{ agent = { requires = ["review"] } }]

[[review]]
name = "critical"
steps = [{ agent = { requires = ["review", "strong-model"] } }, "human"]
```

* An item names a level in its `review` field, defaulting to its type's `review`. No level means no
  review. The planner, a supervisor or the working agent set it; a session may raise it but not lower it
  for an item its agent worked on.
* Submitting (`wf work submit`) moves the item to its type's blocked state and starts the level's first step.
  Steps run in order. A reviewer agent is always a different agent from the ones that worked on the
  item.
* Verdicts: `Approve` moves on to the next step; `RequestChanges` moves the item back to its open state,
  and the next session continues the branch with the review notes; `Escalate` (agents only) inserts a
  human step right after the current one. Human steps wait in `wf inbox`.
* A verdict names the commit it reviewed. What was reviewed is what gets merged; a new submission starts
  the level again.
* Merging needs no separate setting: a level with a human step waits for the human.

### Merge
`[merge] mode` decides how an approved item lands in `target`:

* **`local` (M2):** the daemon builds the merge with `git merge-tree --write-tree` and `git commit-tree`,
  without touching any checkout, then advances the target: fast-forward in a clean checkout of it, or a
  compare-and-swap `git update-ref`. A dirty checkout makes the merge wait (shown in `wf inbox`). A
  conflict sends the item back to work. Then `Merged{item, reviewed, commit}` and the item is done.
* **`origin` (M3):** a central origin (GitHub etc.). wf pushes approved branches and opens pull requests;
  **the origin decides merging** (branch protection). `wf` observes it and emits `Merged` when a
  `wf/<item>` branch lands in the target.
* **Later:** git bundles over iroh-blobs for peer-to-peer repositories; `git-remote-iroh` as prior art.

## 10. Reports

What the working agent says about what it is doing and why: progress, decisions made, options rejected. A
report belongs to the session that wrote it, and through the session to its item. An item can have several
sessions (an agent gives up, a review requests changes), and each report says which one it came from.

```rust
struct Report {
    session: SessionId,             // the item is the session's item
    note: String,
    step: Option<StepId>,           // the step this is about, named by the hash of its text
}
```

* Reports are written with `wf work report "<text>" [--step <number> [--done]]`, only inside a session.
  `--step` takes the number `wf item show` prints for the step, and is stored as the step's id. `--done` requires `--step` and also writes `StepDone` for it. Work sessions only: a review session
  reports without `--step`, since a reviewer does not tick the author's steps.
* The step link is plain data and is not checked when read. `--step` is checked against the item's body
  when the report is written, so a mistyped id writes nothing. If the step's text is edited later, the report
  keeps pointing at a hash that is no longer in the body, and it is shown without a step.
* The tick is its own event with its own rules, so a report never becomes invalid because a step changed.
* Reports are encouraged and never required: `wf work submit` does not check for them. Agent instructions ask
  for reports that stand on their own, saying what was chosen, what was rejected and why.
* Attribution comes from the event signature and the session.
* Redaction happens before signing.
* `wf item show` lists the comments and reports of an item together in time order. Each report shows its
  agent, role and short session id, and the step text while the step exists.
* A work package includes the reports of earlier sessions of the item.
* Nothing depends on commit messages. Overlap notifications (M3) come from the files commits touch, which
  are read from the repository and are not part of a report.

### Two kinds of decisions
| | Item decisions | Project decisions |
|---|---|---|
| Where | the item's reports | the project decision log |
| Written by | the working agent | agents, scribes, humans |
| Ratified | no | yes |
| Agent context | when working on that item | always, once ratified |

## 11. Project decisions and search (M4)

* Distilling threads and reports into project decisions is **normal work**: a `distill` item in the
  `meta` lane, claimed like any other. This avoids duplicate scribes.
* Decisions are items of the built-in `decision` type. Ratified decisions change only through a new
  decision that `supersedes` them.
* Only **ratified** decisions enter agent context. Unratified ones are searchable by humans.
* Search is local per peer, not synced: a derived, rebuildable index (tantivy for keywords, a local vector
  index with a local embedding model), fused by reciprocal rank. `wf search "..." --kind decision,comment --since 30d`.

## 12. Harness integration via ACP

The daemon is an ACP client (protocol v1). Each **wf session** drives one harness subprocess in its own
worktree. A device runs as many sessions as its `max_sessions` allows.

Harnesses are detected on `PATH` and probed with ACP `initialize` (installed, authenticated). wf never
handles harness credentials.

| ACP | Use |
|---|---|
| `session/new` + MCP servers | start in the item's worktree with `WF_SESSION` set; attach the `wf` MCP server (`item_show`, `report`, `inbox`, `search`, `decision_propose`, M3) |
| `session/prompt` | send the work package: item, dependencies, answers, review notes of earlier attempts, the agent rules; later ratified decisions and search hits |
| end of a prompt turn | decides the next step (below) |
| `session/update` | transcript capture (local, `wf session watch`), redaction (M3), presence |
| `session/request_permission` | answered by the agent's `permissions`: allow, deny, or ask, held in `wf inbox` until `wf session allow\|deny` |
| `terminal/*`, `fs/*` | not offered in M2, harnesses use their own tools; later run inside wf's environment with secret injection |
| `session/cancel` | `wf session stop`, `max_duration`, claim or lease lost, or item reassigned |

**Turn end.** When a prompt turn returns, the runner looks at the session and its item:

| Situation | Next |
|---|---|
| the session ended (`wf work submit`, `wf work release`) or the item entered a blocked state | close the harness |
| the item waits on a dependency (its question) | park; prompt again with the answers once the item is ready, end `TimedOut` after `park_timeout` |
| otherwise, nudges left | prompt "continue": submit, ask or release when done or stuck |
| `nudges` turns in a row without a new commit | end `Released`, with a comment |

| Harness | ACP command |
|---|---|
| `opencode` | `opencode acp` |
| `claude-code` | `claude-agent-acp` |
| `codex` | `codex-acp` |
| `gemini-cli` | `gemini --experimental-acp` |

* Pin ACP v1 and keep it behind a `Harness` trait; a `raw-cli` fallback (M3) starts any agent with a
  prompt file and observes commits.
* ACP-level sandboxing only covers what the agent routes through the client; real isolation is OS-level
  (donor mode, *later*).
* `wf attach <session>` over an iroh ALPN `wf/acp/1` (M5).
* The CLI stays. MCP tools and CLI are thin wrappers over the same daemon API.
* *Later:* a scheduler hook, an external program asked over JSON lines at the dispatch, turn-end and
  permission decisions, for policies the agent settings cannot express.

## 13. Secrets and transcripts

* Transcripts (the stored ACP update stream) stay **local and unpublished by default**.
  `publish = none | redacted | full` is a per-workforce policy (*later*).
* `wf` **brokers** secrets, it does not store them. Sources: `secretspec.toml`, 1Password `op://`
  references, the system keyring. Values are injected into the harness environment.
* wf keeps the injected values in memory only and uses them to scrub: exact match plus base64, URL and hex
  encodings, then pattern scanning (gitleaks-style) as a second layer.
* Reports always pass redaction before signing.

## 14. Connectivity and lifecycle

### Creating
`wf init --name <name>` inside a git clone: writes genesis, `.workforce.toml` (only the id), reads the
origin from the clone, detects harnesses and offers one default agent per harness, named by role (`coder`).

### Joining
Two invite kinds, one command:

| Kind | Created by | Effect |
|---|---|---|
| `device` | the principal (principal key required) | adds another machine of yours, same identity |
| `member` | an owner | a new person with their own principal and handle |

User's view, adding the cloud VM:

```
$ wf invite device                     # on a machine holding the principal key
Invite code (valid 24h, single use):  wfi1qk7m3...x9ta

$ cd ~/src/workforce && wf join wfi1qk7m3...x9ta     # on the VM, inside an existing clone
✓ Connected to workforce "workforce" via laptop (direct)
✓ This machine joins as a device of @joshua
Machine name [vm-hetzner-1]: ⏎
Detected harnesses:  [x] opencode (authenticated)  [x] claude-code (not logged in)  [ ] codex (missing)
Host your agents here:  [x] @joshua/coder  [ ] @joshua/reviewer (codex missing)
Max concurrent sessions [1]: 2
Run wf in the background (systemd user service)? [Y/n] ⏎
✓ Daemon running. This machine takes work for: @joshua/coder
```

Everything is also available as flags for headless and Nix setups:
`wf join <code> --yes --name vm --agents coder --max-sessions 2 --daemon`.

Under the hood:
1. The code contains the workforce id, the id of the `InviteCreated` event, the endpoint ids of a few
   members, and the invite secret key. Long codes, no rendezvous infrastructure.
2. **Invites are self-authorizing.** At invite time the principal (or an owner, for member invites) signs
   `InviteCreated{kind, invite: InviteKey, expires}` with the invite's public key. The joiner writes its
   own `DeviceBound` (or `MemberJoined` with a new principal key) as the first event of its feed, with a
   signature by the invite key over its own device key. Any peer verifies this offline; no owner needs to
   be online, the new device never sees the principal key, and nothing in the log could bind another device.
3. **Expiry and single use are enforced at the handshake** by the member serving `wf/join/1`, not by
   admission: they need a clock and coordination, and admission must not depend on either. A leaked code
   redeemed through two members that cannot see each other binds both devices, on every peer alike, and
   is flagged; the fix is `wf fork --exclude <device>`.
4. The joiner dials `wf/join/1` on any reachable member, proves the invite, syncs the log, writes its
   `DeviceBound`, then publishes its `DeviceProfile`.
5. Gossip topics derive from the workforce id; bootstrap peers are the known members' endpoint ids.
6. Non-member endpoints are rejected at the handshake.
7. State lives in `$XDG_DATA_HOME/workforce/<id>/`. The repo holds only `.workforce.toml` with the id.

Defaults use n0's public relays and discovery. `--relay <url>` for self-hosted, mDNS for LAN-only.

## 15. Policy

Stored in the log as principal-signed `PolicySet`. A repo copy is optional and **has no effect on its own**.

```
wf policy edit | diff <file> | apply <file> | export
```

```toml
[assignment]
mode = "dispatch"        # dispatch | pull
fallback = "optimistic"  # optimistic | wait
lease = "30m"

[merge]
mode = "local"           # local | origin (M3)
target = "main"

[[review]]               # ordered, least strict first; an item without a level is not reviewed
name = "light"
steps = [{ agent = { requires = ["review"] } }]
[[review]]
name = "standard"
steps = [{ agent = { requires = ["review", "strong-model"] } }]
[[review]]
name = "critical"
steps = [{ agent = { requires = ["review", "strong-model"] } }, "human"]

[lanes.default]
claim = ["member"]
[lanes.meta]
claim = ["member"]
types = ["distill", "triage"]
[lanes.supervision]
claim = ["member"]
types = ["question"]

[transcripts]
publish = "none"

[compaction]             # M6: when the anchor writes item checkpoints
after = "90d"

[governance]
mode = "advisory"
```

## 16. iroh usage

| Need | Piece |
|---|---|
| connectivity, NAT traversal, relay fallback, E2E encryption | `iroh` endpoint, dial by `EndpointId` |
| restrict to members | endpoint hooks rejecting unknown ids |
| announce heads, presence | `iroh-gossip` (hashes and heads only, payloads over `wf/sync/1`) |
| large artifacts (later) | `iroh-blobs` |

**Not iroh-docs:** it is a mutable key-value store, not a log. Each `(author, key)` keeps only its
latest value by wall clock, and a write removes the author's older entries under the same prefix, so a
device could erase its own events on every replica. That breaks the append-only log, equivocation
evidence and admission against the causal past. Its namespace secret is a bearer credential shared by
every writer, and it is pre-1.0. Adopting it would remove only `wf/sync/1`, while admission, fold,
equivocation, fork and invites stay ours. A state-based model on iroh-docs (one key per item field) would
be simpler, but gives up history, offline authorization and causal conflict resolution (P3, P4, P6,
P9). We write `wf/sync/1` ourselves (about 1-2k lines); it also keeps the log independent of transport.
Keep the shape compatible with Willow ideas (author as subspace, `kind/id` as path) in case that matures.

Missing and built by us: authorization, authenticated gossip membership, ordering, store-and-forward
(the anchor), compaction (later), search, blob access control (later).

## 17. CLI (v1 surface)

```
wf init | invite device|member | join <code> | members | owner add|transfer
wf device list|edit                       # name, hardware, hosted agents, max_sessions
wf agent add|list|edit                    # profiles: harness, model, capabilities, lanes, autonomy
wf session start|list|show|stop|watch     # live and recent sessions, transcripts
wf session allow|deny <session>           # answer a held permission request
wf work submit | ask <question> | release  # agents, inside a session
wf daemon | sync | status | log | export
wf fork | fork accept | fork list
wf policy edit|diff|apply|export
wf item add|list|show|edit|move|link|archive|diff   # all types: task, question, decision, ...
wf comment <item> <text>
wf work report "<text>" [--step <number> [--done]]
wf activity | inbox
wf review <item> --approve|--request-changes|--escalate
wf search <query>
```
All commands support `--json`. Output is `{"schema": 1, "data": ...}`; the schema number is bumped on
incompatible changes and the shapes are snapshot-tested. Item ids print as the first 6 characters of their
base32 hash and any unique prefix of at least 4 characters is accepted, with or without `#`.
Without `--json`, lists (`item list`, `agent list`, `log`, `mentions`) print as borderless tables
with bold headers. Every table is built by one function in `wf-cli`, so the look is defined in one place.
Styling is emitted only when stdout is a terminal; piped output is plain.

`wf item edit <item>` without flags opens a markdown draft in `$VISUAL` or `$EDITOR`: TOML front matter
between `+++` fences (`title`, `priority`, `lane`, `labels`, `requires`, `review`), then the body as plain markdown,
taken verbatim. TOML has no null, so unset fields appear as commented-out hints. Unknown keys, an
invalid priority or label, and a missing or unclosed fence are rejected. Flags (`--title`, `--body`,
`--add-label`, ...) edit single fields instead. The item type and state are not editable here.

Keys are hex files with mode `0600` under `$XDG_DATA_HOME/workforce/keys/`; the repository holds only
`.workforce.toml` with the workforce id.

## 18. Milestones

| M | Scope |
|---|---|
| **M0** | Event log with `actor`, `owner`/`member`, principal and device keys, agent definitions, item model (types, categories, built-in types, relations, derived refs and mentions, archiving), in-memory fold, redb event store with snapshots. Single peer. |
| **M1** | iroh sync, equivocation handling, self-authorizing device invites and `wf join`, device profiles, daemon, gossip, `wf fork`. |
| M2 | Agents at work on one machine: sessions, worktrees and branches per item, ACP harness runner, turn-end handling and parking, local dispatcher, questions and supervisors, review levels, local merge, permission modes, `wf inbox`, checklist progress, `wf work report` and `Report` |
| M3 | Sessions across devices: anchor dispatch, claims and leases, `raw-cli` fallback, `wf` MCP server, session inboxes, overlap notifications, secret brokering, redaction, merge mode `origin` with `Merged` from origin. **= UC1** |
| M4 | Decision ratification flow, distill items, hybrid search |
| M5 | Multiple principals, threads, scopes, `wf attach` |
| M6 | Git bundles over iroh-blobs, peer-to-peer repos, compaction (item checkpoints, frontier) |
| M7 | Donors: revocation, rate limits, sandbox, transcript publishing, governance |

## 19. Open questions
1. Should merges to the main branch require a human key once origin-side protection is not enough?
   (v1: a review level with a human step in `local` mode, the origin in `origin` mode.)
2. Fork following for non-owner forks: ask-first UX.
3. Prior art to read: **Radicle**, **p2panda**, Keyhive, Matrix state resolution (before M7).
4. Do heartbeats belong in the log (M3)? One per minute for three sessions is about 130k events a
   month, the main source of log growth. Liveness is computed against the clock at query time anyway, so
   a lease renewed over gossip plus a final `SessionEnded` might be enough. M2 needs neither: on one
   machine the daemon knows its sessions. Settle this before M3; it decides how urgent compaction is.
5. With several principals (M5), should review steps be able to require a different principal than
   the author (the former `distinct_principal`)?
