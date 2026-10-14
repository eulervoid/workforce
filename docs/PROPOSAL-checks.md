# Proposal: a check before merging

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
**One check command, run on the merge result before the target moves.**

```toml
[merge]
mode = "local"
target = "main"
check = ["just", "check"]     # optional; without it, merging works as now
```

* **What runs.** The policy names a single command. What it does lives in the repository (a
  `justfile` recipe, `scripts/check`, ...), so it evolves with the code: a new crate or test suite
  adds itself in the same branch, a new repository gets a check without a policy edit, and machines
  with different toolchains run the same recipe. Ordering and stopping at the first failure are the
  script's business, not wf's. A branch that changes the script shows that in the diff its reviewer
  approves.
* **What it checks.** The tree that `merge-tree` makes from the target and the reviewed commit,
  checked out in the workforce's one **check worktree** (`<data>/<workforce>/check`), which is reset
  to each tree and cleaned before the run. The worktree persists, so build caches in it stay warm;
  merges are one at a time (below), so nothing else uses it meanwhile.
* **When.** Only at merge, in `local` mode, for work with commits: approved work, and work without a
  review level as its session ends. In `origin` mode the origin's CI decides, as it decides merging.
* **The result, by tree.** A check's result depends only on the tree, so it is kept by tree id in a
  small cache on the merging machine. A tree that passed is not checked again; a restart of the daemon
  loses only checks still running, which run again.
* **Passed:** the merge commit is made from that tree and the target advanced as now. If the target
  moved meanwhile, the new tree is checked again. On one machine that is rare; a target that keeps
  moving can make an item wait, which is accepted for now and said in the SPEC.
* **Failed:** the item goes back to work, like a conflict, but the reason is written as a **change
  request by `@daemon`**: the command, the tree, and the last lines of its output, with the path of
  the full log on the merging machine. Being a change request, it is listed under "resolve every one
  before you submit" in the next session's brief, and it blocks resubmission until resolved.
* **Not the item's fault.** When the check fails, the target's own tree is checked too (cached). If
  the target fails as well, or the check timed out, the item stays in `review` and the inbox tells the
  principal (`main itself fails the check`, `the check timed out`): sending it back would only make an
  agent chase a problem it did not cause.
* **No endless loops.** A pure function in `wf-core::lifecycle` counts the `@daemon` check change
  requests on an item since the principal last acted on it; after two, the next failure waits for the
  principal instead of reopening the item.

**Running it.** The check runs with the machine's `[environment]`, in the same sandbox as agents
(the daemon now executes branch code, which it never did before), with a timeout:

```toml
# machine config
[check]
timeout = "30m"
environment.set = { CARGO_TARGET_DIR = "~/.cache/workforce/check-target" }   # on top of [environment]
```

Its build output must not be shared with the agents' worktrees: a shared cargo target dir mixed
crates of different worktrees on 2026-10-14, and a false pass is the worst outcome a check can have.

**`wf check`** runs the policy's command in the current directory, for an agent before
`wf work submit`, a reviewer, or the principal. The brief tells agents to run it. The daemon's run
before the merge is the one that counts.

**What the principal sees.** A failure is a change request on the item, and a failure that is not
the item's fault is an inbox entry. Nothing new is written to the log: a check result is about one
machine's toolchain, and other devices could not reproduce it anyway. A "checking" status in the
dashboard can come later.

## Prerequisites
The review of this proposal found three things that are wrong today and that a check would make
worse. They come first, as their own items.

1. **The agent sandbox protects less than assumed.** Fence's `code` template, which
   crates/wf-daemon/src/sandbox.rs extends, allows writes to `~/.config/**` and `~/.local/share/**`,
   does not deny reading the wf data dir, and wf adds the whole `<repo>/.git`. So an agent can read
   `principal.key` and sign as the principal, rewrite the machine config (`[sandbox] kind = "off"`), or
   set `core.hooksPath` / `core.fsmonitor` so that git runs its code outside the sandbox the next time
   the daemon runs `git status` or `git worktree add`. "Agents cannot write policy" holds only for
   events written through a session. Fix: deny reading and writing the wf data dir's keys and
   `~/.config/workforce`, deny writing `.git/config`, `.git/hooks` and `~/.config/git`, and have wf-vcs
   run git with `-c core.hooksPath=/dev/null -c core.fsmonitor=false`. The same item fixes why
   `wf init` cannot run in the sandbox: the template denies writing `**/*.key`, which the test suites
   need in their temp directories.
2. **"The next session merges the target into the branch first" relies on a loophole.** The `code`
   template denies `git merge`; agents got through only because the deny did not catch a compound
   shell command. Fix: when a session starts on an item whose last merge failed, the daemon (through
   wf-vcs) merges the target into the item branch; a conflict is left in the worktree for the agent
   to resolve.
3. **Merges run inside the dispatch loop.** `dispatch_once` merges every pending item before it
   dispatches anything (crates/wf-daemon/src/runner/dispatch.rs), and a session end merges inline
   (end.rs). A check taking minutes there would stall all dispatching, and the per-item lock does not
   keep two merges from using the check worktree at once. Fix: one merge task per workforce, fed by
   the dispatcher and session ends, merging one item at a time; while an item waits for it, the merge
   counts as transient and the item stays in `review`.

## Decided
* **Only at merge.** A check step in review levels (`steps = ["check", "principal"]`) would spare
  reviewers red work, but it needs a new step kind, verdicts by the daemon and dispatch changes, and
  its result rarely carries over to the merge, since the target moves in between. `wf check` before
  submitting gets most of that value. Add the step if reviewers keep seeing red work.
* **One command, not a list.** The repository's script sequences the steps.
* **No `Checked` event.** A local cache by tree id and a change request on failure carry what is
  needed.
* **A fix after a failed check is a new submission,** so its review level starts again
  (crates/wf-core/src/review.rs counts only verdicts on the submitted commit). That is how review works
  for any new commit; the cost is a principal re-reviewing one-line fixes. Revisit if it hurts.
* **Naming:** the concept is a *check* everywhere: `[merge] check` in the policy, `[check]` in the
  machine config, `wf check`. Not "gate", not "CI".

## Open questions
1. **Merging without the check.** When the check is flaky or the principal knows better, should there
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
4. The check: `[merge] check` in the policy, `[check]` in the machine config, the check worktree, the
   result cache by tree, failure as a `@daemon` change request, the target's own tree when it fails,
   the timeout, and the limit on failures in `wf-core::lifecycle`.
5. `wf check`, the brief, SPEC §8 (merge), §15 (policy) and the machine config docs, and a
   `justfile` with a `check` recipe for this repository.
