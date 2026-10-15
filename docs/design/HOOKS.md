# Hooks: commands as steps of reviews and recovery

Status: plan for later, not scheduled. Builds on review levels (SPEC §9), recovery levels (planned)
and the permission hook (M2.1).

Today a review step is an agent or the principal, and the permission chain can ask a program. This plan
lets a **command** take a review step or a recovery step too. wf runs the command, writes everything
it needs to decide as JSON on its stdin, and reads one JSON object back. Custom behaviour can then be
scripted in any language, without writing Rust or changing wf: a review gate that runs the test suite
and a linter, a recovery step that reads the failed session's last words and decides between another
try and the principal.

```toml
# policy: the steps name a command by name
[[review]]
name = "gated"
steps = [{ command = "ci-gate" }, { agent = { requires = ["review"] } }]

[[recovery]]
name = "triaged"
steps = [{ command = "triage" }, "retry", "principal"]
```
```toml
# machine config: what the name runs on this machine
[commands.ci-gate]
run = ["./scripts/ci-gate.ts"]      # argv, no shell
timeout = "10m"

[commands.triage]
run = ["wf-triage", "--strict"]
timeout = "30s"
```

## Decisions
* **Steps name a command; the machine config says what it runs.** A signed, synced policy must not
  make other machines run programs (SPEC §12). The policy only says `{ command = "ci-gate" }`; a
  machine whose config defines `ci-gate` can take the step, like an agent with the capabilities of
  a step. A step that no machine can take is flagged in `wf status`, as unstaffed review steps are
  today.
* **One hook protocol for everything wf asks a program.** The permission hook already speaks it:
  one JSON object on stdin naming the hook and its version, one object on stdout, and both sides
  ignore fields they do not know. Review and recovery commands are two more hook kinds:
  `{"hook": "review", "version": 1, ...}` and `{"hook": "recovery", "version": 1, ...}`. A program
  that serves several hooks switches on `hook`.
* **A command decides; wf acts.** The command returns a decision and text. wf validates it and writes
  the events, attributed to the daemon with the command's name, exactly as for an agent step. A
  command never needs a key, a session or the socket, so it cannot do more than its step allows. A
  command may still read the repository and run `wf` read-only commands if it wants more context.
* **Failure passes on, it never decides.** A crash, a timeout or output that does not parse is not a
  verdict. For a review step the item waits in `wf inbox` with the cause; for a recovery step the
  next step of the level runs. The daemon logs the cause and comments once on the item.
* **The schema is published by wf, not written twice.** `wf hook schema [<hook>]` prints JSON
  Schema for each request and response, generated from the Rust types (`schemars`). The TypeScript
  library and any other binding are generated from it, so they cannot drift from the binary.

## Requests and responses (version 1, sketch)
Every request carries `hook`, `version`, `workforce`, and the item: id, title, type, state, body,
labels, review level, open blockers and change requests, and its comments in order.

**Review.** Runs in a read-only checkout of the submitted commit, like a review session.
```jsonc
// in
{ "hook": "review", "version": 1, "item": { ... }, "step": 1,
  "submission": { "commit": "6f2e3bce...", "target": "main", "base": "...",
                  "files": [{ "path": "src/parse.rs", "added": 40, "removed": 3 }] },
  "directory": "/.../worktrees/k3f7a2...-6f2e3bce",
  "earlier_steps": [{ "step": 0, "by": "ci-gate", "verdict": "approve" }] }
// out
{ "verdict": "approve" | "request_changes" | "escalate",
  "note": "Tests pass; clippy clean.",
  "change_requests": ["parser.rs: the error path drops the span"] }   // required for request_changes
```

**Recovery.** Runs in an empty directory; it decides from the history.
```jsonc
// in
{ "hook": "recovery", "version": 1, "item": { ... },
  "ended": { "session": "pp74s3", "agent": "coder", "reason": "released",
             "last_words": "I cannot reach the registry ...", "commits": 2 },
  "attempts": [{ "session": "...", "agent": "coder", "reason": "timed_out" }] }
// out
{ "next": "retry" | "principal" | "cancel",
  "note": "Network was blocked; added registry.npmjs.org to the sandbox. Retrying.",
  "retry_with": { "agent": "coder" } }        // optional: who takes the next attempt
```
The `note` becomes a comment on the item before the next attempt, so the next work brief carries
it, the same as the analysis of a recovery agent step.

## Work items
1. **Hook runner shared by all hooks.** Generalise `decide::hook` (argv, stdin, stdout, timeout,
   passing on failure) into one runner used by permission, review and recovery hooks.
2. **`[commands]` in the machine config**, and `{ command = "<name>" }` as a step in review and
   recovery levels. The command name is a validated name type like `AgentName`.
3. **Review command steps**, in the dispatcher next to agent review steps: same checkout, same
   verdict rules (change requests needed to request changes, `escalate` inserts a principal step).
4. **Recovery command steps.**
5. **`wf hook schema`** and **`wf hook test <command> < request.json`**, which runs a configured
   command on a request without a session, as `wf permissions test` does. `wf hook request <item>
   --hook review` prints the request wf would send now, to write tests against.
6. **TypeScript library** (`@workforce/hooks`, later others): types generated from the schema, and a
   helper that reads the request, switches on `hook` and writes the answer:
   ```ts
   import { hooks } from "@workforce/hooks";

   hooks({
     review: async ({ submission, directory }) => {
       const tests = await $`npm test`.cwd(directory).nothrow();
       return tests.exitCode === 0
         ? { verdict: "approve", note: "npm test passes" }
         : { verdict: "request_changes", change_requests: [tests.stderr.slice(-2000)] };
     },
     recovery: async ({ ended, attempts }) =>
       attempts.length < 3 ? { next: "retry" } : { next: "principal", note: ended.last_words },
   });
   ```

## Open questions
* **Sandbox.** A review command typically runs the code under review (tests, builds), which the agent
  wrote. Run commands in Fence with the review checkout writable, like agents? Probably yes, with an
  opt-out per command.
* **Long-running commands.** CI-like gates take minutes. The step holds the item meanwhile, like a
  review session; is a timeout enough, or do commands need progress output for the dashboard and `wf log --follow`?
* **Scheduler hook.** SPEC §12 mentions a scheduler hook for dispatch decisions. It should use this
  same protocol; whether it belongs in this plan or its own is open.
* **Distribution of commands across machines.** A policy step that only one machine can take ties
  reviews to that machine. Fine on one machine; for stage 2 with several machines, the anchor might be the natural
  host.
