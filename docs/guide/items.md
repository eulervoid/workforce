# Items

An item is a piece of work, or a decision, written so that an agent can take it from start to
submission in one session. Everything about it lives on the item: its description and steps, the
comments and questions, the sessions that worked on it, the review, the merge.

## Adding items

```sh
wf item add                      # write it in $VISUAL or $EDITOR
wf item add --title "Parse durations in policy" --priority P1 --body "$(cat <<'EOF'
Accept `2h`, `30m` and `1h30m` wherever the policy takes a duration.

- [ ] Parse durations with humantime
- [ ] Error messages name the field
- [ ] Tests for both
EOF
)"
```

```
created #k3f7ab
 ITEM (TYPE:STATE)      PRIORITY  TITLE                      STEPS  BLOCKING
 ○ #k3f7ab (task:open)  P1        Parse durations in policy  0/3
```

The body is markdown. Every `- [ ]` line is a **step**: the agent ticks them off as it goes, and
reports on each. Size an item for one session; split work into several items only where the parts
can run in parallel, need a different review level or different capabilities.

| Flag | Says |
|---|---|
| `--title`, `--body` | what the item is about; the body in markdown |
| `--type` | `task` (the default), `decision`, `distill`, or a type of your policy |
| `--priority` | `P0` (most urgent) to `P3` |
| `--label` | a label; repeat for several |
| `--lane` | the lane whose agents take it, instead of its type's |
| `--requires` | what an agent or machine needs to take it, such as `rust` or `gpu`; repeat for several |
| `--review` | its review level, such as `light` or `critical` |
| `--recovery` | what happens when a session ends without finishing, such as `diagnose` |
| `--draft` | hold it back: nothing starts it until `wf item open` |
| `--edit` | open the editor, filled in with the flags given |

`wf item edit <item>` changes an item, in the editor or with the same flags (`--add-label`,
`--remove-requires`, `--review default` and so on).

Items are named by the start of their id, as `#k3f7ab` (`k3f7` is enough while it is unique). Every
command that takes an item offers a picker at a terminal when you leave it out.

## States

Every item is in one of six states:

| State | Meaning |
|---|---|
| `draft` ◌ | held back while it is thought through; nothing starts it |
| `open` ○ | ready to be worked on, once its dependencies are done |
| `active` ◐ | a session works on it |
| `review` ◎ | the work is submitted and waits for its review |
| `done` ✓ | finished; what depends on it can start |
| `cancelled` ✗ | finished without success |

Agents and the daemon move items into `active` and `review` and, after the merge, to `done`. You move
them with verbs:

```sh
wf item open k3f7 a8c2           # release drafts, or reopen items
wf item draft k3f7               # hold back as a draft
wf item done k3f7                # finish without a session; for a decision: accept it
wf item cancel k3f7              # cancel; for a decision: reject it. A session on it stops after its turn
```

Each takes `--note` to say why. A type lists the states it uses: a `decision` starts in `review`, as
a proposal, and is accepted with `wf item done` or rejected with `wf item cancel`.

## Dependencies

```sh
wf item link k3f7 --depends-on a8c2         # k3f7 cannot start before a8c2 is done
wf item link k3f7 --remove-depends-on a8c2
wf item link d9e1 --supersedes d2f4         # for decisions: d9e1 replaces d2f4
```

An open item is **ready** when all its dependencies are done and no blocker is open. It is
**blocked** while some are not, and **stranded** when a dependency was cancelled: then someone has
to remove the dependency or cancel the item.

## Finding items

```sh
wf item list                     # the live items
wf item list --ready             # what can be worked on now
wf item list --ready --for ada   # what agent ada would be given on this machine
wf item list --blocked           # waiting on dependencies; --stranded: a dependency was cancelled
wf item list --state draft,open --label parser --group-by priority
wf item show k3f7                # the item, its comments, links, review and history
wf mentions @ada/grace           # where someone is mentioned
```

`--group-by` takes `state`, `readiness`, `label`, `type`, `lane` or `priority`.

## Comments

All text about an item is a comment: discussion, questions, reports, the reasons for a verdict.

```sh
wf comment k3f7 "Keep the old syntax working too."
wf comment k3f7                                   # write it in the editor
wf comment k3f7 "Done with the parser." --step 1 --done   # about step 1, and tick it
```

`#a8c2` in a comment refers to an item, `@ada/grace` mentions someone. A comment id, such as `q4tz`,
is shown next to each comment.

`wf item check k3f7 1 3` ticks steps 1 and 3 without a comment; `--remove` unticks them.

## Blockers and change requests

Two kinds of comments stay **open** until a later comment resolves them:

* A **blocker** says the work cannot start or go on until someone answers or acts. The item is not
  ready while it is open, and a session working on it **parks**: it waits, and continues once the
  blocker is resolved. An agent asks its questions this way. A blocker that mentions someone goes to
  them; otherwise to a supervisor agent (one in the `supervision` lane), and without one, to you.
* A **change request** says what the work has to do before it is submitted. The agent cannot submit,
  and the work cannot be approved, while one is open. Reviewers write them; you can too.

```sh
wf comment k3f7 "Which error type should this return?" --block
wf comment k3f7 "Name the field in the error." --request-change
wf comment k3f7 "Use PolicyError." --resolve q4tz          # answer and resolve
wf comment k3f7 --resolve-blockers                          # resolve every open blocker
wf comment k3f7 "Done in 3e8a1c." --resolve-change-requests
```

Blockers that wait on you are in `wf inbox` and the dashboard, where `r` answers them.

## Archiving

```sh
wf item archive k3f7             # hide a done or cancelled item from lists; its history stays
wf item archive --done           # archive every done item; --cancelled for the cancelled ones
wf item list --archived          # the live items, then the archived
wf item unarchive k3f7
```
