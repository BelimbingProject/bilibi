#!/usr/bin/env python3
"""Fail on code the project forbids outright, with a message that says what to do instead.

Scans every tracked file of the repository in the current directory (so a
Domain or Extension runs the Platform's copy over its own checkout):

- `#[test]`, `#[cfg(test)]` and `#[tokio::test]` in Rust: Bilibi has no unit tests.
- `window.confirm` in Rust, templates and scripts: use `ConfirmDialog`.

Comment lines are skipped. Usage: forbidden-code.py
"""
import re
import subprocess
import sys

RULES = [
    (
        re.compile(r"#\[\s*(?:test|cfg\(\s*test\s*\)|tokio::test)\s*\]"),
        (".rs",),
        "Bilibi has no unit tests. Prove the behaviour with a walkthrough under `examples/` and list it in "
        "`scripts/run-walkthroughs.sh`: a unit test checks a copy of the logic, a walkthrough checks the running system.",
    ),
    (
        re.compile(r"\bwindow\.confirm\b"),
        (".rs", ".html", ".js", ".mjs", ".ts"),
        "Render `ConfirmDialog` while the request is pending; `window.confirm` cannot be styled, translated or "
        "driven by an agent (apps/base/ui/AGENTS.md).",
    ),
]
COMMENT = ("//", "/*", "*")


def main():
    files = subprocess.run(["git", "ls-files", "-z"], check=True, capture_output=True).stdout.decode().split("\0")
    failures = 0
    for path in filter(None, files):
        wanted = [rule for rule in RULES if path.endswith(rule[1])]
        if not wanted or path.startswith("target/") or path == "scripts/forbidden-code.py":
            continue
        try:
            text = open(path, encoding="utf-8").read()
        except (OSError, UnicodeDecodeError):
            continue
        for number, line in enumerate(text.splitlines(), 1):
            if line.strip().startswith(COMMENT):
                continue
            for pattern, _, message in wanted:
                if pattern.search(line):
                    print(f"::error file={path},line={number}::{message}")
                    failures += 1
    print(f"forbidden-code: {failures} forbidden use(s).")
    sys.exit(1 if failures else 0)


main()
