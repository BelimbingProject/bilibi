#!/usr/bin/env python3
"""Fail when an escape hatch that should only shrink grows.

Counts occurrences of each hatch at the merge base and at HEAD (comment lines
excluded) and fails when HEAD has more. A pull request that removes some
hatches prints the new, lower count. Each rule is a lint that cannot block yet
because old sites remain; the count only falls until it reaches 0, and then the
rule becomes a blocking lint or type (see clippy.toml).

- `Html::raw(` outside apps/base/ui (also `clippy::disallowed_methods`).
- `|safe` outside apps/base/ui.
- `Pending::Every` and `Pending::Events` page-write allowances.

Usage: ratchets.py [--base REF]
Compares HEAD with the merge base of REF (default: origin/main, else main).
"""
import argparse
import re
import subprocess
import sys

COMMENT = ("//", "/*", "*")
RATCHETS = [
    {
        "name": "Html::raw outside base/ui",
        "pattern": re.compile(r"\bHtml::raw\("),
        "suffixes": (".rs",),
        "skip": ("apps/base/ui/",),
        "message": "`Html::raw` skips escaping and is for shared components in `base/ui`. Render a component "
        "(`apps/base/ui/README.md`) or use `Html::text`; hand-built markup has already shipped unbalanced tags (#331).",
    },
    {
        "name": "|safe outside base/ui",
        "pattern": re.compile(r"\|\s*safe\b"),
        "suffixes": (".html", ".rs"),
        "skip": ("apps/base/ui/",),
        "message": "`|safe` is not needed: pass an `Html` value, which Askama already trusts. "
        "A `String` marked safe is an escaping hole.",
    },
    {
        "name": "Pending::Every and Pending::Events",
        "pattern": re.compile(r"\bPending::(?:Every|Events)\b"),
        "suffixes": (".rs",),
        "skip": ("apps/base/module_registry/",),
        "message": "A `Pending` allowance lets a page write skip its operation contract. Declare the write in "
        "`PageWrites` and call `PageContracts::submit`, so agents, bulk runs and rules pass the same checks (ADR 01).",
    },
]


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def resolve_base(requested):
    for ref in [requested] if requested else ["origin/main", "main"]:
        try:
            return git("merge-base", ref, "HEAD").strip()
        except subprocess.CalledProcessError:
            continue
    sys.exit("ratchets: no base ref found; fetch full history or pass --base")


def count(ref, ratchet):
    n = 0
    found = subprocess.run(
        ["git", "grep", "-I", "-n", "-P", ratchet["pattern"].pattern, ref, "--", "apps", "fixtures"],
        capture_output=True,
        text=True,
    ).stdout
    for hit in found.splitlines():
        path, _, rest = hit[len(ref) + 1 :].partition(":")
        text = rest.partition(":")[2]
        if (
            path.endswith(ratchet["suffixes"])
            and not path.startswith(ratchet["skip"])
            and not text.strip().startswith(COMMENT)
        ):
            n += len(ratchet["pattern"].findall(text))
    return n


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base")
    base = resolve_base(parser.parse_args().base)
    grew = 0
    for ratchet in RATCHETS:
        before, after = count(base, ratchet), count("HEAD", ratchet)
        print(f"ratchets: {ratchet['name']}: {before} -> {after}")
        if after > before:
            grew += 1
            print(f"::error::{ratchet['name']} grew from {before} to {after}; it may only fall. {ratchet['message']}")
    sys.exit(1 if grew else 0)


main()
