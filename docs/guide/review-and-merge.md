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

With checks in the policy (see [policy.md](policy.md#checks)), the daemon runs them on the merge
result before it moves the target: one after another, all of them even when one fails, in a worktree
of their own and in the sandbox the machine config gives checks. A check that passed on a tree is
not run on it again.

* **All pass:** the work is merged. If the target moved meanwhile, the new merge result is checked.
* **The work broke one:** for each such check the daemon writes a change request that the check
  fails on the merge into `main`, with the command, the end of its output and where its whole log is,
  and the item goes back to open. The next session has to resolve each before it submits again; the
  new submission is reviewed again.
* **It is not the work's fault:** a check that fails on the target itself too, or runs out of time,
  is a blocker for you, and the item stays in review. Fix the target, or the timeout, and resolve the
  blocker; the merge is checked again.
* **It keeps failing:** after two failed runs since you last acted on the item, the next failure is a
  blocker for you instead of going back to the agent.

`wf check` runs the policy's checks on a checkout as it is, uncommitted changes included, and
`wf check clippy` runs one. It prints `✓` or `✗` per check, with the end of the output of each that
failed and where its log is, and fails when one does. Agents are told to run it before they submit;
the daemon's run before the merge is the one that counts. Not to be mixed up with `wf item check`,
which ticks steps.

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
