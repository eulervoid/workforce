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
                                            ├─ event store (per-author feeds, SQLite)
                                            ├─ projections (SQLite): items, claims, members, progress
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

## 5. Identity and authority

* **Principal key** (ed25519): a human. Rarely used, ideally held in 1Password. Signs owner-level events.
* **Device key** (ed25519): one per daemon/agent seat. Signs ordinary events. Carries a `Delegation` from
  its principal. The iroh `EndpointId` is the device key.
* **Workforce id**: BLAKE3 hash of the genesis event. Identifies, but is not a capability, so it may be
  committed to the repo.

```rust
struct Delegation {
    principal: PrincipalKey,
    device: DeviceKey,
    kind: DeviceKind,        // Human | Agent | Anchor
    labels: Vec<Label>,      // capabilities, e.g. gpu, os:linux
    expires: Option<Timestamp>,
}
```

### Roles (v1)
| Role | Can do |
|---|---|
| `owner` | everything: invites, policy, `ForkAnnounced`, naming other owners |
| `member` | items, claims, comments, progress, reviews, decisions |

Hard rule: `policy.*`, `invite.*` and `owner.*` events are valid only when signed by an owner
**principal key**. No device key, so no agent, can change them.

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
    seq: u64,                // per author, gapless, starting at 1
    prev: Option<EventId>,   // author's previous event
    deps: Vec<EventId>,      // heads seen when writing
    hlc: Hlc,                // tie-breaks only, never authority
    body: EventBody,
    sig: Signature,
}
// EventId = blake3(canonical encoding of everything incl. sig)
```

* **Encoding:** canonical postcard (deterministic, compact). Signature covers the encoding without `sig`.
* **Sync:** exchange version vectors `{author -> max_seq}`, stream missing ranges.
* **Equivocation:** two events with the same `(author, seq)` are proof of misbehavior. v1 records and
  surfaces it; it has no automatic effect.
* **Unknown event types** are stored, relayed, and ignored by the fold, so newer peers can add types.
* **Fold order:** the causal order from `deps`, ties broken by `(hlc, EventId)`.

### Event bodies (v1)
`Genesis`, `MemberAdded`, `DeviceBound`, `InviteCreated`, `PolicySet`, `ItemCreated`, `ItemFieldSet`,
`ItemTransition`, `Comment`, `Assigned`, `Claim`, `Heartbeat`, `Release`, `Progress`, `Review`,
`Merged`, `DecisionProposed`, `DecisionRatified`, `ForkAnnounced`.

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

Ported from `items`: configurable item types and states, `parent`, `depends_on`, cycle rejection,
readiness from dependencies only. Mutations are events:

* `ItemCreated` (short id from the event hash, so concurrent creates cannot collide)
* `ItemFieldSet{field, value}`: last writer wins per field, ordered by causal order, then `(hlc, EventId)`
* `ItemTransition{from, to}`, `Comment`

Comments, history and attribution are core here (the `items` v1 left them out).

### Default workflow
`backlog -> ready -> claimed -> in_progress -> in_review -> done`, plus `changes_requested`.
Transitions are configurable in policy. In v1 transitions are signals, not security gates.

### Routing work: three separate axes
| Axis | Question | Mechanism |
|---|---|---|
| **scope** | who can *see* it | replication filter (*later*, v1 has one scope) |
| **lane** | who may *claim* it | lane policy, matched by role and kind |
| **requires** | who is *able* to | item labels vs. device labels (`gpu`, `os:linux`) |

* Every item is in exactly one lane. The lane may derive from the item type (`distill`, `triage` go to `meta`).
* In v1 lanes are **routing, not security**: members are trusted.
* `wf item ready --for-me` and the dispatcher apply the same lane and requires filter.

### Assignment
`dispatch` is the default; `pull` is configurable per workforce.

1. An agent sends a claim request to the anchor.
2. The anchor picks one and signs `Assigned{item, device, lease}`.
3. If the anchor is unreachable, policy `fallback` decides: `optimistic` (unconfirmed `Claim`, conflicts
   resolved later by earliest causal claim, then lowest hlc, then lowest id) or `wait`.
4. Leases renew with `Heartbeat`, and expire back to `ready`.

## 9. VCS

* Git only. A `VcsAdapter` trait is a code boundary with one implementation. Events carry a `vcs` enum
  with a single variant, so jj can be added later without a format change.
* One worktree per item, branches named `wf/<item>/<device>`.
* **Phase 1:** a central origin (GitHub etc.). **The origin decides merging** (branch protection).
  `wf` observes it and emits `Merged{item, commit}` when a `wf/<item>/...` branch lands in the target.
  `Review` events are coordination signals and gate nothing.
* **Later:** git bundles over iroh-blobs for peer-to-peer repositories; `git-remote-iroh` as prior art.

## 10. Progress log

The "what is being changed right now" feed, tied to items. Small commits make small reports.

```rust
struct Progress {
    item: ItemId,
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
* `wf activity` is a live feed. When two in-progress items' `touches` overlap, both get an inbox entry.

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
* Decisions are append-only. Changes supersede, never edit.
* Only **ratified** decisions enter agent context. Unratified ones are searchable by humans.
* Search is local per peer, not synced: SQLite FTS5 for keywords, local vectors with a local embedding
  model, fused by reciprocal rank. `wf search "..." --kind decision,comment --since 30d`.

## 12. Harness integration via ACP

`wf agent run` is an ACP client (protocol v1) that drives the harness as a subprocess.

| ACP | Use |
|---|---|
| `session/new` + MCP servers | start in the item's worktree, attach the `wf` MCP server (`item_show`, `progress`, `inbox`, `search`, `decision_propose`) |
| `session/prompt` | send the work package: item, dependencies, ratified decisions, search hits |
| `session/update` | transcript capture, redaction, presence |
| `session/request_permission` | answered by policy `[permissions]`: allow, deny, escalate to `wf inbox` |
| `terminal/*`, `fs/*` | run inside wf's environment; secret injection |
| `session/cancel` | lease lost or item reassigned |

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

```
wf init --name soundings          # genesis; prints the workforce id
wf invite --expires 24h           # prints a ticket: id + inviter addresses + secret
wf join <ticket>                  # on another machine
wf device add                     # your own second device, principal-signed
wf repo set <origin>
wf daemon                         # launchd / systemd / NixOS module
```

1. `InviteCreated{hash(secret), expiry, uses}` is in the log, so any online owner-signed path can redeem it.
2. The joiner dials `wf/join/1` with the secret and its device key; a `MemberAdded` and `DeviceBound` follow.
3. Gossip topics derive from the workforce id; bootstrap peers are the known members' endpoint ids.
4. Non-member endpoints are rejected at the handshake.
5. State lives in `$XDG_DATA_HOME/workforce/<id>/`. The repo holds only `.workforce.toml` with the id.

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
wf init | invite | join | device add | members | owner add|transfer
wf daemon | sync | status | log | export
wf repo set
wf fork | fork accept | fork list
wf policy edit|diff|apply|export
wf item add|list|show|ready|update|edit|claim|release|close
wf comment <item> <text>
wf progress "<text>" | activity | inbox
wf review <item> --approve|--request-changes
wf decision list|show|propose|ratify|supersede
wf search <query>
wf agent run [--harness ..] [--labels ..] [--max-parallel ..]
```
All commands support `--json` with a versioned schema.

## 18. Milestones

| M | Scope |
|---|---|
| **M0** | Event log, `owner`/`member`, principal and device keys, items domain, fold to SQLite. Single peer. |
| **M1** | iroh sync, invites, device add, anchor dispatch, lanes, progress from commits, overlap notifications, `wf fork`. **= UC1** |
| M2 | ACP harness, `wf` MCP server, secret brokering, redaction, worktrees, `Merged` from origin |
| M3 | Project decisions, distill items, hybrid search |
| M4 | Multiple principals, threads, scopes, `wf attach` |
| M5 | Git bundles over iroh-blobs, peer-to-peer repos, compaction |
| M6 | Donors: revocation, rate limits, sandbox, transcript publishing, governance |

## 19. Open questions
1. Should merges to the main branch require a human key once origin-side protection is not enough?
   (v1: origin decides.)
2. Fork following for non-owner forks: ask-first UX.
3. Prior art to read: **Radicle**, **p2panda**, Keyhive, Matrix state resolution (before M6).
