# Proposal: checks before merging

Status: proposed; reviewed once (findings folded in below); not built.

## Why
On 2026-10-14 an approved branch merged into `main` without a text conflict and did not compile:
`main` had moved since the review, and another item had removed a variable the branch still used.
Only a reviewer who built the merge result by hand caught it. The daemon merges with
`git merge-tree --write-tree` and `git commit-tree` (crates/wf-vcs/src/git.rs `merge`) and advances
the target; nothing builds or tests what lands.

Review steps judge the submitted commit, and the target keeps moving after the review, so no review
step can close this gap. What has to be tested is the merge result, right before the target moves.

## Proposal
**Named checks, run on the merge result before the target moves. The policy names them; each
machine's config says what a name runs.**

```toml
# policy (signed, synced): what must pass before work lands
[merge]
mode = "local"
target = "main"
checks = ["fmt", "clippy", "test"]   # optional; without it, merging works as now
```
```toml
# machine config: what each name runs on this machine
[commands.fmt]
run = ["cargo", "fmt", "--check"]    # argv, no shell

[commands.clippy]
run = ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]

[commands.test]
run = ["cargo", "test", "--workspace"]
timeout = "20m"
```

* **Why names in the policy and commands in the machine config.** A signed, synced policy must not
  make other machines run programs (SPEC §12, HOOKS.md): a member editing the policy on their device
  must not be able to run anything on mine. The policy says what must pass; the machine that merges
  decides how it checks that, so machines with different toolchains can still share a policy. A
  machine that lacks a command for one of the checks cannot merge, and `wf status` says so, as it
  does for review steps no agent can take.
* **Why separate checks, not one script.** Each check is listed where the principal sees it, its
  result is shown by name ("`clippy` failed"), and its result is kept on its own. And the work being
  checked cannot weaken the checks: a script in the repository could be changed by the very branch
  it checks (to `exit 0`), which work without a review level would merge unseen. The branch can still
  change the tests themselves; that is unavoidable, and it shows in the diff.
* **What is checked.** The tree that `merge-tree` makes from the target and the reviewed commit,
  checked out in the workforce's one **check worktree** (`<data>/<workforce>/check`), which is reset
  to each tree and cleaned before the run. The worktree persists, so build caches in it stay warm;
  merges are one at a time (below), so nothing else uses it meanwhile.
* **When.** Only at merge, in `local` mode, for work with commits: approved work, and work without a
  review level as its session ends. In `origin` mode the origin's CI decides, as it decides merging.
* **How.** The checks run one after another, in the order the policy lists them, and **all of them
  run** even when one fails, so an agent learns every failure in one round instead of one per merge
  attempt. When the build itself is broken, several checks fail for the same cause; that is noise,
  not harm. They do not run in parallel: they share the worktree, and cargo locks its target
  directory anyway.
* **The result, by tree and command.** A check's result depends only on the tree and the command
  that ran, so it is kept by both in a small cache on the merging machine. A check that passed on a
  tree is not run again; changing one command runs only that check again. A restart of the daemon
  loses only checks still running, which run again.
* **All passed:** the merge commit is made from that tree and the target advanced as now. If the
  target moved meanwhile, the new tree is checked again. On one machine that is rare; a target that
  keeps moving can make an item wait, which is accepted for now and said in the SPEC.
* **One failed:** the item goes back to work, like a conflict. Each failed check becomes a **change
  request by `@daemon`**: the check's name, its command, the tree, the last lines of its output, and
  the path of its full log on the merging machine. Being change requests, they are listed under
  "resolve every one before you submit" in the next session's brief and block resubmission until
  resolved, each on its own.
* **Not the item's fault.** When a check fails, the same check runs on the target's own tree too
  (cached). A check that fails there as well, or that timed out, is not sent to the agent: the item
  stays in `review` and the inbox tells the principal (`main itself fails clippy`, `test timed out`).
  This is decided per check, so a branch that breaks `fmt` while `main` breaks `test` still goes back
  for `fmt`.
