# Permission requests

A harness asks before it runs a tool: a shell command, a file edit, a fetch. wf answers those
requests for its agents. Each agent says how, with `--permissions` on `wf agent add` or
`wf agent edit`:

| Mode | What happens to a request |
|---|---|
| `allow` (the default) | allowed. On a machine without a sandbox it is asked instead |
| `ask` | goes through the deciders of the machine config, then to you |
| `deny` | refused |

`allow` is safe because agents run in a sandbox (see [machine-config.md](machine-config.md#the-sandbox)).
With `[sandbox] kind = "off"`, an agent runs with your privileges, so wf treats `allow` as `ask`.

## Answering a request

A request that no decider answers waits for you. It shows in the dashboard's first tab, in
`wf inbox`, and as a notification when one is configured. Meanwhile the session waits.

```sh
wf inbox                         # coder asks to cargo test; `wf session allow` or `deny s4k2`
wf session allow <session>       # let it run
wf session deny <session>        # refuse it
```

Without a session id, both commands offer the pending requests to pick from at a terminal. In the
dashboard, `a` allows and `d` denies the selected request.

## The deciders

The deciders are listed in the machine config (`~/.config/workforce/config.toml`) and asked in
order. Each one allows, denies, or passes the request on to the next. What all of them pass on goes
to you. A decider that fails, times out or answers something that does not parse passes the request
on; it never decides by accident.

Without a `[permissions]` section, only the built-in rules decide, and then you.

```toml
[permissions]
guidance = "Allow reading anything and writing in the worktree. Ask about the network."

[[permissions.decider]]
kind = "rules"                   # built in: a session's own `wf` commands

[[permissions.decider]]
kind = "model"                   # a decision model over the System One API, such as Jev
url = "https://openrouter.ai/api/v1/systemone"
model = "typesafe/jev-1.13"
api_key_env = "OPENROUTER_API_KEY"
question = { noul = { allow_above = 0.95, deny_below = 0.05 } }   # or { choice = { min_confidence = 0.8 } }

[[permissions.decider]]
kind = "hook"                    # any program: the request as JSON on stdin,
command = ["my-judge"]           # `{"permission": "allow" | "deny" | "ask", "reason": ...}` on stdout

[[permissions.decider]]
kind = "agent"                   # an agent of the workforce, in a harness session the daemon keeps
agent = "judge"
```

`guidance` says in words what may and may not be done. Deciders that read, models and agents, get
it with every request.

### `rules`

The built-in rules allow what a session needs to report back: a command that only runs `wf` with
one of `item`, `comment`, `work`, `review`, `inbox`, `status`, `log`, `search` and `mentions`, or
asks a `wf` command for `--help`, optionally piped into `head`, `tail`, `grep`, `rg`, `wc`, `sort`,
`uniq`, `cut` or `jq`. Commands that change devices, agents, the policy or permissions are not
among them, so an agent cannot widen what it may do. Anything the rules do not fully understand is
passed on.

### `model`

A decision model behind the System One API. `api_key_env` names the environment variable of the
daemon that holds the key; it is not passed on to agents. `question` says what the model is asked:

* `noul`: how likely the call should be allowed. Allowed above `allow_above`, denied below
  `deny_below`, passed on in between.
* `choice`: which of allow, deny and ask fits. Taken when the model's confidence is at least
  `min_confidence`.

`timeout` (default `"20s"`) bounds each call.

### `hook`

Any program, run without a shell in the session's directory. It gets the request as one JSON object
on stdin:

```json
{
  "hook": "permission",
  "version": 1,
  "workforce": "kjfllqz46iux...",
  "item": { "id": "k3f7ab...", "title": "Parse durations in policy", "type": "task", "state": "active" },
  "session": "s4k2...",
  "agent": { "name": "coder", "capabilities": ["rust"] },
  "directory": "/Users/ada/.local/share/workforce/.../worktrees/k3f7ab...",
  "tool": { "kind": "execute", "title": "cargo test", "input": { "command": "cargo test" } },
  "guidance": "Allow reading anything and writing in the worktree. Ask about the network."
}
```

and answers with one JSON object on stdout:

```json
{"permission": "allow", "reason": "runs the tests"}
```

`permission` is `allow`, `deny` or `ask` (pass it on); `reason` is optional. Fields either side does
not know are ignored, so a hook keeps working as wf adds to the request. `hook` is always
`permission` for now; a program that will serve several kinds of hooks can switch on it. `timeout`
(default `"20s"`) bounds the run; a non-zero exit passes the request on.

A hook that allows `cargo` commands and asks about the rest:

```sh
#!/bin/sh
if jq -e '.tool.input.command // "" | startswith("cargo ")' >/dev/null; then
  echo '{"permission": "allow", "reason": "cargo"}'
else
  echo '{"permission": "ask"}'
fi
```

### `agent`

One of your agents decides, in a harness session the daemon keeps for that purpose. After `turns`
requests (default 50) the session is replaced, so its context stays small. Define the agent as any
other, with `wf agent add`; give it `--instructions` on how strict to be. An agent decider only runs
in the daemon, so `wf permissions test` passes over it.

## Trying it out

`wf permissions test` runs the deciders of this machine's config on a request, without a session:

```sh
$ wf permissions test --command "cargo test"
passed by rules: no rule covers it
the principal would be asked

$ wf permissions test --command "wf item show k3f7 | head"
allowed by rules: `wf item` reports to wf
```

Or give it a whole request on stdin, in the form a hook gets: `wf permissions test < request.json`.

The transcript of a session (`wf session watch`, or the dashboard) says for each request who decided
it and why.
