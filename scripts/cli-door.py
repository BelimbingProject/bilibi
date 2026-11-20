#!/usr/bin/env python3
"""Keep the agent command line (apps/cli, `bilibi-axi`) a door and nothing more.

The command line is one more door to the operation contracts that `/api/v1`,
`/mcp` and the pages call. A door holds no decision: validation, permission,
previews and writes are the server's answers. This check holds the structural
half of that rule, the half a compiler can be asked about: the program may not
link a Bilibi crate or a database driver, so it has nothing to decide with and
no way to reach data except the versioned API over HTTP.

It reads Cargo's own resolved package graph (`cargo metadata`), not the
manifest's text, so a renamed, optional, target-specific or transitive
dependency counts as what Cargo would build. Dev-dependencies are left out:
the two-doors walkthrough stands up a real server to call.

The behavioural half is that walkthrough (apps/cli/examples), which sends the
same request through the command line and through MCP and compares the answers.

Usage: cli-door.py
"""
import json
import subprocess
import sys

PACKAGE = "bilibi-axi"
# A database driver would let the program read or write around the API.
DRIVERS = {"tokio-postgres", "postgres", "deadpool-postgres", "sqlx", "diesel", "rusqlite"}


def main():
    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--locked"],
            check=True,
            capture_output=True,
        ).stdout
    )
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    roots = [package["id"] for package in metadata["packages"] if package["name"] == PACKAGE]
    if len(roots) != 1:
        print(f"::error::Expected one `{PACKAGE}` package in the workspace, found {len(roots)}.")
        return 1
    # Everything the program links: normal and build dependencies, transitively.
    seen, queue, via = set(), [roots[0]], {}
    while queue:
        current = queue.pop()
        for dependency in nodes[current]["deps"]:
            kinds = {kind["kind"] for kind in dependency["dep_kinds"]}
            if kinds <= {"dev"} or dependency["pkg"] in seen:
                continue
            seen.add(dependency["pkg"])
            via[dependency["pkg"]] = current
            queue.append(dependency["pkg"])
    failures = 0
    for identifier in sorted(seen):
        package = packages[identifier]
        name = package["name"]
        local = package["source"] is None
        if not (local or name in DRIVERS):
            continue
        # Name each offending edge once, not everything it drags in.
        parent = packages[via[identifier]]
        if via[identifier] != roots[0] and parent["source"] is None:
            continue
        path, at = [name], identifier
        while via[at] != roots[0]:
            at = via[at]
            path.append(packages[at]["name"])
        chain = " <- ".join(path + [PACKAGE])
        what = "a workspace crate" if local else "a database driver"
        print(
            f"::error file=apps/cli/Cargo.toml::`{PACKAGE}` links {what}: {chain}. The command line is a door: "
            "it may only call the versioned API over HTTP, so it must not link Bilibi's crates or a database "
            "driver. Put the rule in the operation contract and let the command line print the server's answer "
            "(apps/cli/README.md, \"Only a door\")."
        )
        failures += 1
    if failures:
        return 1
    print(f"{PACKAGE} links {len(seen)} crates, none of them a workspace crate or a database driver.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
