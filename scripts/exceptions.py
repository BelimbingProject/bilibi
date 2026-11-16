#!/usr/bin/env python3
"""List every new escape hatch a change adds, and require the pull request to say why.

Reads the lines this change adds (comment lines excluded) and finds each new
escape hatch: `#[allow(`, `#[expect(`, `Pending::Every`, `Pending::Events`,
`begin_view_state(`, `get_detached(`, `Html::raw(` and `|safe`. Every one is
printed to the job summary, so a reviewer sees all exceptions in one place.

The check fails only when a hatch has no line in the pull request body, under
"## Exceptions", of the form "<path>: <hatch>: <reason>" (one line per file and
hatch, however many times the file adds it). `<hatch>` is `allow`, `expect`,
`Pending::Every`, `Pending::Events`, `begin_view_state`, `get_detached`,
`Html::raw` or `safe`. Without PR_BODY (a local run) it only lists.
`Html::raw` and `|safe` in apps/base/ui are the shared components and are not
listed. Files under target/ are skipped.

Usage: exceptions.py [--base REF]
Compares HEAD with the merge base of REF (default: origin/main, else main) in
the repository of the current directory, so a Domain or Extension runs the
Platform's copy over its own checkout.
"""
import argparse
import os
import re
import subprocess
import sys

COMMENT = ("//", "/*", "*")
SUFFIXES = (".rs", ".html")
HATCHES = [
    ("allow", re.compile(r"#!?\[\s*allow\(")),
    ("expect", re.compile(r"#!?\[\s*expect\(")),
    ("Pending::Every", re.compile(r"\bPending::Every\b")),
    ("Pending::Events", re.compile(r"\bPending::Events\b")),
    ("begin_view_state", re.compile(r"(?<!fn )\bbegin_view_state\(")),
    ("get_detached", re.compile(r"(?<!fn )\bget_detached\(")),
    ("Html::raw", re.compile(r"\bHtml::raw\(")),
    ("safe", re.compile(r"\|\s*safe\b")),
]
SHARED_UI = "apps/base/ui/"
SHARED_UI_HATCHES = {"Html::raw", "safe"}


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def resolve_base(requested):
    for ref in [requested] if requested else ["origin/main", "main"]:
        try:
            return git("merge-base", ref, "HEAD").strip()
        except subprocess.CalledProcessError:
            continue
    sys.exit("exceptions: no base ref found; fetch full history or pass --base")


def added_lines(base):
    """(path, line number, text) for every line this change adds."""
    path, line = None, 0
    for text in git("diff", "-U0", "-M", "--no-color", base, "HEAD").splitlines():
        if text.startswith("+++ "):
            path = text[6:] if text.startswith("+++ b/") else None
        elif text.startswith("@@"):
            line = int(re.search(r"\+(\d+)", text).group(1))
        elif text.startswith("+") and path:
            yield path, line, text[1:]
            line += 1


def explained(section, path, hatch):
    for line in section.splitlines():
        if re.fullmatch(
            rf"\s*(?:-\s+)?`?{re.escape(path)}`?\s*:\s*`?{re.escape(hatch)}`?\s*:\s*\S.*",
            line,
        ):
            return True
    return False


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base")
    base = resolve_base(parser.parse_args().base)
    found = {}
    for path, number, text in added_lines(base):
        if not path.endswith(SUFFIXES) or path.startswith("target/") or text.strip().startswith(COMMENT):
            continue
        for hatch, pattern in HATCHES:
            if pattern.search(text) and not (path.startswith(SHARED_UI) and hatch in SHARED_UI_HATCHES):
                found.setdefault((path, hatch), []).append(number)
    body = os.environ.get("PR_BODY")
    section = ""
    if body is not None:
        match = re.search(r"^##\s+Exceptions\s*$(.*?)(?=^##\s|\Z)", body.replace("\r", ""), re.S | re.M)
        section = match.group(1) if match else ""
    missing, summary = [], []
    for (path, hatch), numbers in sorted(found.items()):
        ok = body is None or explained(section, path, hatch)
        where = ", ".join(str(n) for n in numbers[:5]) + (" ..." if len(numbers) > 5 else "")
        print(f"::warning file={path},line={numbers[0]}::New escape hatch `{hatch}` x{len(numbers)} (lines {where}).")
        summary.append(f"- `{path}`: `{hatch}` x{len(numbers)} (lines {where})" + ("" if ok else " **(not explained)**"))
        if not ok:
            missing.append((path, hatch))
    if summary and os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as out:
            out.write("### New escape hatches\n" + "\n".join(summary) + "\n")
    for path, hatch in missing:
        print(
            f"::error file={path}::This change adds `{hatch}` here. Prefer the rule it escapes; if it must stay, add a line "
            f"under '## Exceptions' in the pull request body: '{path}: {hatch}: <why no other way works>'."
        )
    print(f"exceptions: {len(found)} new hatch(es) listed, {len(missing)} not explained.")
    sys.exit(1 if missing else 0)


main()
