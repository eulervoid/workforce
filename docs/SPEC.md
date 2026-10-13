# workforce (`wf`): spec, draft v0

> A peer-to-peer, CLI-first coordination layer for coding agents and the humans who work with them.

Status: draft. wf grows through four **stages**, each trusting more parties than the one before.
The current target is **stage 1**; everything marked *later* is designed for, but not built.

## 1. Stages

| Stage | Who | Machines | Milestones |
|---|---|---|---|
| **1** | single user | a single machine | M0, M2, M2.1 built; M2.2 makes it fit for daily use |
| **2** | single user | several trusted machines | M1 (sync, invites, fork) built; M3 (sessions across machines) next |
| **3** | a trusted team | several trusted machines | M5 |
| **4** | untrusted participants | several, possibly untrusted machines | M7, a possible end goal |

Stage 4 is far off and may never happen. Designs keep it in mind, so that they do not rule it out,
but it does not drive them: until then members and their machines are trusted (section 5).

### Stage 1: single user, single machine (current)
One developer with agents on their own machine, working on a real project every day.

* Nothing to run besides `wf daemon`; no network is needed.
* Off-path cases are handled visibly: a session that fails, stalls or is interrupted never leaves an
  item stuck without saying so.
* A daemon restart (for example after rebuilding wf) costs no work, and no agent context with an
  agent that can resume its sessions.

### Stage 2: single user, several trusted machines
A laptop (sleeps, roams), a cloud VM (always on), a home desktop (GPU, behind a NAT). Each runs agents.

* Machines sit behind different NATs with changing IPs. No port forwarding, no VPN.
* The laptop is often offline. Work continues and merges back when it reconnects.
* All machines are the same human. One principal with several device keys.
* Work should go to the machine that fits (GPU, OS, harness, model).
* Two agents should not silently pick up the same item.

### Stage 3: a trusted team, several trusted machines
* Every change traces back to a principal.
* Review by a different principal than the author is a useful default.
* Async, multi-timezone: lots of "why did we decide X?" lookups.
* Not every transcript is meant for everyone.

### Stage 4: untrusted participants, possibly untrusted machines
* Donated agent work: needs revocation, rate limits, sandboxing, probation.
* Donors can take part in discussions. Governance is advisory.
* Prompt injection through issue text is a real risk on donor machines.

### Cross-cutting
1. The only thing all peers must agree on is **project state**: items, claims, comments, decisions, membership.
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
* **Snapshots:** the folded state is serialized to redb, with the number of events it has applied. Startup
  loads the latest snapshot and applies newer events. Large bodies (comments) stay in the log
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

**Principal always means the accountable person**, and nothing else: who owns the keys, whom agents
act for, who decides a `principal` step of a review or recovery level, whom blockers escalate to.
Stages 1 and 2 have one principal; a team (stage 3) has several. Code, CLI and docs say *principal*
for this concept, never *human*, *user* or *person*; a step that waits on a principal names no
particular one until stage 3, when it may (`{ principal = "@joshua" }`).

* **Principal key** (ed25519): one per human. Signs authority events: owner actions, invites,
  device bindings, policy. Kept on disk for M0 (`$XDG_DATA_HOME/workforce/keys/`); a 1Password or
  ssh-agent hook comes later.
* **Device key** (ed25519): one per machine. Signs all ordinary events. It is the iroh `EndpointId`.
  Never copied between machines.
* **Agents have no keys.** The device signs, and every event carries an `actor`:

```rust
enum Actor {
    Principal(PrincipalKey),
    Agent { agent: AgentId, session: SessionId },
    Daemon(PrincipalKey),     // what the daemon of a principal's device does by itself
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
park_timeout = "2h"       # how long a session waits on a blocker or dependency before it ends
max_duration = "4h"       # optional limit per session
permissions = "allow"     # allow | ask | deny: answers to the harness's permission requests; a machine
                          # that runs agents without a sandbox asks instead of allowing
instructions = "..."      # optional: told in every brief, before the item (how strictly to review, ...)
```

* Named by role, not by harness, so the harness can change later.
* Names are unique per principal and immutable in v1, so mentions keep resolving. The other fields can
  change: a new `AgentDefined` for an existing name replaces the definition (last writer wins).
* Agent definitions are work configuration, not authority, so a device key may sign them. Agent actors
  cannot write them, so an agent cannot raise its own autonomy.
* An agent whose `lanes` include `supervision` is a **supervisor**: it resolves the blockers of other
  agents or escalates them to the principal (section 10). It has no other rights; lanes route, members are
  trusted.

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

* `SessionStarted{agent, item, role}` (device-signed), `role` = `Work`, `Review{commit}`, `Supervise`
  or `Diagnose` (a step of a recovery level, M2.2).
  Its hash prefix is the session id. `SessionEnded{reason}`, reason = `Submitted{commit} | Reviewed |
  Supervised | Released | TimedOut | Failed | Cancelled | Interrupted | Diagnosed`.
* One branch per **item** (`wf/<item>`, section 9) in one worktree. A session is an attempt: the next
  session on the item (after a release, a crash or requested changes) continues the branch.
* **Ending moves the item first.** `after_end(session, reason)` in `wf-core` says what happens to the
  item (back to open, to the review state, merged and done, or nothing), and the daemon does it
  while the session still holds the item, then writes `SessionEnded`. Otherwise the dispatcher would see
  an open item without a live session in between and start another. An item that is no longer being
  worked on is not reopened by a stale end. The daemon writes such a move as `ItemMoved{by_session}`
  naming the session, so the move is attributed to the start or end that caused it. The same holds for
  the move to the active state when a work session starts.
