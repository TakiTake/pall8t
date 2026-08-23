#!/usr/bin/env python3
"""herdr plugin entrypoints for pall8t sandboxes.

Every entrypoint answers the same question first: *which sandbox is this
pane's?* pall8t labels each container it starts with the herdr pane it was
launched from (`pall8t.herdr.pane`), so the answer is a lookup in
`pall8t ls --json` rather than a guess from the container's name.

A thin shell over the pall8t CLI on purpose: no socket calls, no reading
pall8t's state directory, nothing that breaks when pall8t's internals
change. The one contract it depends on is `ls --json`.
"""

import json
import os
import shutil
import subprocess
import sys

PANE_LABEL = "pall8t.herdr.pane"
PROJECT_LABEL = "pall8t.project"


def fail(message: str) -> int:
    print(f"pall8t plugin: {message}", file=sys.stderr)
    return 1


def containers() -> list[dict]:
    pall8t = shutil.which("pall8t")
    if pall8t is None:
        raise LookupError("pall8t is not on PATH")
    out = subprocess.run(
        [pall8t, "ls", "--json"], capture_output=True, text=True, check=True
    ).stdout
    return json.loads(out or "[]")


def running(items: list[dict]) -> list[dict]:
    return [c for c in items if c.get("status") == "running"]


def pick(items: list[dict]) -> dict:
    """This pane's sandbox.

    By label when herdr tells us the pane (the normal path). Without one —
    an entrypoint invoked outside a pane context — a single running
    sandbox is unambiguous enough to use; two or more are not, and
    guessing would attach the user to someone else's agent.
    """
    live = running(items)
    if not live:
        raise LookupError("no pall8t sandbox is running")
    pane = os.environ.get("HERDR_PANE_ID")
    if pane:
        matches = [c for c in live if c.get("labels", {}).get(PANE_LABEL) == pane]
        if matches:
            return matches[0]
        raise LookupError(
            f"none of the {len(live)} running sandboxes is labelled for pane "
            f"{pane} — this pane's agent may have exited, or its sandbox was "
            "started by a pall8t too old to label containers (< 0.5.0)"
        )
    if len(live) == 1:
        return live[0]
    names = ", ".join(c.get("name", "?") for c in live)
    raise LookupError(f"several sandboxes are running and no pane to choose by: {names}")


def cmd_status(_args: list[str]) -> int:
    items = containers()
    live = running(items)
    try:
        mine = pick(items)
    except LookupError as e:
        mine = None
        print(f"this pane: {e}\n")
    if mine:
        labels = mine.get("labels", {})
        print(f"container : {mine.get('name')}")
        print(f"image     : {mine.get('image')}")
        print(f"project   : {labels.get(PROJECT_LABEL, '?')}")
        print(f"pall8t    : {labels.get('pall8t.version', '?')}")
        print(f"herdr     : pane {labels.get(PANE_LABEL, '-')}, "
              f"sandbox {labels.get('pall8t.herdr.sandbox', '-')}")
        worktree = labels.get("pall8t.worktree.git_dir")
        if worktree:
            print(f"worktree  : main .git at {worktree}")
        print()
    others = [c for c in live if mine is None or c.get("name") != mine.get("name")]
    print(f"other running sandboxes: {len(others)}")
    for c in others:
        print(f"  {c.get('name')}  {c.get('labels', {}).get(PROJECT_LABEL, '?')}")
    return 0


def cmd_shell(_args: list[str]) -> int:
    mine = pick(containers())
    name = mine["name"]
    print(f"pall8t exec {name} — the agent keeps running; this is a second shell.\n")
    # bash first, sh as the fallback: the default image has bash, a
    # minimal custom one may not.
    for shell in ("bash", "sh"):
        code = subprocess.run(["pall8t", "exec", name, "--", shell]).returncode
        if code != 127:
            return code
    return fail(f"no bash or sh inside {name}")


def cmd_rebuild(args: list[str]) -> int:
    mine = pick(containers())
    project = mine.get("labels", {}).get(PROJECT_LABEL)
    if not project:
        return fail("this sandbox carries no project label to rebuild in")
    print(f"pall8t build in {project}\n")
    # The build applies to the *next* run: the container in front of you
    # keeps the image it booted, which is the point of an explicit rebuild.
    code = subprocess.run(["pall8t", "build", *args], cwd=project).returncode
    print("\n(the running sandbox keeps its current image; the next "
          "`pall8t run` picks this one up)")
    input("press enter to close ")
    return code


def cmd_stop(_args: list[str]) -> int:
    mine = pick(containers())
    name = mine["name"]
    return subprocess.run(["pall8t", "stop", name]).returncode


COMMANDS = {
    "status": cmd_status,
    "shell": cmd_shell,
    "rebuild": cmd_rebuild,
    "stop": cmd_stop,
}


def main(argv: list[str]) -> int:
    if not argv or argv[0] not in COMMANDS:
        return fail(f"usage: pall8t_plugin.py <{'|'.join(COMMANDS)}> [args]")
    try:
        return COMMANDS[argv[0]](argv[1:])
    except LookupError as e:
        return fail(str(e))
    except subprocess.CalledProcessError as e:
        return fail(f"pall8t exited {e.returncode}: {(e.stderr or '').strip()}")
    except json.JSONDecodeError as e:
        return fail(f"could not parse `pall8t ls --json` ({e})")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
