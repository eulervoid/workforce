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
1. The only thing all peers must agree on is **project state**: items, claims, progress, decisions, membership.
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
| P11 | **Derivable:** all workforce state is derivable from the log plus the repo. No daemon-only state. |

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

### Storage
* **The event log is the only durable truth.** It is a key-value workload (append, get by `EventId`,
  range by `(author, seq)`, heads, pending) and lives in **redb**: pure Rust, one file, copy-on-write
  B+trees, one writer with concurrent MVCC readers, stable file format.
* **State is folded into in-memory Rust structs** by `wf-core`. Queries are typed Rust, not SQL. Changing
  the state shape needs no migration, only a refold.
* **Snapshots:** the folded state is serialized to redb, keyed by the heads it covers. Startup loads the
  latest snapshot and applies newer events. Large bodies (comments, progress messages) stay in the log
  and are loaded on demand by `EventId`.
* **Derived indexes** (search, M3) are rebuildable and never a second source of truth.
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
```

* Named by role, not by harness, so the harness can change later.
* Names are unique per principal and immutable in v1, so mentions keep resolving. The other fields can
  change: a new `AgentDefined` for an existing name replaces the definition (last writer wins).
* Agent definitions are work configuration, not authority, so a device key may sign them.

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
A **session** is one run of one agent on one device, working on **exactly one item**. It is the unit
that holds a claim.

* `SessionStarted{agent, item}` (device-signed). Its hash prefix is the session id.
  `SessionEnded{reason: done | released | failed | cancelled}`.
* session = claim = worktree = branch (`wf/<item>/<session>`).
* When the item is finished the session ends; the next item starts a new session. Context carries over
  through the progress log, not a long-lived conversation.
* **Liveness is computed at query time** from `Heartbeat` events. A session without a heartbeat within the
  lease counts as dead, even without `SessionEnded` (crash, sleeping laptop). Its claim expires and the
  item becomes assignable again, possibly to the same agent on another device.
* Location transparency stops at uncommitted work. Reassignment carries over only what was pushed, which
  is why agents commit small.
* wf session ids are separate from ACP session ids; the ACP id is a local detail of the runner.

### Roles (v1)
| Role | Can do |
|---|---|
| `owner` | everything: invites, policy, `ForkAnnounced`, naming other owners |
| `member` | items, claims, comments, progress, reviews, decisions |

Hard rule: `policy.*`, `invite.*`, `owner.*` and device-binding events are valid only when signed by a
**principal key** (owner principal where the role requires it). No device key, so no agent, can create them.

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
    deps: Vec<EventId>,      // heads seen when writing
    hlc: Hlc,                // tie-breaks only, never authority
    body: EventBody,
    sig: Signature,
}
// EventId = blake3(canonical encoding of everything incl. sig)
```

* **Encoding:** canonical postcard (deterministic, compact). Signature covers the encoding without `sig`. See *Encoding and data format* below for the decision and the exact layout.
* **Sync:** exchange version vectors `{author -> max_seq}`, stream missing ranges.
* **Equivocation:** two events with the same `(author, seq)` are proof of misbehavior. v1 records and
  surfaces it; it has no automatic effect.
* **Unknown event types** are stored, relayed, and ignored by the fold, so newer peers can add types.
* **Fold order:** the causal order from `deps`, ties broken by `(hlc, EventId)`.

### Event bodies (v1)
| Group | Bodies | Signed by |
|---|---|---|
| Authority | `Genesis`, `InviteCreated`, `MemberJoined`, `DeviceBound`, `PolicySet`, `OwnerAdded`, `ForkAnnounced` | principal key (carried in a device feed) |
| Identity | `AgentDefined`, `DeviceProfile` | device |
| Items | `ItemCreated`, `ItemEdited`, `ItemTransitioned`, `ItemLinked`, `ItemArchived`, `ItemUnarchived`, `Comment` | device |
| Work | `SessionStarted`, `SessionEnded`, `Assigned`, `Claim`, `Heartbeat`, `Progress`, `Review`, `Merged` | device (`Assigned` by the anchor) |

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
               deps: Vec<EventId>                 // sorted, deduplicated
               hlc: Hlc
               body: EventBody
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
* `hlc` of an event is greater than the `hlc` of its `prev` and every dep. The fold orders concurrent
  writes by `(hlc, event id)`, which therefore extends causal order.

