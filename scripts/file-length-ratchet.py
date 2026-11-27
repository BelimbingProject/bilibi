#!/usr/bin/env python3
"""Warn about Rust files that are over 800 lines and new or growing.

Lists each .rs file that is new and over the limit, or already over it and
longer in this change, as a CI warning annotation and a job summary. The check
fails only when a listed file has no line in the pull request body, under
"## File length", of the form "<path>: keep: <reason>" or
"<path>: split: <what was split along which responsibility>".
Files under fixtures/, examples/, migrations/, generated/ and target/ are
skipped; lock files are not .rs files.

Usage: file-length-ratchet.py [--base REF]
Compares HEAD with the merge base of REF (default: origin/main, else main).
The pull request body comes from the PR_BODY environment variable; without it
(a local run) the script only warns. The same script is vendored unchanged into
every Bilibi repository.
"""
import argparse
import os
import re
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
    body = os.environ.get("PR_BODY")
    section = ""
    if body is not None:
        match = re.search(r"^##\s+File length\s*$(.*?)(?=^##\s|\Z)", body.replace("\r", ""), re.S | re.M)
        section = match.group(1) if match else ""
    missing, summary = [], []
    for path, before, after in failures:
        message = f"{path}: {before}, now {after} lines (limit {LIMIT})."
        print(f"::warning file={path}::{message} Split it by responsibility or say why it stays.")
        declared = body is None or any(
            path in line and re.search(r"\b(keep|split):\s*\S{3,}", line) for line in section.splitlines()
        )
        if not declared:
            missing.append(path)
        summary.append(f"- `{path}`: {before}, now {after} lines" + ("" if declared else " **(no keep/split line)**"))
    if summary and os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as out:
            out.write(f"### Rust files over {LIMIT} lines that are new or grew\n" + "\n".join(summary) + "\n")
    for path in missing:
        print(
            f"::error file={path}::Add a line under '## File length' in the pull request body: "
            f"'{path}: keep: <reason>' or '{path}: split: <what was split along which responsibility>'."
        )
    print(f"file-length-ratchet: checked {checked} changed Rust file(s), {len(failures)} listed, {len(missing)} without a keep/split line.")
    sys.exit(1 if missing else 0)


main()