* **No endless loops.** A pure function in `wf-core::lifecycle` counts the failed check runs on an
  item since the principal last acted on it; after two, the next failure waits for the principal
  instead of reopening the item.

**`wf check [<name>...]`** runs the policy's checks, or the named ones, in the current directory and
prints ✓ or ✗ per name with the failures' output: for an agent before `wf work submit` (or
`wf check clippy` while fixing one failure), a reviewer, or the principal. In a session it asks the
daemon for the commands, since the sandbox keeps agents out of the machine config. The brief tells
agents to run it. The daemon's run before the merge is the one that counts.

**What the principal sees.** A failure is a change request per failed check on the item, and a
failure that is not the item's fault is an inbox entry. Nothing new is written to the log: a check's
result is about one machine's toolchain, and other devices could not reproduce it anyway. A
"checking" status with a result per name in the dashboard can come later.

## Commands
A **command** is a program the machine config names, for wf to run on code. Checks are its first
use; the same entry serves the command review steps of HOOKS.md and the after-merge hook below.

```toml
[commands.<name>]
run = ["..."]              # argv, no shell
timeout = "30m"            # the default
environment.set = { ... }  # on top of the machine's [environment]
sandbox = "agent"          # the default: the agents' sandbox
```

* **Runs on code in the agents' sandbox.** The daemon has never run branch code itself; a check
  does, so it gets the sandbox agents get, with the machine's `[environment]`.
* **Its build output is its own.** A shared cargo target directory mixed crates of different
  worktrees on 2026-10-14, and a false pass is the worst outcome a check can have. The check worktree
  keeps cargo's default `target/` inside it; a machine config that sets `CARGO_TARGET_DIR` for its
  agents must not let it reach the commands (`wf status` warns when it does).
* **One runner.** The daemon runs a command the same way for every use: check out a commit or tree
  in a worktree, run the named command with its environment, sandbox and timeout, keep its output in
  a log per name, return passed, failed or timed out with the output's tail. What a result means is
  up to the use.

### Three uses, one runner
| | When | On what | What the result means |
|---|---|---|---|
| **Command review step** (HOOKS.md) | after a submission | the submitted commit | a verdict |
| **Check** (this proposal) | before the target moves | the merge tree | whether the work may land |
| **After merge** (later) | after the target moved | the merge commit | nothing for the item; a failure goes to the principal |

### Later: after merge
Building binaries, uploading them or deploying is not a check: it acts on what landed instead of
deciding whether it lands. wf does not become a CI system for it (no jobs, artifacts, matrices or
caches); in `origin` mode the origin's CI does it. For `local` mode one hook will do:

```toml
[merge]
after = ["release"]        # names again; the machine config says what "release" runs
```

