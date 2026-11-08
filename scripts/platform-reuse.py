#!/usr/bin/env python3
"""Flag added code that rebuilds what a Base module already does.

Reads the rules in scripts/platform-reuse.toml (one data file, easy to extend)
and checks each line this change adds: tables, SQL enum types, columns and Rust
types whose names look like a Base concern (approvals, audit logs,
notifications, settings, attachments...), and code that brings its own HTTP or
SMTP client, local time or timer loop. Each hit is a CI warning naming the
Base module to use (apps/base/CAPABILITIES.md).

The check fails only when a flagged rule has no line in the pull request body,
under "## Platform reuse", of the form "<id>: uses <base module> ..." or
"<id>: no fit: <reason>". Without PR_BODY (a local run) it only warns. Files
under fixtures/, examples/, generated/ and target/ are skipped.

Usage: platform-reuse.py [--base REF]
Compares HEAD with the merge base of REF (default: origin/main, else main) in
the repository of the current directory, so a Domain or Extension runs the
Platform's copy over its own checkout (docs/domain-ci/platform-reuse.yml).
"""
import argparse
import fnmatch
import os
import pathlib
import re
import subprocess
import sys
import tomllib

RULES = pathlib.Path(__file__).resolve().with_suffix(".toml")
SHOWN = 10
SKIPPED_DIRS = {"fixtures", "examples", "generated", "target"}
SQL_TYPES = (
    r"bigint|bigserial|int|integer|smallint|serial|text|varchar|character|char|boolean|bool|"
    r"timestamptz|timestamp|date|time|uuid|jsonb|json|numeric|decimal|real|double|bytea|inet|citext"
)
SQL_NAMES = [
    ("table", re.compile(r"\bcreate\s+(?:unlogged\s+|temp(?:orary)?\s+)?table\s+(?:if\s+not\s+exists\s+)?(?:\"?\w+\"?\.)?\"?(\w+)", re.I)),
    ("table", re.compile(r"\balter\s+table\s+.*\brename\s+to\s+\"?(\w+)", re.I)),
    ("type", re.compile(r"\bcreate\s+type\s+(?:\"?\w+\"?\.)?\"?(\w+)", re.I)),
]
SQL_COLUMNS = [
    re.compile(r"\badd\s+column\s+(?:if\s+not\s+exists\s+)?\"?(\w+)", re.I),
    re.compile(r"\brename\s+column\s+\"?\w+\"?\s+to\s+\"?(\w+)", re.I),
]
SQL_COLUMN_LINE = re.compile(rf"^\s*\"?([a-z_]\w*)\"?\s+(?:{SQL_TYPES})\b", re.I)
RUST_TYPE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum|type|trait)\s+([A-Za-z_]\w*)")
COMMENT = ("//", "--", "#", "/*", "*")


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def resolve_base(requested):
    for ref in [requested] if requested else ["origin/main", "main"]:
        try:
            return git("merge-base", ref, "HEAD").strip()
        except subprocess.CalledProcessError:
            continue
    sys.exit("platform-reuse: no base ref found; fetch full history or pass --base")


def snake(name):
    return re.sub(r"(?<=[a-z0-9])(?=[A-Z])|(?<=[A-Z])(?=[A-Z][a-z])", "_", name).lower()


def load_rules():
    rules = tomllib.loads(RULES.read_text())["rule"]
    for rule in rules:
        for key in ("names", "columns"):
            rule[key] = re.compile(rule[key], re.I) if key in rule else None
        rule["code"] = [{"files": c["files"], "pattern": re.compile(c["pattern"], re.I)} for c in rule.get("code", [])]
    return rules


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


def defined_names(path, text):
    """(kind, snake_case name) of what this line defines: a table, type or column."""
    if path.endswith(".sql") or (path.endswith(".rs") and re.search(r"\b(create|alter)\s+(table|type)\b", text, re.I)):
        for kind, pattern in SQL_NAMES:
            yield from ((kind, m.lower()) for m in pattern.findall(text))
        for pattern in SQL_COLUMNS:
            yield from (("column", m.lower()) for m in pattern.findall(text))
        if path.endswith(".sql"):
            match = SQL_COLUMN_LINE.match(text)
            if match and match.group(1).lower() not in {"constraint", "primary", "unique", "check", "foreign"}:
                yield "column", match.group(1).lower()
    if path.endswith(".rs"):
        match = RUST_TYPE.match(text)
        if match:
            yield "type", snake(match.group(1))


def hits(rule, path, text):
    if any(fnmatch.fnmatch(path, glob) for glob in rule.get("skip", [])):
        return
    for kind, name in defined_names(path, text):
        pattern = rule["columns"] if kind == "column" else rule["names"]
        if pattern and pattern.search(name):
            yield f"{kind} `{name}`"
    for code in rule["code"]:
        if any(fnmatch.fnmatch(path, glob) or fnmatch.fnmatch(path.rsplit("/", 1)[-1], glob) for glob in code["files"]):
            match = code["pattern"].search(text)
            if match:
                yield f"`{match.group(0).strip()}`"


def explained(section, rule_id):
    line = re.compile(
        rf"^\W*`?{re.escape(rule_id)}`?\s*:\s*(uses?\s+.*\b(base/\w+|bilibi_\w+)|no\s+fit:\s*\S.{{9,}})", re.I
    )
    return any(line.search(text) for text in section.splitlines())


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base")
    base = resolve_base(parser.parse_args().base)
    rules = load_rules()
    found = {}
    for path, number, text in added_lines(base):
        if text.strip().startswith(COMMENT) or SKIPPED_DIRS & set(path.split("/")[:-1]):
            continue
        for rule in rules:
            for what in hits(rule, path, text):
                found.setdefault(rule["id"], []).append((path, number, what))
    body = os.environ.get("PR_BODY")
    section = ""
    if body is not None:
        match = re.search(r"^##\s+Platform reuse\s*$(.*?)(?=^##\s|\Z)", body.replace("\r", ""), re.S | re.M)
        section = match.group(1) if match else ""
    missing, summary = [], []
    for rule in rules:
        items = found.get(rule["id"])
        if not items:
            continue
        ok = body is None or explained(section, rule["id"])
        for path, number, what in items[:SHOWN]:
            print(
                f"::warning file={path},line={number}::{what} looks like what {rule['use']} already does "
                f"({rule['id']}). {rule['why']}"
            )
        if len(items) > SHOWN:
            print(f"platform-reuse: {rule['id']}: {len(items) - SHOWN} more line(s) not shown.")
        if not ok:
            missing.append(rule)
        first = ", ".join(f"{path}:{number} {what}" for path, number, what in items[:3])
        summary.append(f"- `{rule['id']}` → `{rule['use']}`: {len(items)} line(s), e.g. {first}" + ("" if ok else " **(not explained)**"))
    if summary and os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as out:
            out.write("### Added code that looks like what a Base module already does\n" + "\n".join(summary) + "\n")
    for rule in missing:
        print(
            f"::error::Platform reuse: {rule['id']} lines look like what {rule['use']} already does. "
            f"Use it, then add a line under '## Platform reuse' in the pull request body: "
            f"'{rule['id']}: uses {rule['use']} (how)' or '{rule['id']}: no fit: <why {rule['use']} does not fit>'. "
            f"Map of Base: apps/base/CAPABILITIES.md."
        )
    lines = sum(len(items) for items in found.values())
    print(f"platform-reuse: {lines} added line(s) flagged under {len(found)} rule(s), {len(missing)} rule(s) not explained.")
    sys.exit(1 if missing else 0)


main()