#### Body tags
Tags are explicit and never reused. An unknown tag decodes to `Unknown{tag, payload}`, is stored and
relayed unchanged, and is ignored by the fold. A known tag with an undecodable payload makes the whole
event invalid.

| Tag | Body | Payload |
|---|---|---|
| 1 | `Genesis` | `Signed<Genesis>`: `name`, `founder: MemberSeed`, `members: Vec<MemberSeed>`, `policy` (TOML text), `forked_from: Option<ForkPoint>` |
| 2 | `PolicySet` | `Signed<PolicySet>`: `policy` (TOML text, stored verbatim) |
| 3 | `AgentDefined` | `name`, `harness`, `model: Option<String>`, `capabilities`, `lanes` |
| 10 | `ItemCreated` | `item_type`, `title`, `body`, `priority`, `lane`, `labels`, `requires` |
| 11 | `ItemEdited` | `item`, `changes: Vec<Change>` (`Title`, `Body`, `Priority`, `Lane`, `Labels`, `Requires`) |
| 12 | `ItemTransitioned` | `item`, `to`, `note: Option<String>` |
| 13 | `ItemLinked` | `item`, `link`: `DependsOn(SetChange<ItemId>)` or `Supersedes(ItemId)` |
| 14 | `ItemArchived` | `item` |
| 15 | `ItemUnarchived` | `item` |
| 16 | `Comment` | `item`, `body` |

* `Signed<T>` = `statement: T`, `signer: PrincipalKey`, `signature: Signature`.
* `MemberSeed` = `principal`, `handle`, `devices: Vec<DeviceKey>`. The founder is `owner`; seeded
  members are `member`.
* `SetChange<T>` = `Add(T)` or `Remove{value: T, observed: Vec<EventId>}`. A removal lists the add events
  its author had seen, so a concurrent add survives it (add wins) without any ancestry index.
* An item id is the id of its `ItemCreated` event. Policy and `Genesis` carry TOML text, not a parsed
  structure, so the stored policy is exactly what the owner wrote.
* Bodies for later milestones (`SessionStarted`, `Heartbeat`, `Progress`, ...) take new tags.

#### Local storage (redb)
One file per workforce at `$XDG_DATA_HOME/workforce/<workforce id>/store.redb`. Values are the postcard
encodings above; redb itself stores bytes.

| Table | Key | Value |
|---|---|---|
| `events` | event id (32 bytes) | encoded `Event` |
| `feeds` | `(author, seq)` | event id. Appends must be gapless per author |
| `heads` | author | `(seq, event id)` of the latest event |
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
version; device signature; `workforce` matches (genesis names none); all parents (`prev` and `deps`)
are known; the feed is gapless and unforked; `hlc` is greater than every parent's; the author is a bound
device and the actor belongs to its principal; authority bodies carry a valid principal signature of an
owner for this workforce; referenced items exist; no item links to itself.
* Events with missing parents (or arriving before genesis) wait in a **pending set**, capped at 10,000
  events, and are applied when their parents arrive. Any arrival order and any duplication give the same
  state; this is tested with generated event sets.
* A second event with the same `(author, seq)` is rejected, not recorded.

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
* `supersedes` also writes `superseded` into the target's `state` register with the link's stamp.
* `depends_on` cycles from concurrent links are resolved at query time, not stored: edges are taken in
  `(hlc, EventId)` order and an edge that would close a cycle is ignored and flagged.
* Comment and transition-note text stays in the log. The fold keeps who wrote it and the tokens parsed
  from it. `#refs` and `@mentions` are stored as unresolved tokens and resolved against the whole state
  at query time, because resolution depends on what is known.
* The first device and the members' handles come from genesis (`founder`, `members`); handles are what
  `@mentions` resolve against. `DeviceBound` and `MemberJoined` arrive with invites in M1.

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

* Parent events up to `heads` are a read-only prefix, validated under the parent's rules. Item ids,
  comments, progress and decisions carry over.