* Only the daemon that made the merge runs it, so it runs once however many devices there are.
* It gets the hook protocol's JSON on stdin (`{"hook": "merged", "version": 1, "item": ...,
  "commit": ..., "target": ...}`), so it can decide itself whether a merge is worth a release.
* Its failure goes to the principal's inbox and never reopens the item: the work is already in the
  target.
* It needs secrets and the network (an upload token), so its `[commands.release]` sets `sandbox` and
  the secrets wf brokers for it (SPEC §13). That is the main reason commands live in the machine
  config.

Nothing of it is built in this proposal; the runner is built so that it fits.

## Prerequisites
The review of this proposal found three things that are wrong today and that checks would make
worse. They come first, as their own items.

1. **The agent sandbox protects less than assumed.** Fence's `code` template, which
   crates/wf-daemon/src/sandbox.rs extends, allows writes to `~/.config/**` and `~/.local/share/**`,
   does not deny reading the wf data dir, and wf adds the whole `<repo>/.git`. So an agent can read
   `principal.key` and sign as the principal, rewrite the machine config (`[sandbox] kind = "off"`, or
   a command), or set `core.hooksPath` / `core.fsmonitor` so that git runs its code outside the
   sandbox the next time the daemon runs `git status` or `git worktree add`. "Agents cannot write
   policy" holds only for events written through a session. Fix: deny reading and writing the wf data
   dir's keys and `~/.config/workforce`, deny writing `.git/config`, `.git/hooks` and `~/.config/git`,
   and have wf-vcs run git with `-c core.hooksPath=/dev/null -c core.fsmonitor=false`. The same item
   fixes why `wf init` cannot run in the sandbox: the template denies writing `**/*.key`, which the
   test suites need in their temp directories.
2. **"The next session merges the target into the branch first" relies on a loophole.** The `code`
   template denies `git merge`; agents got through only because the deny did not catch a compound
   shell command. Fix: when a session starts on an item whose last merge failed, the daemon (through
   wf-vcs) merges the target into the item branch; a conflict is left in the worktree for the agent
   to resolve.
3. **Merges run inside the dispatch loop.** `dispatch_once` merges every pending item before it
   dispatches anything (crates/wf-daemon/src/runner/dispatch.rs), and a session end merges inline
   (end.rs). Checks taking minutes there would stall all dispatching, and the per-item lock does not
   keep two merges from using the check worktree at once. Fix: one merge task per workforce, fed by
   the dispatcher and session ends, merging one item at a time; while an item waits for it, the merge
   counts as transient and the item stays in `review`.

## Decided
* **Only at merge.** A check step in review levels would spare reviewers red work, but its result
  rarely carries over to the merge, since the target moves in between. `wf check` before submitting
  gets most of that value, and HOOKS.md's command review step covers the rest when it is built, on
  the same runner.
* **Several named checks, not one script,** so each is explicit in the policy, reported by name, and
  out of reach of the branch it checks. Not an API for a script to report steps: that is a protocol
  every script must follow, and it hides what runs again.
* **Names in the policy, commands in the machine config,** as for every program wf runs.
* **All checks run,** even after one failed. Dependencies between checks (`after = "build"`) only if
  the noise of a broken build hurts.
* **No `Checked` event.** A local cache by tree and command and a change request per failed check
  carry what is needed.
* **A fix after a failed check is a new submission,** so its review level starts again
  (crates/wf-core/src/review.rs counts only verdicts on the submitted commit). That is how review works
  for any new commit; the cost is a principal re-reviewing one-line fixes. Revisit if it hurts.
* **Not a CI system.** Work after the merge is one hook, not jobs.
* **Naming:** a *check* is one named thing that must pass before work lands (`[merge] checks`,
  `wf check`); a *command* is what a name runs on a machine (`[commands.<name>]`). Not "step" (review
  levels have steps), not "gate", not "CI", not "job".

## Open questions
1. **Merging without the checks.** When a check is flaky or the principal knows better, should there
   be `wf item merge --unchecked <item>`, or is editing the policy for a moment enough?
2. **How many failures before the principal decides:** two, as above, or part of the recovery level?
3. **Batching** several approved items into one checked merge (as bors or GitHub's merge queue do),
   if checks get slow. Not before it hurts.

## Steps
1. Sandbox: deny the wf data dir's keys and config and git's config and hooks to agents; git without
   hooks or fsmonitor in wf-vcs; `wf init` and the test suites able to run in the sandbox.
2. wf-vcs and the daemon merge the target into the item branch when a session starts on an item whose
   merge failed; a conflict is left for the agent.
3. One merge task per workforce, out of the dispatch loop.
4. Commands and the runner: `[commands.<name>]` in the machine config, running a named command on a
   tree in a worktree with its environment, sandbox and timeout, a log per name.
5. Checks: `[merge] checks` in the policy, the check worktree, the result cache by tree and command,
   a `@daemon` change request per failed check, the target's own tree when one fails, the timeout,
   the limit on failures in `wf-core::lifecycle`, and the warning for machines missing a command.
6. `wf check`, through the daemon in a session; the brief; SPEC §8 (merge), §15 (policy) and the
   machine config docs; the commands for this repository in its README.
