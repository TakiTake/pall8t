# pall8t sandbox — a herdr plugin

Puts the sandbox controls where the panes already are: check what a pane's
sandbox is, open a second shell inside it, rebuild its image, stop it —
from herdr, without a second terminal.

```sh
herdr plugin link contrib/herdr-plugin      # from a pall8t checkout
herdr plugin list                           # confirm it linked
```

Bind the entrypoints to keys or invoke them from herdr's plugin UI; see
[herdr's plugin docs](https://herdr.dev/docs/plugins/).

| entrypoint | what it does |
|---|---|
| `status` (popup) | the pane's container, image, project, pall8t version, herdr pane and sandbox mode, plus a count of other running sandboxes |
| `shell` (overlay) | `pall8t exec` into the pane's container — a second shell beside the agent, which keeps running |
| `rebuild` / `rebuild-no-cache` (popup) | `pall8t build` in that sandbox's project directory |
| `stop` (action) | `pall8t stop` for the pane's container — the agent's session ends |

## How it finds the pane's sandbox

pall8t labels each container it starts with the herdr pane it was launched
from (`pall8t.herdr.pane`), so the plugin looks it up in `pall8t ls --json`
rather than guessing from the container's name. Without a pane in the
environment it falls back to a single running sandbox, and refuses to
choose when there are several — attaching you to another agent's container
would be worse than an error.

Sandboxes started by pall8t **before 0.5.0** carry no labels; restart the
agent to get one.

## Scope

A thin shell over the pall8t CLI: no socket calls, no reading pall8t's
state directory. The only contract it depends on is `pall8t ls --json`.
Needs `python3` (macOS ships one with the Xcode command line tools) and
`pall8t` on `PATH`.

It lives in `contrib/` rather than its own repository so it versions with
the CLI it drives — a `ls --json` change and the plugin that reads it land
in the same commit.