* Parent events after the fork point are ignored.
* Excluded members are absent from the new member list, so their connections are rejected at the iroh handshake.
* Anyone can fork. The forker becomes founder of the new workforce.
* When the old owner forks, they publish `ForkAnnounced{to, members}` in the old workforce. Included
  members' daemons follow automatically if the announcement is owner-signed, otherwise they ask
  (`wf fork accept`). The old workforce is archived locally.

This replaces revocation and recovery keys in v1.

```
wf fork [--exclude <member|principal>...] [--name <name>]
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
| `active` | being worked on | has a session/claim; lease expiry returns it to the type's `open` state |
| `blocked` | waiting on something **outside** the model (review, CI, a vendor) | not claimable; entering it ends the session's claim; `@mentions`/`#refs` in the transition note say what it waits on |
| `done` | finished successfully | terminal; satisfies dependents |
| `cancelled` | finished without success | terminal; dependents are flagged, not unblocked |

Blocking on another item is **computed** from `depends_on`, never stored as a state.

\[\text{ready}(i) \iff \text{cat}(i) = \texttt{open} \wedge \forall d \in \text{depends\_on}(i): \text{cat}(d) = \texttt{done} \wedge \text{no live session on } i\]

`wf item show` prints an effective status, e.g. `open · blocked by #q7x2 (question, open)`.

### Archiving
`archived` is a flag, orthogonal to state, so an archived item keeps its terminal outcome.

* `ItemArchived` / `ItemUnarchived`, allowed only in `done` or `cancelled`.
* The fold still processes all events, but readers only get a **stub** for archived items (id, type, title,
  final state, archived time). Body, comments and history load from the log on demand. M0 still keeps the
  full item in memory behind that stub view: dropping bodies at fold time would make the state depend on
  the order of archive, unarchive and concurrent edits, so real stubs wait for compaction (M5).
* Hidden from lists, search and agent context by default (`--archived`). Dependencies on them still resolve.
* Reclaiming disk space is compaction (M5); archiving is what makes it safe.

### Type configuration (part of the signed policy)