* A work session ends when the agent submits (`wf work submit`), releases (`wf work release`), or runs out
  of nudges or time. Waiting on a **blocker** (an open blocking comment, section 10, such as its own
  question) or on a dependency **parks** it: the harness keeps its conversation and continues once the item
  is ready again, or ends after the agent's `park_timeout`.
* A supervise session ends when the supervisor resolves the blocker it was started for, or escalates it.
* Context carries over within a session in the conversation, and across sessions through comments.
* **Liveness:** on one machine (M2) a session is live from `SessionStarted` until `SessionEnded`. A
  daemon that stops leaves its sessions live: it cancels their turns and closes the harnesses. The next
  daemon resumes each in the agent's own conversation (ACP `session/resume`, or `session/load` with the
  replayed history left out of the transcript), by the id the agent gave the session, which the daemon
  keeps beside the transcript. A session that was working is told that wf restarted and what others
  wrote on its item since its last prompt; a parked one parks again. When the agent can do neither,
  the session ends `Interrupted`, no failure of the attempt. Across devices (M3) liveness is computed
  at query time from a lease: a session without a renewal within the lease counts as dead, even without
  `SessionEnded` (crash, sleeping laptop). Its claim expires and the item becomes assignable again,
  possibly to the same agent on another device.
* The claim belongs to the earliest live work session of an item by `(hlc, EventId)`; a later one
  cancels itself (M3: two daemons can only start sessions on one item at the same time across devices).
  On one machine `wf session start` refuses an item that has a live work session.
* Agents run `wf` with `WF_SESSION` and `WF_WORKFORCE` set by the runner, so their events carry
  `Actor::Agent{agent, session}` and they address the workforce of the session, whatever the committed
  `.workforce.toml` of their checkout says.
* Location transparency stops at uncommitted work. Reassignment carries over only what was pushed, which
  is why agents commit small.
* wf session ids are separate from ACP session ids; the ACP id is a local detail of the runner.

### Roles (v1)
| Role | Can do |
|---|---|
| `owner` | everything: invites, policy, `ForkAnnounced`, naming other owners |
| `member` | items, claims, comments, reviews, decisions |

Agent actors act for their principal with the member's rights, minus a few **guards** (authoring checks
against mistakes and shortcuts, not attacks): a session cannot resolve a blocker it wrote, cannot lower
the review level of an item its agent worked on, cannot move its own item to a done state (it submits),
and cannot write `AgentDefined` or authority bodies. A reviewer or supervisor agent never worked on the
item it reviews or supervises.

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
    actor: Actor,            // principal, agent session or daemon acting through this device
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
| Items | `ItemCreated`, `ItemEdited`, `ItemMoved`, `ItemLinked`, `ItemArchived`, `ItemUnarchived`, `Comment` | device |
| Work | `SessionStarted`, `SessionEnded`, `Review`, `Merged` (M2); `Assigned`, `Claim`, `Heartbeat` (M3) | device (`Assigned` by the anchor) |

Decisions are an item type, not a separate event family. Questions, requested changes and reports are
comments (section 10).

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
               actor: Actor                       // Principal(PrincipalKey) | Agent{agent: AgentId, session: SessionId} | Daemon(PrincipalKey)
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
| 6 | `DeviceProfile` | `name`, `hardware` (requirements in one string, separated by commas or spaces), `agents: {agent -> max_sessions}`, `max_sessions` (M1) |
| 7 | `ForkAnnounced` | `Signed<ForkAnnounced>`: `to: WorkforceId`, `members` (M1) |
| 8 | `AgentRemoved` | `name`: the acting principal's agent is gone until it is defined again; its sessions and their events stay valid (M2.2) |
| 10 | `ItemCreated` | `item_type`, `title`, `body`, `priority`, `lane`, `labels`, `requires`, `review: Option<ReviewName>` |
| 11 | `ItemEdited` | `item`, `changes: Vec<Change>` (`Title`, `Body`, `Priority`, `Lane`, `Labels`, `Requires`, `Review`, `StepDone`, `Recovery`) |
| 12 | `ItemMoved` | `item`, `to`, `by_session: Option<SessionId>` (the session whose start or end the move carries out), `because: Option<EventId>` (the comment that says why) |
| 13 | `ItemLinked` | `item`, `link`: `DependsOn(SetChange<ItemId>)` or `Supersedes(ItemId)` |
| 14 | `ItemArchived` | `item` |
| 15 | `ItemUnarchived` | `item` |
| 16 | `Comment` | `item`, `text`, `kind`: `Plain`, `Blocker` or `ChangeRequest`, `resolves: Vec<EventId>`, `step: Option<StepId>` (see *Comments*) |
| 17 | `ItemCheckpoint` | `item`, `state`: the folded item with stamps, add tags and comment text (M6, see *Compaction*) |
| 20 | `SessionStarted` | `agent: AgentName`, `item`, `role`: `Work`, `Review{commit: Option<CommitId>}` or `Supervise` (M2) |
| 21 | `SessionEnded` | `session: SessionId`, `reason: EndReason` (M2, see *Sessions*) |
| 22 | `Review` | `item`, `commit: Option<CommitId>` (`None`: work submitted without commits), `verdict`: `Approve`, `RequestChanges` or `Escalate`, `because: Option<EventId>` (M2) |
| 23 | `Merged` | `item`, `reviewed: CommitId`, `commit: CommitId` (M2) |

