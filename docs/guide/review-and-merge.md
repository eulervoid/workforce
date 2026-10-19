# Review and merge

When an agent submits, its commits go through the item's **review level**. Once every step approves,
the daemon merges the work into the target branch and the item is done.

## Review levels

The default policy has five levels, from least to most strict:

| Level | Who reviews |
|---|---|
| `none` | nobody: the work is merged as it is submitted |
| `light` | an agent with the capability `review` |
| `standard` | an agent with `review` and `strong-model` |
| `critical` | an agent with `review` and `strong-model`, then you |
| `manual` | you alone; the default for tasks |

An item gets its type's level unless it names one: `wf item add --review light`, or later
`wf item edit k3f7 --review critical` (`--review default` goes back to the type's). An agent may raise
the level of the item it works on, never lower it. You can change the levels, or add your own, in the
[policy](policy.md#review-levels).

A review step for an agent is taken by one of your agents whose capabilities meet it, with `--start
auto` and a free slot. It never reviews an item it worked on. If no agent can take a step, `wf status`
says so.

## Your review

A step of yours waits in `wf inbox` and on the dashboard's first tab:

```
 WAITING           ITEM     TITLE                      WHAT
 principal_review  #k3f7ab  Parse durations in policy  review of 3e8a1c4f: `wf item diff`, then `wf review`
```

Look at the work:

```sh
wf item show k3f7                # what the agent reported, step by step
wf item diff k3f7                # the diff, through git: your pager, colors and diff config apply
wf item diff k3f7 --stat         # anything after the item goes to `git diff`, such as `-- src/`
wf item checkout k3f7            # put your main checkout on the work, detached, to build and try it
wf item checkout -               # and back to where it was
cd $(wf item path k3f7)          # or go to the item's worktree
```

`wf item diff` shows what the work changes since it left the target. Once the item is merged, it shows
what the merge brought into the target, however far the target moved since, and `wf item checkout`
still checks out the work itself. While the branch has no commits ahead of the target, it says the
agent has not made any commits; with `--json`, `committed` is then `null`, where commits that change nothing
give an empty one.

While an agent works on the item on this machine, the diff has two parts, never one: the committed
part above, and under it, headed `uncommitted, against <head>`, what the worktree has changed since
the branch head, new files included. Only commits carry over to a later session and are what review
and merge see, and a comment on lines is anchored to a commit, so the uncommitted part is for
looking at only. A file changed in both shows in both. With `--json`, `uncommitted` holds the second
part, and is absent, not empty, when no worktree was read, as when no session is running. Git's own
options, such as `--stat`, apply to both parts.

When the agent's session runs on another of your machines, `wf item diff` asks that machine through
the daemon and prints what it has committed, saying which device it came from; `--json` has the
device in `device`. Git's own options and pager do not apply to it. A machine that cannot be reached is
said as such, not as a missing branch; the diff of an item with no running session is read here.

Then give a verdict:

```sh
wf review k3f7 --approve
```
```
approved #k3f7ab
```

To send it back, write what has to change as change requests first, one comment each, then the
verdict:

```sh
wf comment k3f7 "Name the field in the error message." --request-change
wf comment k3f7 "Test the empty string." --request-change
wf review k3f7 --request-changes --note "Close; two things."
```
```
requested changes on #k3f7ab
```

The item goes back to open, and the next session continues the same branch with your change requests
in its brief. It resolves each one before it can submit again. In the dashboard, `v` then `a` approves,
and `v` then `r` writes the change requests in your editor, one per line, and sends the work back.

A reviewer agent can also `--escalate`: then a step of yours is added after its own.

## The merge

Once the last step approves, the daemon merges the reviewed commit into the target branch of the
policy, `main` by default. It builds the merge commit without touching any checkout,
`Merge #k3f7ab: Parse durations in policy`, and moves the branch. If the target is checked out, in
your main checkout for example, that checkout is fast-forwarded so your files follow. Then it removes
the item's worktree and its `wf/<item>` branch, and the item is done; what depended on it can start.

Work on an item whose level has no steps is merged as soon as the session submits.

Three things can hold a merge: the checks, below, and these two:

* **Your checkout has changes.** If the target branch is checked out with uncommitted changes, the
  daemon does not touch it. It comments once, `Waiting to merge: main is checked out in ~/src/app with
  changes to src/lib.rs. Commit or stash them.`, and `wf inbox` lists a `merge` entry. Commit or stash,
  and the daemon merges within half a minute.
* **A conflict.** The daemon comments `Merging into main conflicts in src/parse.rs. The next session
  merges main into the branch first.`, the item goes back to open, and the dashboard warns of it
  (see [Warnings](dashboard.md#warnings)). The next session resolves the
  conflict and submits again, for a new review.

What was reviewed is what gets merged: a new submission starts the review over.

## Checks

With checks in the policy (see [policy.md](policy.md#checks)), the daemon runs them on the merge of
the work into the target twice: when the agent submits, before anyone reviews, and before it moves
the target. They run one after another, all of them even when one fails, in a worktree of their own.
A check that passed on a tree in a sandbox is not run on it again in that sandbox.

**At submission** the checks run in the agents' sandbox, `[sandbox]` of the machine config, even when
you let the checks of the merge run without one: the code is not reviewed yet. `wf work submit`
answers at once that they run, and the agent ends its turn; a whole test suite takes longer than an
agent's harness lets a command run. The session waits until they are done.

* **They fail:** the daemon prompts the same session with each failed check's name, command, the end
  of its output and where its whole log is, and writes one change request with the same. The agent
  fixes them, resolves the change request and submits again. Nobody reviews work that fails its
  checks.
* **They pass:** the session ends and the item goes to review.

**At the merge** they run again as the machine config says, which may be without a sandbox (see
[machine-config.md](machine-config.md#checks)); this is the only place where a check can run outside
it. Only a check that ran in another sandbox, or on another merge result because the target moved,
runs again.

* **All pass:** the work is merged.
* **The work broke one:** the daemon writes a change request that the check fails on the merge into
  `main`, and the item goes back to open. The next session has to resolve it before it submits again;
  the new submission is checked and reviewed again.
* **It is not the work's fault:** a check that fails on the target itself too, or runs out of time,
  is a blocker for you, at submission as well as at the merge, and the item waits in review. Fix the
  target, or the timeout, and resolve the blocker; the merge is checked again.
* **It keeps failing:** after two failed runs, at submission or merge, since you last acted on the
  item, the next failure is a blocker for you instead of going back to the agent.

`wf check` runs the policy's checks on a checkout as it is, uncommitted changes included, and
`wf check clippy` runs one. It prints `✓` or `✗` per check, with the end of the output of each that
failed and where its log is, and fails when one does. Agents are told not to run the whole suite
while they work, since submitting runs it; `wf check` is for when it is worth it. Not to be mixed up
with `wf item check`, which ticks steps.

The merge commit is signed as your git config says, or as the machine config chooses; see
[machine-config.md](machine-config.md#commits-and-signatures).

## When a session does not finish

A session that releases its item, runs out of nudges or time, or fails, did not finish. The item's
**recovery level** says what happens next, one step per unsuccessful attempt:

| Level | After each unsuccessful attempt |
|---|---|
| `none` | the item waits for you |
| `standard` | try again, try again, then it waits for you; the default for tasks |
| `diagnose` | a reviewer agent says what went wrong, then one more try, then it waits for you |

Set it with `wf item add --recovery diagnose` or `wf item edit --recovery`. An item that waits for you
shows in `wf inbox` as `recovery`, with how the last session ended. Comment with what to do
differently: whatever you do to the item starts its recovery level over, so the daemon tries again,
and the next brief carries your comment. Or start a session yourself with `wf session start <item>`,
or cancel the item.