```toml
[types.task]
lane = "default"
fields = ["priority", "labels", "requires"]
depends_on = ["task", "question", "decision"]   # allowed target types
initial = "open"
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
| `question` | open, answered (done), dropped (cancelled) | an agent that needs an answer creates one, makes its task depend on it and ends its session; the task is ready again when it is answered |
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
* **Ordering** of `item list` and `item ready`: priority (`P0` first), items without a priority last, then
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
| `title`, `body`, `priority`, `lane` | last writer wins per field; losing concurrent body edits stay visible in `item show --history` |
| `state` | last writer wins. Not validated by peers: the author checks it against the policy before signing, and a state the policy does not know is flagged, not rejected (see *Admission and fold*) |
| `labels`, `requires` | set with add/remove; add wins over concurrent remove |
| `depends_on` | same set semantics; concurrent adds that form a cycle: the edge with the greatest `(hlc, EventId)` is ignored and flagged |

Events: `ItemCreated` (id = short base32 prefix of the event hash), `ItemEdited{changes}`,
`ItemTransitioned{to, note}`, `ItemLinked{kind, target, add|remove}`, `ItemArchived`, `ItemUnarchived`,
`Comment{item, body}`.

### Stored vs computed

| Stored (events) | Computed in the fold | Computed at query time |
|---|---|---|
| type, title, body, state, priority, lane, labels, requires, depends_on, supersedes, archived | category, author, created, updated, references, mentions, backlinks, `blocked_by`, cycle and unknown-state flags, sessions per item | session liveness, `ready`, overlap notifications |

The fold is a pure function of events and never reads the clock. Everything that depends on "now" is
computed per query.

### Routing work: three separate axes
| Axis | Question | Mechanism |
|---|---|---|
| **scope** | who can *see* it | replication filter (*later*, v1 has one scope) |
| **lane** | who may *claim* it | lane policy, matched by role and kind |
| **requires** | who is *able* to | item `requires` vs. agent capabilities plus device hardware |

* Every item is in exactly one lane. The lane may derive from the item type (`distill`, `triage` go to `meta`).
* In v1 lanes are **routing, not security**: members are trusted.
* `wf item ready --for-me` and the dispatcher apply the same lane and requires filter.

### Assignment
`dispatch` is the default; `pull` is configurable per workforce.

The dispatch target is an **agent**; the dispatcher picks any device that hosts it and has a free slot
(device and per-agent `max_sessions`).

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
* One worktree per **session**, branch `wf/<item>/<session>`. Worktrees live outside the repo in
  `$XDG_DATA_HOME/workforce/<id>/worktrees/<session>`. Removed when the session ends if everything was
  pushed, kept and flagged otherwise.
* **Phase 1:** a central origin (GitHub etc.). **The origin decides merging** (branch protection).
  `wf` observes it and emits `Merged{item, commit}` when a `wf/<item>/...` branch lands in the target.
  `Review` events are coordination signals and gate nothing.
* **Later:** git bundles over iroh-blobs for peer-to-peer repositories; `git-remote-iroh` as prior art.

## 10. Progress log

The "what is being changed right now" feed, tied to items. Small commits make small reports.

```rust
struct Progress {
    item: ItemId,
    session: Option<SessionId>,     // None for human commits
    commit: CommitInfo,             // raw, always present
    parsed: Option<ParsedNotes>,    // best effort
}
struct CommitInfo {
    vcs: VcsKind, id: CommitId, parents: Vec<CommitId>,
    message: String,                // verbatim, after redaction
    author: String, timestamp: Timestamp,   // informational only
    touches: Vec<PathPrefix>, diffstat: DiffStat,
}
struct ParsedNotes { parser: u16, decisions: Vec<String>, project_decisions: Vec<String> }
```

* No validation of message content: any commit yields a valid event.
* `parsed` comes from a trailer parser and may be absent. Peers may re-parse from `message`.
* Attribution comes from the event signature, not the git author.
* Redaction happens before signing.
* Convention (docs and agent instructions, not code): `Decision:` and `Project-Decision:` trailers.
* `wf progress "..."` for notes without a commit.
* `wf activity` is a live feed. When two live sessions' `touches` overlap, both sessions get an inbox entry.
* Sessions check their inbox after each commit; that is where comments and overlap notices arrive.

### Two kinds of decisions
| | Item decisions | Project decisions |
|---|---|---|
| Where | the item's progress log | the project decision log |
| Written by | the working agent | agents, scribes, humans |
| Ratified | no | yes |
| Agent context | when working on that item | always, once ratified |

## 11. Project decisions and search (M3)

* Distilling threads and progress into project decisions is **normal work**: a `distill` item in the
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
| `session/new` + MCP servers | start in the session's worktree, attach the `wf` MCP server (`item_show`, `progress`, `inbox`, `search`, `decision_propose`) |
| `session/prompt` | send the work package: item, dependencies, ratified decisions, search hits |
| `session/update` | transcript capture, redaction, presence |
| `session/request_permission` | answered by policy `[permissions]`: allow, deny, escalate to `wf inbox` |
| `terminal/*`, `fs/*` | run inside wf's environment; secret injection |
| `session/cancel` | `wf session stop`, lease lost, or item reassigned |

* Pin ACP v1 and keep it behind a `Harness` trait; a `raw-cli` fallback starts any agent with a prompt
  file and observes commits.
* ACP-level sandboxing only covers what the agent routes through the client; real isolation is OS-level
  (donor mode, *later*).
* `wf attach <session>` over an iroh ALPN `wf/acp/1` (M4).
* The CLI stays. MCP tools and CLI are thin wrappers over the same daemon API.

## 13. Secrets and transcripts

* Transcripts (the stored ACP update stream) stay **local and unpublished by default**.
  `publish = none | redacted | full` is a per-workforce policy (*later*).
* `wf` **brokers** secrets, it does not store them. Sources: `secretspec.toml`, 1Password `op://`
  references, the system keyring. Values are injected into the harness environment.
* wf keeps the injected values in memory only and uses them to scrub: exact match plus base64, URL and hex
  encodings, then pattern scanning (gitleaks-style) as a second layer.
* Progress events always pass redaction before signing.

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
1. The code contains the workforce id, the endpoint ids of a few members, and a random invite secret.
   Long codes, no rendezvous infrastructure.
2. **Invites are self-authorizing.** At invite time the principal (or an owner, for member invites) signs
   `InviteCreated{kind, role, hash(secret), expires, uses}`. The joiner writes its own `DeviceBound`
   (or `MemberJoined` with a new principal key) containing the secret's preimage. Any peer verifies this
   offline; no owner needs to be online and the new device never sees the principal key.
3. Concurrent redemptions of a single-use invite: earliest `(hlc, EventId)` wins; the loser stays unbound
   and is asked for a new code.
4. The joiner dials `wf/join/1` on any reachable member, syncs the log, then publishes its `DeviceProfile`.
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

[review]
approvals = 1
distinct_principal = false

[lanes.default]
claim = ["member"]
[lanes.meta]
claim = ["member"]
types = ["distill", "triage"]

[transcripts]
publish = "none"

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

**Not iroh-docs:** a namespace secret is a bearer credential (no per-author revocation), it is
last-writer-wins by wall clock, and it is pre-1.0. We write `wf/sync/1` ourselves (about 1-2k lines).
Keep the shape compatible with Willow ideas (author as subspace, `kind/id` as path) in case that matures.

Missing and built by us: authorization, authenticated gossip membership, ordering, store-and-forward
(the anchor), compaction (later), search, blob access control (later).

## 17. CLI (v1 surface)

```
wf init | invite device|member | join <code> | members | owner add|transfer
wf device list|edit                       # name, hardware, hosted agents, max_sessions
wf agent add|list|edit                    # profiles: harness, model, capabilities, lanes
wf session list|show|stop                 # live and recent sessions
wf daemon | sync | status | log | export
wf fork | fork accept | fork list
wf policy edit|diff|apply|export
wf item add|list|show|ready|edit|move|link|archive    # all types: task, question, decision, ...
wf comment <item> <text>
wf progress "<text>" | activity | inbox
wf review <item> --approve|--request-changes
wf search <query>
```
All commands support `--json`. Output is `{"schema": 1, "data": ...}`; the schema number is bumped on
incompatible changes and the shapes are snapshot-tested. Item ids print as the first 6 characters of their
base32 hash and any unique prefix of at least 4 characters is accepted, with or without `#`.
Without `--json`, lists (`item list|ready`, `agent list`, `log`, `mentions`) print as borderless tables
with bold headers. Every table is built by one function in `wf-cli`, so the look is defined in one place.
Styling is emitted only when stdout is a terminal; piped output is plain.

`wf item edit <item>` without flags opens a markdown draft in `$VISUAL` or `$EDITOR`: TOML front matter
between `+++` fences (`title`, `priority`, `lane`, `labels`, `requires`), then the body as plain markdown,
taken verbatim. TOML has no null, so unset fields appear as commented-out hints. Unknown keys, an
invalid priority or label, and a missing or unclosed fence are rejected. Flags (`--title`, `--body`,
`--add-label`, ...) edit single fields instead. The item type and state are not editable here.

Keys are hex files with mode `0600` under `$XDG_DATA_HOME/workforce/keys/`; the repository holds only
`.workforce.toml` with the workforce id.

## 18. Milestones

| M | Scope |
|---|---|
| **M0** | Event log with `actor`, `owner`/`member`, principal and device keys, agent definitions, item model (types, categories, built-in types, relations, derived refs and mentions, archiving), in-memory fold, redb event store with snapshots. Single peer. |
| **M1** | iroh sync, self-authorizing invites and `wf join`, device profiles, anchor dispatch, lanes, progress from commits, overlap notifications, `wf fork`. **= UC1** |
| M2 | Sessions and worktrees, ACP harness runner, `wf` MCP server, session inboxes, secret brokering, redaction, `Merged` from origin |
| M3 | Decision ratification flow, distill items, hybrid search |
| M4 | Multiple principals, threads, scopes, `wf attach` |
| M5 | Git bundles over iroh-blobs, peer-to-peer repos, compaction |
| M6 | Donors: revocation, rate limits, sandbox, transcript publishing, governance |

## 19. Open questions
1. Should merges to the main branch require a human key once origin-side protection is not enough?
   (v1: origin decides.)
2. Fork following for non-owner forks: ask-first UX.
3. Prior art to read: **Radicle**, **p2panda**, Keyhive, Matrix state resolution (before M6).