* `Signed<T>` = `statement: T`, `signer: PrincipalKey`, `signature: Signature`.
* `MemberSeed` = `principal`, `handle`, `devices: Vec<DeviceKey>`, `previous: Vec<PrincipalKey>`. The
  founder is `owner`; seeded members are `member`. `previous` lists keys the member used in the parent
  workforce of a fork, so prefix events resolve to the same handle.
* `SetChange<T>` = `Add(T)` or `Remove{value: T, observed: Vec<EventId>}`. A removal lists the add events
  its author had seen, so a concurrent add survives it (add wins) without any ancestry index.
* An item id is the id of its `ItemCreated` event. Policy and `Genesis` carry TOML text, not a parsed
  structure, so the stored policy is exactly what the owner wrote. Peers do not parse it to admit
  the event: a policy a version cannot read (written by a newer one) is flagged in `wf status`, and
  that version keeps running on the last policy it could read, or the default.
* `CommitId` = `Git(GitOid)`, the single `vcs` variant (section 9).
* Bodies for later milestones (`Assigned`, `Claim`, `Heartbeat`, ...) take new tags. Tag 24 (`Report`,
  now a comment) is retired.

#### Local storage (redb)
One file per workforce at `$XDG_DATA_HOME/workforce/<workforce id>/store.redb`. Values are the postcard
encodings above; redb itself stores bytes.

| Table | Key | Value |
|---|---|---|
| `events` | event id (32 bytes) | encoded `Event` |
| `feeds` | `(author, seq)` | event ids (multimap: more than one only after equivocation). An append's `prev` must be stored |
| `log` | position | event id, in the order events were applied |
| `pending` | event id | encoded `Event` still waiting for parents |
| `meta` | `snapshot` | the snapshot blob |

A snapshot blob is `(format: u32, ref parser version: u16, State)`. It is a cache: any mismatch of
format or parser version, or a snapshot that is ahead of the log or disagrees with it, discards it and the state is refolded
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
* An item's `state` is empty until its first move and means "the type's initial state under the
  current policy", so item creation does not depend on concurrent policy changes.
* `supersedes` also writes `superseded` into the target's `state` register with the link's stamp. It
  is the only link whose event changes another item, which compaction has to account for.
* `depends_on` cycles from concurrent links are resolved at query time, not stored: edges are taken in
  `(hlc, EventId)` order and an edge that would close a cycle is ignored and flagged.
* Comment text stays in the log. The fold keeps who wrote each comment, its kind, what it resolves, its
  step and the tokens parsed from it. Resolving is grow-only (a resolved comment never reopens), so
  concurrent resolutions converge. `#refs` and `@mentions` are stored as unresolved tokens and resolved against the whole state
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
  its add tags (stamp, author, seq), and comments including their text. An old item is then one event.
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
  Item ids, comments and decisions carry over.
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

The item is the **single universal document**: tasks, decisions, meta work, and later discussions are
all items. Types differ only in configured states and behaviour, so comments, mentions, references,
search and inbox work the same everywhere. A question is not an item: it is a blocking comment on the item
it is about (section 10). `items` is a reference for the model, not a
compatibility target.

### States and categories
Each type configures its own states. Each state has a free name and one of six fixed **categories**.
Built-in behaviour keys on categories, never on state names.

| Category | Meaning | Built-in behaviour |
|---|---|---|
| `inactive` | exists, not actionable (backlog, draft) | not claimable |
| `open` | actionable | claimable once dependencies are done |
| `active` | being worked on | has a session/claim; when the session ends without submitting (or its lease expires) the item returns to the type's `open` state |
| `review` | the work is submitted and waits for its review steps | not claimable; entering it ends the session's claim |
| `done` | finished successfully | terminal; satisfies dependents |
| `cancelled` | finished without success | terminal; open dependents become **stranded** (flagged, not unblocked) |

Being blocked is **computed**, never stored as a state: from `depends_on`, and from **blockers**, the
open blocking comments on the item (section 10).

\[\text{ready}(i) \iff \text{cat}(i) = \texttt{open} \wedge \forall d \in \text{depends\_on}(i): \text{cat}(d) = \texttt{done} \wedge \text{no open blocker on } i \wedge \text{no live session on } i\]

The live-session condition ignores a session parked on the item: it is the one that continues once the
item is ready again.

An open or active item is **blocked** while some dependency is not done or some blocker is open, and
**stranded** when some dependency is `cancelled`: waiting will not free it, so someone has to remove the
dependency or cancel the item. `wf item list` filters with `--ready`, `--blocked` and `--stranded`
(combined, they match any), and `wf status` flags stranded items.

`wf item show` prints an effective status, e.g. `open · blocked by #x7k2 (task, open)` or
`in_progress · 1 blocker → supervisor (c7f2)`.

