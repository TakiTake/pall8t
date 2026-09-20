#!/usr/bin/env python3
"""Report drift between herdr's socket-API method inventory and the
classification pall8t's relay applies to it (src/relay.rs).

The relay decides what a sandboxed agent may send across the bridge. Its
`READ` list — the methods allowed even under `[herdr] sandbox =
"readonly"` — is a hand-made snapshot of herdr's inventory, so it goes
stale silently: a read method a newer herdr adds is classified `Mutate`
and denied in readonly mode, and nobody finds out.

This is a REPORT, not a gate (issue #59, same posture as the mutation and
hygiene workflows). Classification stays a human decision: the script
says what changed, never edits the list. Every *finding* exits 0, and so
does a missing herdr — that is a skip, not a failure.

What does exit non-zero is this script being unable to do its job at
all: schema output that is not JSON, a schema with no methods in it, or
a `relay.rs` whose `READ`/`ADMIN_NAMESPACES` it cannot parse. None of
those are drift — they are the report checking nothing, and a drift
report that silently checks nothing is worse than a red weekly job,
because it reads exactly like a clean one.

Inventory source: `herdr api schema --json`, whose `schemas.request.oneOf`
carries one entry per method. Pass --schema to read a saved copy instead.

Caveat the report repeats: the schema is not the whole inventory. herdr
0.8's `pane.graphics.stream` is documented and served, yet absent from
the schema (it hijacks the connection rather than answering in the
request/response shape). So "absent from the schema" is a prompt to go
look, never a licence to delete a READ entry.
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
RELAY = REPO / "src" / "relay.rs"


def herdr_methods(schema: dict) -> set[str]:
    """Request method names, from the request schema's per-method oneOf."""
    variants = schema.get("schemas", {}).get("request", {}).get("oneOf", [])
    methods = set()
    for variant in variants:
        const = variant.get("properties", {}).get("method", {}).get("const")
        if isinstance(const, str):
            methods.add(const)
    return methods


def relay_classification(source: str) -> tuple[set[str], list[str]]:
    """pall8t's READ allowlist and admin namespaces, parsed from relay.rs."""
    read = _const_list(source, "READ")
    admin = _const_list(source, "ADMIN_NAMESPACES")
    return read, sorted(admin)


def _const_list(source: str, name: str) -> set[str]:
    match = re.search(rf"const {name}: &\[&str\] = &\[(.*?)\];", source, re.S)
    if not match:
        raise SystemExit(f"cannot find `{name}` in {RELAY} — did the relay change shape?")
    return set(re.findall(r'"([^"]+)"', match.group(1)))


def classify(method: str, read: set[str], admin: list[str]) -> str:
    """Mirror of relay::classify — exact READ match wins over namespace."""
    if method in read:
        return "Read"
    if any(method.startswith(ns) for ns in admin):
        return "HostAdmin"
    return "Mutate"


def load_schema(args) -> dict | None:
    """The schema, or `None` for the one case that is a legitimate skip:
    no herdr to ask. Unparseable output is not that case — it is the
    thing this script is here to notice, so it stops loudly."""
    if args.schema:
        try:
            return _parse(Path(args.schema).read_text(), args.schema)
        except OSError as e:
            raise SystemExit(f"cannot read {args.schema}: {e}") from e
    try:
        out = subprocess.run(
            [args.herdr, "api", "schema", "--json"],
            capture_output=True, text=True, check=True,
        ).stdout
    except (FileNotFoundError, subprocess.CalledProcessError) as e:
        print(f"skipped: cannot read herdr's schema via `{args.herdr} api schema --json` ({e})")
        return None
    return _parse(out, f"`{args.herdr} api schema --json`")


def _parse(text: str, source: str) -> dict:
    """JSON, or a one-line failure rather than a traceback. A herdr that
    prints anything alongside its JSON — a deprecation notice, a progress
    line — lands here, and that is a herdr change worth a human look,
    which is this script's whole subject."""
    try:
        return json.loads(text)
    except json.JSONDecodeError as e:
        raise SystemExit(
            f"cannot parse the schema from {source}: {e}. "
            "Not drift — the inventory could not be read at all, so this "
            "report would have checked nothing."
        ) from e


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--herdr", default="herdr", help="herdr binary to ask (default: herdr on PATH)")
    parser.add_argument("--schema", help="read a saved `herdr api schema --json` instead")
    args = parser.parse_args()

    schema = load_schema(args)
    if schema is None:
        return 0

    methods = herdr_methods(schema)
    if not methods:
        # Same class as unparseable output, so the same treatment: a
        # schema that parsed but yielded no methods means the walk below
        # compares the READ list against nothing and reports clean.
        raise SystemExit(
            "no request methods found under `schemas.request.oneOf` — the "
            "schema parsed but its shape has changed, so there was nothing "
            "to compare the relay's list against."
        )

    read, admin = relay_classification(RELAY.read_text())
    protocol = schema.get("protocol", "?")

    mutate = sorted(m for m in methods if classify(m, read, admin) == "Mutate")
    stale = sorted(read - methods)

    print(f"herdr protocol {protocol}: {len(methods)} request methods")
    print(f"pall8t relay: {len(read)} READ entries, admin namespaces {admin}")
    print()
    print(f"## Classified Mutate ({len(mutate)})")
    print("Allowed under `full`, denied under `readonly`. Any that are pure")
    print("inspection belong in READ — that is the drift this job exists for.")
    for m in mutate:
        print(f"  {m}")
    print()
    print(f"## READ entries absent from the schema ({len(stale)})")
    if stale:
        print("Check each against herdr's docs/source before touching it: the")
        print("schema omits methods that hijack the connection instead of")
        print("answering in the request/response shape (`pane.graphics.stream`")
        print("is served by 0.8 but not in its schema). Only a method herdr")
        print("really dropped should leave READ.")
        for m in stale:
            print(f"  {m}")
    else:
        print("  none")
    return 0


if __name__ == "__main__":
    sys.exit(main())
