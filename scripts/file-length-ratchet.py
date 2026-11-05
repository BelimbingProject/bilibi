#!/usr/bin/env python3
"""Fail a change that makes a Rust file longer than 800 lines.

A new .rs file over the limit fails. A file already over the limit fails only
when the change makes it longer, so big files can shrink but never grow.
Files under fixtures/, examples/, migrations/, generated/ and target/ are
skipped. Lock files are not .rs files, so they never match.

Usage: file-length-ratchet.py [--base REF]
Compares HEAD with the merge base of REF (default: origin/main, else main).
The same script is vendored unchanged into every Bilibi repository.
"""
import argparse
import subprocess
import sys

LIMIT = 800
SKIPPED_DIRS = {"fixtures", "examples", "migrations", "generated", "target"}


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True).stdout


def lines(ref, path):
    data = git("show", f"{ref}:{path}")
    return data.count(b"\n") + (1 if data and not data.endswith(b"\n") else 0)


def skipped(path):
    return not path.endswith(".rs") or bool(SKIPPED_DIRS & set(path.split("/")[:-1]))


def resolve_base(requested):
    for ref in ([requested] if requested else ["origin/main", "main"]):
        try:
            return git("merge-base", ref, "HEAD").decode().strip()
        except subprocess.CalledProcessError:
            continue
    sys.exit("file-length-ratchet: no base ref found; fetch full history or pass --base")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base")
    base = resolve_base(parser.parse_args().base)
    out = git("diff", "--name-status", "-M", "-z", "--diff-filter=AMR", base, "HEAD").split(b"\0")
    failures, checked, i = [], 0, 0
    while i < len(out) and out[i]:
        status = out[i].decode()
        if status.startswith("R"):
            old, new, i = out[i + 1].decode(), out[i + 2].decode(), i + 3
        else:
            old = new = out[i + 1].decode()
            i += 2
        if skipped(new):
            continue
        checked += 1
        after = lines("HEAD", new)
        before = 0 if status == "A" else lines(base, old)
        if after > LIMIT and after > before:
            failures.append((new, "new file" if status == "A" else f"{before} lines before", after))
    for path, before, after in failures:
        print(f"{path}: {before}, now {after} lines (limit {LIMIT}). Split it or keep it from growing.")
    print(f"file-length-ratchet: checked {checked} changed Rust file(s), {len(failures)} over the limit and growing.")
    sys.exit(1 if failures else 0)


main()