A type's **open, active, review and done state** is the first state in its list with that category.
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
depends_on = ["task", "decision"]               # allowed target types
initial = "open"
review = "standard"                              # optional default review level (section 9)
transitions = "any"                              # later: explicit lists, guards, on_enter hooks
states = [
  { name = "backlog",     category = "inactive" },
  { name = "open",        category = "open" },
  { name = "in_progress", category = "active" },
  { name = "in_review",   category = "review" },
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
| `task` | backlog (inactive), open, in_progress (active), in_review (review), done, dropped (cancelled) | default unit of work; what agent sessions are assigned |
| `decision` | proposed (review), ratified (done), rejected (cancelled), superseded (done) | project decisions; ratifying is the approval of the review level (`manual` by default), objections are change requests; only `ratified` enters agent context |
| `distill` | open, in_progress (active), done | system-created meta work in the `meta` lane |

Custom types (`bug`, `doc`, `spike`, ...) are fully configurable.

### Priority
`priority` orders work; it is a field only of types that list it in `fields`. By default that is only
`task`. A decision records an outcome and `distill` is housekeeping, so ordering them means
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
| `references` | derived from body and comments | many, cycles fine | backlinks only, to an item or a comment |
| `mentions` | derived from body and comments | many | inbox entries |

* **No parent/child.** Decomposition is "the big task depends on its parts", which also makes it ready
  for integration exactly when the parts are done. `item show` lists dependents as "required by".
  A label on the edge can be added later if "part of" needs to be distinguishable.
* `depends_on` is never derived from text: a stray `#ref` must not block work.

### Derived references and mentions
* `#k3f7a2` for items (any unique prefix of at least 4 characters, like git), `#k3f7a2/c7f2` for a
  comment on an item (section 10).
* `@joshua` for principals, `@joshua/coder` for agents, and with session inboxes (M3)
  `@joshua/coder/s4k2` for sessions. Short forms (`@coder`) work when unique in the workforce.
* Parsed with `pulldown-cmark`, skipping code spans and blocks. Tokens that do not resolve are plain text.
* Derived in the fold with a versioned parser, not stored as events. A better parser means a refold.

A handle match wins: `@joshua` is a principal even if an agent is also called `joshua`. A short name that
matches no handle resolves to an agent only if exactly one agent has that name.

**Mention resolution:**

| Written | Goes to |
|---|---|
| `@joshua` | the principal's inbox |
| `@joshua/coder` in a comment **on an item** | the live session of that agent on this item; otherwise the agent's inbox, picked up by its next session |
| `@joshua/coder` elsewhere | the agent's inbox |
| `@joshua/coder/s4k2` (M3) | that session; if it has ended, the agent's inbox with a note |

So `@coder please also handle the empty case` on an item reaches the session working on it, which checks
its inbox after each commit.

### Fields, events and merge rules

| Field | Merge rule |
|---|---|
| `type` | set at creation, immutable |
| `title`, `body`, `priority`, `lane`, `review` | last writer wins per field; losing concurrent body edits stay visible in the activity of `item show` (section 10) |
| `state` | last writer wins. Not validated by peers: the author checks it against the policy before signing, and a state the policy does not know is flagged, not rejected (see *Admission and fold*) |
| `labels`, `requires` | set with add/remove; add wins over concurrent remove |
| `depends_on` | same set semantics; concurrent adds that form a cycle: the edge with the greatest `(hlc, EventId)` is ignored and flagged |

Events: `ItemCreated` (id = short base32 prefix of the event hash), `ItemEdited{changes}`,
`ItemMoved{to, by_session, because}`, `ItemLinked{kind, target, add|remove}`, `ItemArchived`,
`ItemUnarchived`, `Comment{item, text, kind, resolves, step}`.

### Stored vs computed

| Stored (events) | Computed in the fold | Computed at query time |
|---|---|---|
| type, title, body, state, priority, lane, review, labels, requires, depends_on, supersedes, archived, comments | category, author, created, updated, references, mentions, backlinks, `blocked_by`, open blockers and change requests, cycle and unknown-state flags, sessions per item, claim holder, checklist progress (`- [ ]` items in the body) | session liveness, `ready`, review status, overlap notifications |

The fold is a pure function of events and never reads the clock. Everything that depends on "now" is
computed per query.

### Routing work: three separate axes
| Axis | Question | Mechanism |
|---|---|---|
| **scope** | who can *see* it | replication filter (*later*, v1 has one scope) |
| **lane** | who may *claim* it | lane policy, matched by role and kind |
| **requires** | who is *able* to | item `requires` and agent review or recovery steps vs. agent capabilities plus device hardware |

* Every item is in exactly one lane. The lane may derive from the item type (`distill`, `triage` go to
  `meta`). An agent takes items from the lanes in its definition. The `supervision` lane holds no items:
  agents in it take blockers (section 10).
* In v1 lanes are **routing, not security**: members are trusted.
* `wf item list --ready --for <agent>` and the dispatcher apply the same lane and requires filter.
* Judgment about what to work on next lives in items, not in the dispatcher: planner and supervisor
  agents create, split, order and unblock items, and the dispatcher follows the data.

### Assignment
**On one machine (M2)** each daemon dispatches for the agents its device hosts, woken by new events,
ended sessions and a slow tick. For each agent with `start = "auto"` and a free slot (device and
per-agent `max_sessions`, parked sessions included) it starts, in this order:

1. a review session for an item awaiting a review step the agent can take (section 9);
2. a supervise session for an item with an open blocker routed to supervisors, if the agent is in the
   `supervision` lane and never worked on the item (section 10);
3. a work session for the first ready item in the agent's lanes that its capabilities and the device's
   hardware meet, in `wf item list --ready` order.

Agents with `start = "manual"` run only through `wf session start <item> --agent <name>`.

**Across devices (M3)** `dispatch` is the default; `pull` will be configurable per workforce, in an
`[assignment]` table with `mode`, `fallback` and `lease`. The dispatch
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
* One worktree and branch per **item**, `wf/<item>`, continued by every session on the item, where
  `<item>` is the first 12 characters of the item id (short ids are too short to name a branch safely).
  Worktrees live outside the repo in `$XDG_DATA_HOME/workforce/<id>/worktrees/<item>` and are removed
  after the merge, or at once when a session submits without commits. A review
  session gets a detached checkout of the commit it reviews in `worktrees/<item>-<commit prefix>`,
  removed when the session ends.
* Git is called through the `git` CLI (2.38 or later) behind the adapter, and `wf-vcs` is the only
  crate that does. Whichever worktree it is given, the adapter works on the main checkout of the
  repository.
* The daemon finds the repository from a file next to the store of the workforce, which every `wf`
  command run in the main checkout refreshes. Checkouts of sessions do not count: they come and go.

### Review
Review levels are an ordered list in the policy, from least to most strict. Each level is a sequence of
steps; a step is an agent matched by capabilities or the principal:

```toml
[[review]]
name = "light"
steps = [{ agent = { requires = ["review"] } }]

[[review]]
name = "critical"
steps = [{ agent = { requires = ["review", "strong-model"] } }, "principal"]
```

* An item names a level in its `review` field, defaulting to its type's `review` (`manual` for `task`
  and `decision`). A type without one, like an item on the level `none`, is not reviewed.
  `wf item edit --review <level>` sets it and `--review default` goes back to the type's level; `default`
  is therefore not a level name. The planner, a supervisor or the working agent set it; a session may
  raise it but not lower it for an item its agent worked on.
* Submitting (`wf work submit`) moves the item to its type's review state and starts the level's first
  step. It is refused while the item has an open change request. A submission without commits is
  reviewed too; its reviewer gets the history of the item and an empty directory. When the last step
  approves, the dispatcher moves the item to its done state (after the merge, if there are commits),
  which unblocks what depends on it. An item that someone moved after the submission is left where it is.
  Steps run in order. A reviewer agent never did the work on the item; the same reviewer looks again
  after changes. The status (`Awaiting{step}`, `ChangesRequested`, `Approved`, `Merged`) is computed
  from the latest submitted commit, the level and the verdicts on that commit; nothing is stored.
* `wf review <item>` writes the verdict; in a review session it also ends the session `Reviewed`. A
  verdict on a commit other than the submitted one is refused, and a review session comments on the item,
  not on its steps. `wf item diff` shows the work as a reviewer sees it, through git so the user's
  pager and diff settings apply, and `wf item checkout` puts the main checkout on it (detached, since
  the branch is checked out in the item's worktree). *Later:* `wf review --annotate` turns comments
  written in a diff tool such as lumen into change requests.
* Verdicts: `Approve` moves on to the next step, and is refused while a change request is open;
  `RequestChanges` needs at least one open change request, written by the reviewer (section 10), and
  moves the item back to the first open state of its type in the same event, as the fold applies it;
  the next session continues the branch and resolves them;
  `Escalate` (agents only) inserts a principal step right after the current one. Principal steps wait in
  `wf inbox`. A verdict may carry a plain comment as its `because`.
* On the next review, the reviewer sees how each change request was resolved. A resolution it does not
  accept gets a new change request; resolved comments never reopen.
* A verdict names the commit it reviewed. What was reviewed is what gets merged; a new submission starts
  the level again.
* Merging needs no separate setting: a level with a principal step waits for the principal.

### Merge
`[merge] mode` decides how an approved item lands in `target`:

* **`local` (M2):** the daemon builds the merge with `git merge-tree --write-tree` and `git commit-tree`,
  without touching any checkout, then advances the target: fast-forward in a clean checkout of it, or a
  compare-and-swap `git update-ref`. A dirty checkout makes the merge wait: the item sits in its review
  state with one comment, `wf inbox` shows it, and the dispatcher tries again on every change and
  tick without saying so again. A conflict sends the item back to work with a comment naming the
  files, and the next session merges the target into the branch first. Then `Merged{item, reviewed,
  commit}`, the branch and worktree are removed, and the item is done. Work without a review level
  merges as its session ends; approved work is merged by the dispatcher, one merge per item at a time.
* **`origin` (M3):** a central origin (GitHub etc.). wf pushes approved branches and opens pull requests;
  **the origin decides merging** (branch protection). `wf` observes it and emits `Merged` when a
  `wf/<item>` branch lands in the target.
* **Later:** git bundles over iroh-blobs for peer-to-peer repositories; `git-remote-iroh` as prior art.

## 10. Comments

All text about an item is a comment: discussion, questions, requested changes, the reasons for a move or
a verdict, and what the working agent reports. There is no second kind of note.

```rust
struct Comment {
    item: ItemId,
    text: String,
    kind: CommentKind,
    resolves: Vec<EventId>,         // blockers and change requests this comment resolves
    step: Option<StepId>,           // the step this is about, named by the hash of its text
}
enum CommentKind { Plain, Blocker, ChangeRequest }
```

```
wf comment <item> "<text>"
wf comment <item> "<text>" --block              # a blocker
wf comment <item> "<text>" --request-change     # a change request
wf comment <item> "<reply>" --resolve <comment> [--resolve <comment> ...]
wf comment <item> "<reply>" --resolve-blockers | --resolve-change-requests
wf comment <item> "<text>" --step <number> [--done]
```

### Blockers and change requests
Both stay **open** until a later comment resolves them. They differ in what they hold up:

| Kind | Purpose | While open | Goes to |
|---|---|---|---|
| **Blocker** | the work cannot start or continue until someone else answers or acts | the item is not ready; a work session on it parks (section 5) | whoever it `@mentions`; otherwise a supervisor; otherwise the principal of the agent that wrote it, or the author if a principal |
| **Change request** | something the work must do before it is submitted | `wf work submit` and `Approve` are refused | whoever works on the item next; the work brief lists it |

* A question from an agent is a blocker on its own item: `wf comment <item> "<question>" --block`. Its
  session parks, and resumes with every comment written while it was parked once the blocker is resolved.
  So does a wait on something outside the model (credentials, CI, a vendor).
* A blocker without a mention goes to the `supervision` lane: the dispatcher starts a **supervise
  session** for an agent in that lane that never worked on the item. It gets the item's history and an
  empty directory, and ends `Supervised` when it either resolves the blocker with an answer, or escalates
  it with a new blocker that `@mentions` the principal and resolves the old one. Without a supervisor, the
  blocker goes straight to the principal. Blockers waiting on a principal are listed in `wf inbox`.
* Change requests come from reviewers (section 9) and from anyone who wants something done before the
  work is submitted. The working agent resolves each once it is done, saying how or naming the commit.
* **Resolving:** any comment can resolve open blockers and change requests of its item, including a
  blocker or change request itself (an escalation). `--resolve-blockers` and `--resolve-change-requests`
  expand to the ids open when the command runs, and the event stores those ids: a blocker written
  concurrently on another device is never resolved by a reply that did not see it. A resolving comment
  may have empty text. Anyone may resolve, except that a session cannot resolve a blocker it wrote.
* Resolution is grow-only: there is no reopening. A resolution someone does not accept gets a new
  blocker or change request.

### Comment ids
A comment's id is its event id. It prints as the first **4** characters of its base32 hash, scoped to the
item: `--resolve c7f2` on `#rpdhor`, or `#rpdhor/c7f2` from anywhere else, which also makes a backlink
to the comment. A longer prefix is accepted, and an ambiguous one is refused, as with items.

* Fixed length, not the shortest unique prefix: a shortest prefix grows when a later comment collides,
  and ids already printed or pasted would stop matching.
* Not sequence numbers (`c1`, `c2`): two devices commenting concurrently would both write the same
  number, and the numbering would shift as their events sync.

### Reports
What the working agent says about what it is doing and why: progress, decisions made, options rejected. A
report is a comment written by a work session, so it belongs to that session and through it to the item.
An item can have several sessions (an agent gives up, a review requests changes), and the activity shows
which one each comment came from.

* `--step` takes the number `wf item show` prints for the step, and is stored as the step's id. `--done`
  requires `--step` and also writes `StepDone` for it. Review and supervise sessions comment without
  `--step`, since they do not tick the author's steps.
* The step link is plain data and is not checked when read. `--step` is checked against the item's body
  when the comment is written, so a mistyped number writes nothing. If the step's text is edited later,
  the comment keeps pointing at a hash that is no longer in the body, and it is shown without a step.
* The tick is its own event with its own rules, so a comment never becomes invalid because a step changed.
* Reports are encouraged and never required: `wf work submit` does not check for them. Agent instructions
  ask for reports that stand on their own, saying what was chosen, what was rejected and why.
* Attribution comes from the event signature and the session.
* Redaction happens before signing (M3; until then comments are signed as written).
* A work brief includes the comments of earlier sessions of the item.
* Nothing depends on commit messages. Overlap notifications (M3) come from the files commits touch, which
  are read from the repository and are not part of a comment.

### Activity
`wf item show` ends with the item's **activity**: one timeline of everything that happened to it, in
fold order. It replaces a separate history listing.

```
activity · 2026-10-11
  06:32  @joshua created the task
  06:52  @joshua/coder/pp74s3 started work
  06:52  @joshua/coder/pp74s3 ⧗ blocker → supervisor                            c7f2
           Item has two steps with no concrete work … What change do you want made?
  06:55  @joshua/supervisor/k3j1 ⧗ blocker → @joshua · resolves c7f2            a91d
           No decision covers this; it's about intent.
  06:59  @joshua resolved a91d                                                  e04b
           Fix the formatting of the body, nothing else
  06:59  @joshua/coder/pp74s3 resumed
  07:04  @joshua/coder/pp74s3 submitted 3e8a1c → in_review
  07:10  @joshua/reviewer/m2p9 requested changes on 3e8a1c
           △ b2e4  parser.rs: the error path drops the span
           △ d81c  missing test for empty input
  07:31  @joshua/coder/pp74s3 resolved b2e4, d81c                               f13a
           Kept the span in ParseError; added empty_input test.
```

* **Comments** take a header line (time, actor, kind, what they resolve, id) and their wrapped text.
  Change requests written together with a verdict are listed under it.
* **Events** take one line: moves (`moved to backlog`; with a `because`, the comment's text below),
  edits (`set priority P1`, `labels +bug`, `review → principal`, `edited the body`, `checked step 2: …`),
  links (`now depends on #x7k2`, `no longer depends on #x7k2`), sessions (`started work`,
  `started review of 3e8a1c`, `released`, `timed out`), verdicts, merges and archiving.
* **Moves carried out for a session** (`by_session`) are shown in the session's line, not on their own:
  `started work`, `submitted 3e8a1c → in_review`, `released → open`.
* **Other items** appear where they change this one: a dependency done (`#x7k2 done`) or cancelled
  (`#x7k2 dropped · stranded`), a new dependent (`#abc now depends on this`), a reference
  (`referenced in #abc`).
* A body edit that lost to a concurrent edit is shown with its text, so nothing silently disappears.
* The date heads the section and is repeated when the day changes; lines show the time only.
* The item header lists what is open: `2 open: ⧗ c7f2 blocker → supervisor · △ d81c change request`.
* `--json` gives the activity as a list of entries tagged by kind.

### Two kinds of decisions
| | Item decisions | Project decisions |
|---|---|---|
| Where | the reports (comments) of the item's work sessions | the project decision log |
| Written by | the working agent | agents, scribes, humans |
| Ratified | no | yes |
| Agent context | when working on that item | always, once ratified |

## 11. Project decisions and search (M4)

* Distilling threads and comments into project decisions is **normal work**: a `distill` item in the
  `meta` lane, claimed like any other. This avoids duplicate scribes.
* Decisions are items of the built-in `decision` type. Ratified decisions change only through a new
  decision that `supersedes` them.
* Only **ratified** decisions enter agent context. Unratified ones are searchable by humans.
* Search is local per peer, not synced: a derived, rebuildable index (tantivy for keywords, a local vector
  index with a local embedding model), fused by reciprocal rank. `wf search "..." --kind decision,comment --since 30d`.

## 12. Harness integration via ACP

The daemon is an ACP client (protocol v1). Each **wf session** drives one harness subprocess in its own
worktree. A device runs as many sessions as its `max_sessions` allows.

Harnesses are detected on `PATH` by their ACP command; probing them with ACP `initialize` (installed,
authenticated) is *later*. wf never handles harness credentials.

| ACP | Use |
|---|---|
| `session/new` + MCP servers | start in the item's worktree with `WF_SESSION` set; attach the `wf` MCP server (`item_show`, `comment`, `inbox`, `search`, `decision_propose`, M3) |
| `session/prompt` | send the brief: item, dependencies, comments of earlier sessions, open change requests, the agent rules; later ratified decisions and search hits |
| end of a prompt turn | decides the next step (below) |
| `session/update` | transcript capture (local, `wf session watch`), redaction (M3), presence |
| `session/request_permission` | answered by the agent's `permissions`: allow, deny, or ask, which goes through the permission chain below |
| `terminal/*`, `fs/*` | not offered in M2, harnesses use their own tools; later run inside wf's environment with secret injection |
| `session/cancel` | `wf session stop`, `max_duration`, claim or lease lost, or item reassigned. Not when the agent ended its own session (`submit`, `release`, a verdict): it then gets 20 seconds to finish its turn, because the command it ran is still waiting for its answer |

**Turn end.** When a prompt turn returns, the runner looks at the session and its item:

| Situation | Next |
|---|---|
| the session ended (`wf work submit`, `wf work release`, a verdict, a supervisor's resolution) | close the harness |
| the item has an open blocker or waits on a dependency | park; prompt again with the comments written meanwhile once the item is ready, end `TimedOut` after `park_timeout` |
| otherwise, nudges left | prompt "continue": submit, write a blocker or release when done or stuck |
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
* *Later:* a scheduler hook, an external program asked at the dispatch and turn-end decisions, for
  policies the agent settings cannot express.

**Hooks.** Every hook wf runs speaks JSON. wf writes one object to the hook's stdin, naming the hook
and its version (`{"hook": "permission", "version": 1, ...}`), and reads one object from its stdout.
Both sides ignore fields they do not know.

### Permission requests (M2.1)
A request of an agent whose permissions are `ask` passes through a chain of deciders, in the order the
machine config lists them, until one answers. Each one allows, denies or passes the request on:

* **`rules`**: built-in. Running `wf` alone is allowed, because it is how a session reports back:
  its session commands, help for any command (`wf … --help`), and lists of these joined by `;` or
  `&&`, each optionally piped into filters such as `grep`.
* **`hook`**: a program that gets the request on stdin and prints
  `{"permission": "allow" | "deny" | "ask", "reason": ...}`. `ask`, a crash or a timeout passes on.
* **`model`**: a decision model such as Jev, called over the System One API at a configured
  endpoint `url` (TypeSafe, OpenRouter, Cloudflare Workers AI, ...). Asked as a `noul`, its probability that the call
  should be allowed is compared with two thresholds: allow above one, deny below the other, pass on in
  between. Asked as a `choice` between allow, deny and ask, its pick is taken above a minimum
  confidence. Its API key comes from an environment variable.
* **`agent`**: an agent that the daemon keeps one live session of, prompted with the request, the
  item and the guidance. It answers like a hook.

What every decider passes on goes to a person, through `wf inbox`, `wf watch` or
`wf session allow|deny`. Without a config the chain is `rules`, then the person. Deciders are set in the
machine config (`$XDG_CONFIG_HOME/workforce/config.toml`), never in the signed policy: policy that syncs
must not make other machines run programs or call paid APIs. Each decision, its decider and its reason
go into the transcript of the session that asked.

## 13. Secrets and transcripts

* Transcripts (the stored ACP update stream) stay **local and unpublished by default**.
  `publish = none | redacted | full` is a per-workforce policy (*later*).
* `wf` **brokers** secrets, it does not store them. Sources: `secretspec.toml`, 1Password `op://`
  references, the system keyring. Values are injected into the harness environment.
* wf keeps the injected values in memory only and uses them to scrub: exact match plus base64, URL and hex
  encodings, then pattern scanning (gitleaks-style) as a second layer.
* Comments written by agent sessions always pass redaction before signing.

## 14. Connectivity and lifecycle

### Creating
`wf init --name <name>` inside a git clone: writes genesis, `.workforce.toml` (only the id), reads the
origin from the clone, and lists the harnesses found on PATH with the `wf agent add` command for each; it
defines no agent. `wf init -i` asks which harnesses to define agents for, with their permissions and model.

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
[merge]
mode = "local"           # local | origin (M3)
target = "main"

[[review]]               # ordered, least strict first
name = "none"
steps = []               # no steps: nothing waits for approval
[[review]]
name = "light"
steps = [{ agent = { requires = ["review"] } }]
[[review]]
name = "standard"
steps = [{ agent = { requires = ["review", "strong-model"] } }]
[[review]]
name = "critical"
steps = [{ agent = { requires = ["review", "strong-model"] } }, "principal"]
[[review]]
name = "manual"          # the strictest: a principal decides alone
steps = ["principal"]

[[recovery]]             # what happens when a session ends without finishing (M2.2)
name = "none"
steps = ["principal"]
[[recovery]]
name = "standard"        # tasks: try twice more, then the principal decides
steps = ["retry", "retry", "principal"]
[[recovery]]
name = "diagnose"        # an agent says what went wrong, before the one retry
steps = [{ agent = { requires = ["review"] } }, "retry", "principal"]

[lanes.default]
[lanes.meta]
[lanes.supervision]      # holds no items: its agents supervise blockers

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
wf init [-i] | status | log | export | import <file> | members
wf policy show|edit|apply <file>          # apply and edit need an owner's principal key
wf fork [--exclude <device|@member>]... [--new-principal] | fork accept <id> | fork list
wf item add|list|show|edit|move|cancel|link|check|archive|unarchive   # all types: task, decision, ...
wf item list [--type] [--state] [--label] [--lane] [--priority] [--ready [--for <agent>]] [--blocked] [--stranded] [--group-by]
wf item diff|checkout|path <item>         # the work through git; the main checkout on it; its worktree
wf comment <item> <text> [--block | --request-change] [--resolve <comment>... | --resolve-blockers | --resolve-change-requests] [--step <number> [--done]]
wf mentions <@handle | @handle/agent>
wf review <item> --approve|--request-changes|--escalate [--note]
wf inbox                                  # blockers, reviews, merges, recovery, permission requests
wf watch [--item <item>]                  # events as they arrive; answer what waits on you
wf agent add|list|edit|remove             # harness, model, capabilities, lanes, instructions, autonomy
wf harness models <harness>               # the models a harness offers
wf session start|list [--live]|show|stop|watch|allow|deny
wf work brief | submit | release          # agents, inside a session
wf permissions test                       # run the permission chain on a request
wf daemon start|stop|status|run
wf sync [--serve] | invite device | join <code> | device list|edit
```
*Later:* `wf invite member`, `wf owner add|transfer`, `wf policy diff|export`, `wf search <query>`.

All commands support `--json`. Output is `{"schema": 4, "data": ...}`; the schema number is bumped on
incompatible changes and the shapes are snapshot-tested. Item ids print as the first 6 characters of their
base32 hash and any unique prefix of at least 4 characters is accepted, with or without `#`.
Without `--json`, lists (`item list`, `agent list`, `session list`, `device list`, `log`, `mentions`)
print as borderless tables with bold headers, and say so when they are empty; times are local. Every table is built by one function in `wf-cli`, so the look is defined in one place.
Styling is emitted only when stdout is a terminal; piped output is plain.

`wf item edit <item>` without flags opens a markdown draft in `$VISUAL` or `$EDITOR`: TOML front matter
between `+++` fences (`title`, `priority`, `lane`, `labels`, `requires`, `review`, `recovery`), then the body as plain markdown,
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
| M2 | Agents at work on one machine: sessions, worktrees and branches per item, ACP harness runner, turn-end handling and parking, local dispatcher, blockers, change requests and supervisors, review levels, local merge, permission modes, `wf inbox`, checklist progress, reports as comments, item activity |
| M2.1 | Convenience at the CLI: machine config, permission chain (rules, hooks, decision models, agents), `wf item diff` through git, `wf item checkout\|path`, `wf watch` |
| M2.2 | Daily use on one machine: recovery levels, no silent stalls, daemon management, export, network off for a single device, CLI polish. **= stage 1** |
| M3 | Sessions across devices: anchor dispatch, claims and leases, `raw-cli` fallback, `wf` MCP server, session inboxes, overlap notifications, secret brokering, redaction, merge mode `origin` with `Merged` from origin. **= stage 2** |
| M4 | Decision ratification flow, distill items, hybrid search |
| M5 | Multiple principals, threads, scopes, `wf attach`. **= stage 3** |
| M6 | Git bundles over iroh-blobs, peer-to-peer repos, compaction (item checkpoints, frontier) |
| M7 | Donors: revocation, rate limits, sandbox, transcript publishing, governance. **= stage 4** |

A TUI with the live state of the workforce comes after M2.1, built on what `wf watch` shows.

## 19. Open questions
1. Should merges to the main branch require a principal key once origin-side protection is not enough?
   (v1: a review level with a principal step in `local` mode, the origin in `origin` mode.)
2. Fork following for non-owner forks: ask-first UX.
3. Prior art to read: **Radicle**, **p2panda**, Keyhive, Matrix state resolution (before M7).
4. Do heartbeats belong in the log (M3)? One per minute for three sessions is about 130k events a
   month, the main source of log growth. Liveness is computed against the clock at query time anyway, so
   a lease renewed over gossip plus a final `SessionEnded` might be enough. M2 needs neither: on one
   machine the daemon knows its sessions. Settle this before M3; it decides how urgent compaction is.
5. With several principals (M5), should review steps be able to require a different principal than
   the author (the former `distinct_principal`)?
