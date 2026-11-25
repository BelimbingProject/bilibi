#!/usr/bin/env python3
"""Create and verify Bilibi's installation-pinned release proofs."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import stat
import urllib.request
from datetime import datetime, timezone


PAYLOAD_TYPE = "application/vnd.bilibi.release.v1+json"
PURPOSE = "https://bilibi.my/provenance/release/v1"
# A release tag names one channel: a stable setup-vX.Y.Z or a nightly-YYYY-MM-DD.
# A policy is pinned to one channel and never mixes the two.
TAG = re.compile(r"setup-v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")
NIGHTLY = re.compile(r"nightly-([0-9]{4})-([0-9]{2})-([0-9]{2})\Z")
# The order of one policy's tags: a setup version or a nightly date. A tag of
# the other channel has no order here, so it can never be compared or admitted.
Order = tuple[int, int, int, int]
REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
TOKEN = re.compile(r"[A-Za-z0-9_.-]{1,128}\Z")
KINDS = ("setup", "installer", "server-tools")
MAX_PROOF_BYTES = 64 * 1024
ED25519_SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")


class Refusal(Exception):
    pass


def refuse(message: str) -> None:
    raise Refusal(message)


def strict_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            refuse(f"the release proof repeats {key}")
        result[key] = value
    return result


def read_json(path: Path, *, limit: int = MAX_PROOF_BYTES) -> dict[str, object]:
    try:
        raw = path.read_bytes()
    except OSError as error:
        refuse(f"cannot read {path}: {error}")
    if len(raw) > limit:
        refuse(f"{path} is larger than a release proof can be")
    try:
        value = json.loads(raw, object_pairs_hook=strict_object)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        refuse(f"{path} is not valid JSON: {error}")
    if not isinstance(value, dict):
        refuse(f"{path} must contain one JSON object")
    return value


def exact_fields(value: dict[str, object], expected: set[str], what: str) -> None:
    got = set(value)
    if got != expected:
        refuse(f"{what} fields are not exactly {', '.join(sorted(expected))}")


def command(arguments: list[str], *, data: bytes | None = None, pass_fds: tuple[int, ...] = ()) -> bytes:
    try:
        result = subprocess.run(
            arguments,
            input=data,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            pass_fds=pass_fds,
        )
    except FileNotFoundError:
        refuse("OpenSSL is not installed, so the operator release signature cannot be checked")
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", "replace").strip()
        refuse(detail or "OpenSSL refused the release signature")
    return result.stdout


def public_key_from_private(path: Path, *, der: bool = False) -> bytes:
    arguments = ["openssl", "pkey", "-in", str(path), "-pubout"]
    if der:
        arguments.extend(["-outform", "DER"])
    return command(arguments)


def public_key(path: Path, *, der: bool = False) -> bytes:
    arguments = ["openssl", "pkey", "-pubin", "-in", str(path), "-pubout"]
    if der:
        arguments.extend(["-outform", "DER"])
    return command(arguments)


def fingerprint(der: bytes) -> str:
    if len(der) != len(ED25519_SPKI_PREFIX) + 32 or not der.startswith(ED25519_SPKI_PREFIX):
        refuse("the release signing key must be Ed25519")
    return f"sha256:{hashlib.sha256(der).hexdigest()}"


def digest(path: Path) -> str:
    hasher = hashlib.sha256()
    try:
        with path.open("rb") as opened:
            while block := opened.read(1024 * 1024):
                hasher.update(block)
    except OSError as error:
        refuse(f"cannot read {path}: {error}")
    return hasher.hexdigest()


def valid_repository(value: str) -> str:
    if not REPOSITORY.fullmatch(value):
        refuse("the repository must be owner/name")
    return value


def valid_workflow(value: str) -> str:
    if not (1 <= len(value) <= 255) or any(ord(char) < 0x21 or ord(char) > 0x7E for char in value):
        refuse("the workflow identity must be 1 to 255 visible ASCII characters")
    return value


def channel(value: str) -> str:
    """`setup` or `nightly`: which channel a canonical tag belongs to."""
    if TAG.fullmatch(value):
        return "setup"
    if NIGHTLY.fullmatch(value):
        return "nightly"
    refuse("the release tag must be setup-vX.Y.Z or nightly-YYYY-MM-DD")


def order(value: str) -> Order:
    """A tag's place in its channel: setup versions by number, nightlies by date."""
    if match := TAG.fullmatch(value):
        return (0, *(int(part) for part in match.groups()))
    if match := NIGHTLY.fullmatch(value):
        try:
            datetime(int(match[1]), int(match[2]), int(match[3]))
        except ValueError:
            refuse(f"{value} is not a real date")
        return (1, *(int(part) for part in match.groups()))
    refuse("the release tag must be setup-vX.Y.Z or nightly-YYYY-MM-DD")


def policy_order(minimum: str, tag: str) -> Order:
    """A tag's place under one policy: both tags must be of the policy's channel.

    A policy accepts one channel only: no nightly under a setup policy, no
    setup under a nightly one. Comparing across channels would let a nightly
    pass a setup floor or a setup pass a nightly one.
    """
    if channel(tag) != channel(minimum):
        refuse(
            f"release {tag} is a {channel(tag)} tag but this installation accepts only {channel(minimum)} tags from {minimum}"
        )
    return order(tag)


def valid_token(value: str, what: str) -> str:
    if not TOKEN.fullmatch(value):
        refuse(f"the {what} is invalid")
    return value


def b64(value: bytes) -> str:
    return base64.b64encode(value).decode("ascii")


def unb64(value: object, what: str) -> bytes:
    if not isinstance(value, str):
        refuse(f"the release proof {what} must be base64 text")
    try:
        return base64.b64decode(value, validate=True)
    except ValueError:
        refuse(f"the release proof {what} is not valid base64")


def canonical(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode("ascii")


def pae(payload_type: str, payload: bytes) -> bytes:
    encoded_type = payload_type.encode("utf-8")
    return b"DSSEv1 %d " % len(encoded_type) + encoded_type + b" %d " % len(payload) + payload


def atomic_json(path: Path, value: object, mode: int = 0o640) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=path.parent, delete=False) as opened:
        temporary = Path(opened.name)
        json.dump(value, opened, sort_keys=True, separators=(",", ":"))
        opened.write("\n")
        opened.flush()
        os.fsync(opened.fileno())
    os.chmod(temporary, mode)
    os.replace(temporary, path)
    directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def load_trust(path: Path) -> dict[str, object]:
    trust = read_json(path)
    common = {
        "version", "kind", "repository", "repository_id", "workflow", "minimum_tag",
        "changed_at", "changed_by",
    }
    if trust.get("kind") == "operator":
        exact_fields(trust, common | {"public_key", "fingerprint"}, "release trust")
    elif trust.get("kind") == "github":
        exact_fields(trust, common, "release trust")
    else:
        refuse("the release trust kind must be github or operator")
    if trust.get("version") != 1:
        refuse("the release trust version is not supported")
    valid_repository(str(trust.get("repository", "")))
    if not isinstance(trust.get("repository_id"), int) or int(trust["repository_id"]) <= 0:
        refuse("the trusted repository ID must be a positive number")
    valid_workflow(str(trust.get("workflow", "")))
    order(str(trust.get("minimum_tag", "")))
    changed_at = trust.get("changed_at")
    if not isinstance(changed_at, str) or not re.fullmatch(
        r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", changed_at
    ):
        refuse("the release trust change time is invalid")
    try:
        datetime.strptime(changed_at, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError:
        refuse("the release trust change time is invalid")
    valid_workflow(str(trust.get("changed_by", "")))
    return trust


def make_trust(args: argparse.Namespace) -> None:
    value: dict[str, object] = {
        "version": 1,
        "kind": args.kind,
        "repository": valid_repository(args.repository),
        "repository_id": args.repository_id,
        "workflow": valid_workflow(args.workflow),
        "minimum_tag": args.minimum_tag,
        "changed_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "changed_by": valid_workflow(args.changed_by),
    }
    minimum = order(args.minimum_tag)
    if args.repository_id <= 0:
        refuse("the trusted repository ID must be a positive number")
    if args.existing_releases is not None:
        for recipe in args.existing_releases.glob("*/release.json"):
            if not recipe.parent.name.isdecimal() or recipe.parent.is_symlink() or recipe.is_symlink():
                continue
            setup = read_json(recipe).get("setup")
            if not isinstance(setup, dict) or not setup.get("tag"):
                continue
            if channel(str(setup["tag"])) == channel(args.minimum_tag) and order(str(setup["tag"])) >= minimum:
                refuse(f"the initial minimum tag must be newer than the existing release {setup['tag']}")
    if args.kind == "operator":
        if args.public_key is None:
            refuse("operator trust needs --public-key")
        pem = public_key(args.public_key).decode("ascii")
        value["public_key"] = pem
        value["fingerprint"] = fingerprint(public_key(args.public_key, der=True))
    elif args.public_key is not None:
        refuse("GitHub trust does not take --public-key")
    atomic_json(args.output, value)
    print(json.dumps({key: value[key] for key in value if key != "public_key"}, sort_keys=True))


def sign(args: argparse.Namespace) -> None:
    repository = valid_repository(args.repository)
    workflow = valid_workflow(args.workflow)
    channel(args.tag)
    valid_token(args.target, "target")
    der = public_key_from_private(args.private_key, der=True)
    key_id = fingerprint(der)
    statement = {
        "_type": PURPOSE,
        "artifact": {
            "kind": args.kind,
            "sha256": digest(args.artifact),
            "target": args.target,
        },
        "repository": {"id": args.repository_id, "name": repository},
        "tag": args.tag,
        "workflow": workflow,
    }
    payload = canonical(statement)
    payload_fd = os.memfd_create("bilibi-release-pae")
    try:
        os.write(payload_fd, pae(PAYLOAD_TYPE, payload))
        os.lseek(payload_fd, 0, os.SEEK_SET)
        signature = command(
            [
                "openssl", "pkeyutl", "-sign", "-rawin", "-inkey", str(args.private_key),
                "-in", f"/proc/self/fd/{payload_fd}",
            ],
            pass_fds=(payload_fd,),
        )
    finally:
        os.close(payload_fd)
    envelope = {
        "payloadType": PAYLOAD_TYPE,
        "payload": b64(payload),
        "signatures": [{"keyid": key_id, "sig": b64(signature)}],
    }
    atomic_json(args.output, envelope, 0o644)
    print(json.dumps({"fingerprint": key_id, "sha256": statement["artifact"]["sha256"]}, sort_keys=True))


def verify_operator(args: argparse.Namespace) -> None:
    trust = load_trust(args.trust)
    if trust["kind"] != "operator":
        refuse("this installation does not trust operator-signed release proofs")
    minimum = str(trust["minimum_tag"])
    if policy_order(minimum, args.tag) < order(minimum):
        refuse(f"release {args.tag} is below this installation's minimum trusted tag {minimum}")
    envelope = read_json(args.proof)
    exact_fields(envelope, {"payloadType", "payload", "signatures"}, "release proof")
    if envelope["payloadType"] != PAYLOAD_TYPE:
        refuse("the release proof payload type is not supported")
    signatures = envelope["signatures"]
    if not isinstance(signatures, list) or len(signatures) != 1 or not isinstance(signatures[0], dict):
        refuse("the release proof must carry exactly one signature")
    signature = signatures[0]
    exact_fields(signature, {"keyid", "sig"}, "release signature")
    if signature["keyid"] != trust["fingerprint"]:
        refuse("the release proof was signed by a different key")
    payload = unb64(envelope["payload"], "payload")
    try:
        statement = json.loads(payload, object_pairs_hook=strict_object)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        refuse(f"the release proof statement is not valid JSON: {error}")
    if not isinstance(statement, dict) or canonical(statement) != payload:
        refuse("the release proof statement is not canonical JSON")
    exact_fields(statement, {"_type", "artifact", "repository", "tag", "workflow"}, "release statement")
    if statement["_type"] != PURPOSE:
        refuse("the release statement purpose is not supported")
    artifact = statement["artifact"]
    repository = statement["repository"]
    if not isinstance(artifact, dict) or not isinstance(repository, dict):
        refuse("the release proof artifact and repository must be objects")
    exact_fields(artifact, {"kind", "sha256", "target"}, "release artifact")
    exact_fields(repository, {"id", "name"}, "release repository")
    expected = {
        "repository": trust["repository"],
        "repository_id": trust["repository_id"],
        "workflow": trust["workflow"],
        "tag": args.tag,
        "kind": args.kind,
    }
    got = {
        "repository": repository["name"],
        "repository_id": repository["id"],
        "workflow": statement["workflow"],
        "tag": statement["tag"],
        "kind": artifact["kind"],
    }
    for key, wanted in expected.items():
        if got[key] != wanted:
            refuse(f"the release proof has the wrong {key.replace('_', ' ')}")
    valid_token(str(artifact["target"]), "target")
    if artifact["target"] != args.target:
        refuse("the release proof has the wrong target")
    actual_digest = digest(args.artifact)
    if artifact["sha256"] != actual_digest:
        refuse("the release artifact does not match its signed SHA-256 digest")
    pem = str(trust["public_key"]).encode("ascii")
    trusted_der = command(["openssl", "pkey", "-pubin", "-outform", "DER"], data=pem)
    if fingerprint(trusted_der) != trust["fingerprint"]:
        refuse("the trusted public key does not match its recorded fingerprint")
    key_fd = os.memfd_create("bilibi-release-public-key")
    signature_fd = os.memfd_create("bilibi-release-signature")
    payload_fd = os.memfd_create("bilibi-release-pae")
    try:
        os.write(key_fd, pem)
        os.lseek(key_fd, 0, os.SEEK_SET)
        os.write(signature_fd, unb64(signature["sig"], "signature"))
        os.lseek(signature_fd, 0, os.SEEK_SET)
        os.write(payload_fd, pae(PAYLOAD_TYPE, payload))
        os.lseek(payload_fd, 0, os.SEEK_SET)
        command(
            [
                "openssl", "pkeyutl", "-verify", "-rawin", "-pubin",
                "-inkey", f"/proc/self/fd/{key_fd}",
                "-sigfile", f"/proc/self/fd/{signature_fd}",
                "-in", f"/proc/self/fd/{payload_fd}",
            ],
            pass_fds=(key_fd, signature_fd, payload_fd),
        )
    finally:
        os.close(key_fd)
        os.close(signature_fd)
        os.close(payload_fd)
    print(json.dumps({"kind": args.kind, "sha256": actual_digest, "tag": args.tag, "target": artifact["target"]}, sort_keys=True))


def trust_values(args: argparse.Namespace) -> None:
    trust = load_trust(args.trust)
    for key in ("kind", "repository", "repository_id", "workflow", "minimum_tag"):
        print(trust[key])


def check_tag(args: argparse.Namespace) -> None:
    trust = load_trust(args.trust)
    check_policy_tag(trust, args.tag)


def check_policy_tag(trust: dict[str, object], tag: str) -> None:
    minimum = str(trust["minimum_tag"])
    if policy_order(minimum, tag) < order(minimum):
        refuse(f"release {tag} is below this installation's minimum trusted tag {minimum}")


def check_admission(args: argparse.Namespace) -> None:
    trust = load_trust(args.trust)
    source = json.loads(args.source)
    for policy_field, source_field in (
        ("kind", "proof"), ("repository", "repository"),
        ("repository_id", "repository_id"), ("workflow", "workflow"),
        ("fingerprint", "fingerprint"),
    ):
        if trust.get(policy_field) != source.get(source_field):
            refuse("the release was verified under a different release trust policy")
    check_policy_tag(trust, source.get("tag") or "")


def load_replay(path: Path) -> dict[str, object]:
    """The replay record, checked: one highest tag and digest per channel."""
    state = read_json(path)
    exact_fields(state, {"version", "highest"}, "release replay state")
    admitted = state["highest"]
    if state["version"] != 1 or not isinstance(admitted, dict):
        refuse("the release replay state is not valid")
    for key, entry in admitted.items():
        if key not in ("setup", "nightly") or not isinstance(entry, dict):
            refuse("the release replay state is not valid")
        exact_fields(entry, {"tag", "digest"}, "release replay entry")
        if channel(str(entry["tag"])) != key or not re.fullmatch(r"[0-9a-f]{64}", str(entry["digest"])):
            refuse("the release replay state is not valid")
    return state


def check_replay(args: argparse.Namespace, *, resume: bool = False) -> None:
    """Refuse a tag at or below this channel's highest admitted one.

    The replay record holds one highest tag and digest per channel, keyed by
    the channel's name, so a nightly and a setup release never raise or lower
    each other's floor and an update from one channel is not read as a
    downgrade of the other.
    """
    wanted = order(args.tag)
    name = channel(args.tag)
    if not re.fullmatch(r"[0-9a-f]{64}", args.digest):
        refuse("the admitted release digest is invalid")
    if not args.state.exists():
        return
    admitted = load_replay(args.state)["highest"]
    if name not in admitted:
        return
    highest = str(admitted[name]["tag"])
    floor = order(highest)
    if wanted < floor:
        refuse(f"release {args.tag} is older than the highest admitted release {highest}")
    if wanted == floor:
        if args.digest != admitted[name]["digest"]:
            refuse(f"release {args.tag} cannot replace the admitted bytes for that tag")
        if not (resume or args.resume):
            refuse(f"release {args.tag} has already been admitted")


def record_replay(args: argparse.Namespace) -> None:
    check_replay(args, resume=True)
    admitted: dict[str, object] = {}
    if args.state.exists():
        admitted = load_replay(args.state)["highest"]
    admitted[channel(args.tag)] = {"tag": args.tag, "digest": args.digest}
    atomic_json(args.state, {"version": 1, "highest": admitted})


def verify_github(args: argparse.Namespace) -> None:
    repository = valid_repository(args.repository)
    workflow = valid_workflow(args.workflow)
    channel(args.tag)
    env = {key: value for key, value in os.environ.items() if key not in (
        "GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN",
        "GH_CONFIG_DIR", "GH_HOST",
    )}
    env.update(GH_HOST="github.com", GH_PROMPT_DISABLED="1", NO_COLOR="1")
    verify = [
        "gh", "attestation", "verify", str(args.artifact), "--repo", repository,
        "--signer-workflow", workflow, "--source-ref", f"refs/tags/{args.tag}",
        "--format", "json", "--jq",
        ".[].verificationResult.signature.certificate.sourceRepositoryIdentifier",
    ]
    with tempfile.TemporaryDirectory() as temporary:
        config = Path(temporary)
        env["GH_CONFIG_DIR"] = temporary
        if args.app:
            token = os.environ.get("GH_TOKEN")
            if not token:
                refuse("the configured GitHub App did not supply an installation token")
            env["GH_TOKEN"] = token
        else:
            artifact_digest = digest(args.artifact)
            url = f"https://api.github.com/repos/{repository}/attestations/sha256:{artifact_digest}?per_page=100&predicate_type=provenance"
            request = urllib.request.Request(url, headers={
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
                "User-Agent": "Bilibi release verifier",
            })
            try:
                with urllib.request.urlopen(request, timeout=30) as response:
                    raw = response.read(8 * 1024 * 1024 + 1)
                if len(raw) > 8 * 1024 * 1024:
                    refuse("the public GitHub attestation response is too large")
                rows = json.loads(raw)["attestations"]
                bundles = [row["bundle"] for row in rows if isinstance(row.get("bundle"), dict)]
            except (OSError, ValueError, KeyError, TypeError, AttributeError):
                refuse(f"could not fetch public attestations for {repository}; configure the GitHub App for private repositories")
            if not bundles:
                refuse(f"GitHub returned no public attestation bundles for {repository} and this artifact")
            bundle = config / "bundles.jsonl"
            bundle.write_text("".join(json.dumps(row) + "\n" for row in bundles), encoding="utf-8")
            roots = config / "trusted-root.jsonl"
            with roots.open("wb") as output:
                subprocess.run(["gh", "attestation", "trusted-root"], env=env, stdout=output, check=True)
            verify.extend(["--bundle", str(bundle), "--custom-trusted-root", str(roots)])
        verified = subprocess.run(verify, env=env, stdout=subprocess.PIPE, check=True, text=True)
    ids = verified.stdout.split()
    if not ids or any(identifier != str(args.repository_id) for identifier in ids):
        refuse("the verified attestation does not identify the pinned GitHub repository number")


def write_instance(args: argparse.Namespace) -> None:
    if os.geteuid() != 0 or not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,31}", args.instance):
        refuse("instance trust can only be written by the privileged installer")
    os.environ["PATH"] = "/usr/sbin:/usr/bin:/sbin:/bin"
    trust_dir = Path("/opt/bilibi") / args.instance / "trust"
    for directory in (trust_dir.parent, trust_dir):
        info = directory.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
            refuse("the instance trust path is not protected")
    trust = trust_dir / "release.json"
    state = trust_dir / "release-state.json"
    for path in (trust, state):
        if path == state and not path.exists() and not path.is_symlink():
            continue
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
            refuse("the instance trust file is not protected")
    load_trust(trust)
    raw = sys.stdin.buffer.read(MAX_PROOF_BYTES + 1)
    if len(raw) > MAX_PROOF_BYTES:
        refuse("the instance trust request is too large")
    request = json.loads(raw, object_pairs_hook=strict_object)
    if not isinstance(request, dict):
        refuse("the instance trust request must be an object")
    if request.get("operation") == "admit":
        source = request["source"]
        check_admission(argparse.Namespace(trust=trust, source=json.dumps(source)))
        replay = argparse.Namespace(state=state, tag=source["tag"],
                                    digest=source["digest"], resume=False)
        check_replay(replay)
        record_replay(replay)
    elif request.get("operation") == "replace":
        actor = request["actor"]
        if type(actor) is not int or actor <= 0:
            refuse("the release trust request has no account")
        policy = request["policy"]
        exact_fields(policy, {"kind", "repository", "repository_id", "workflow", "minimum_tag", "public_key"}, "release trust request")
        if policy["kind"] not in ("github", "operator") or type(policy["repository_id"]) is not int:
            refuse("the release trust request has an invalid proof method or repository number")
        with tempfile.TemporaryDirectory(dir=trust_dir) as temporary:
            public_key = None
            if policy["kind"] == "operator":
                public_key = Path(temporary) / "public.pem"
                public_key.write_text(policy["public_key"], encoding="ascii")
            elif policy["public_key"] is not None:
                refuse("GitHub release trust does not take a public key")
            make_trust(argparse.Namespace(
                kind=policy["kind"], repository=policy["repository"], repository_id=policy["repository_id"],
                workflow=policy["workflow"], minimum_tag=policy["minimum_tag"], changed_by=f"account:{actor}",
                existing_releases=None, public_key=public_key, output=trust,
            ))
    else:
        refuse("the instance trust request has an unknown operation")


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)

    trusted = commands.add_parser("make-trust")
    trusted.add_argument("--kind", choices=("github", "operator"), required=True)
    trusted.add_argument("--repository", required=True)
    trusted.add_argument("--repository-id", type=int, required=True)
    trusted.add_argument("--workflow", required=True)
    trusted.add_argument("--minimum-tag", required=True)
    trusted.add_argument("--existing-releases", type=Path)
    trusted.add_argument("--changed-by", required=True)
    trusted.add_argument("--public-key", type=Path)
    trusted.add_argument("--output", type=Path, required=True)
    trusted.set_defaults(run=make_trust)

    signed = commands.add_parser("sign")
    signed.add_argument("--artifact", type=Path, required=True)
    signed.add_argument("--private-key", type=Path, required=True)
    signed.add_argument("--repository", required=True)
    signed.add_argument("--repository-id", type=int, required=True)
    signed.add_argument("--workflow", required=True)
    signed.add_argument("--tag", required=True)
    signed.add_argument("--kind", choices=KINDS, required=True)
    signed.add_argument("--target", required=True)
    signed.add_argument("--output", type=Path, required=True)
    signed.set_defaults(run=sign)

    verified = commands.add_parser("verify-operator")
    verified.add_argument("--artifact", type=Path, required=True)
    verified.add_argument("--proof", type=Path, required=True)
    verified.add_argument("--trust", type=Path, required=True)
    verified.add_argument("--tag", required=True)
    verified.add_argument("--kind", choices=KINDS, required=True)
    verified.add_argument("--target", required=True)
    verified.set_defaults(run=verify_operator)

    shown = commands.add_parser("trust-values")
    shown.add_argument("--trust", type=Path, required=True)
    shown.set_defaults(run=trust_values)

    checked_tag = commands.add_parser("check-tag")
    checked_tag.add_argument("--trust", type=Path, required=True)
    checked_tag.add_argument("--tag", required=True)
    checked_tag.set_defaults(run=check_tag)

    admission = commands.add_parser("check-admission")
    admission.add_argument("--trust", type=Path, required=True)
    admission.add_argument("--source", required=True)
    admission.set_defaults(run=check_admission)

    replayed = commands.add_parser("check-replay")
    replayed.add_argument("--state", type=Path, required=True)
    replayed.add_argument("--tag", required=True)
    replayed.add_argument("--digest", required=True)
    replayed.add_argument("--resume", action="store_true")
    replayed.set_defaults(run=check_replay)

    recorded = commands.add_parser("record-replay")
    recorded.add_argument("--state", type=Path, required=True)
    recorded.add_argument("--tag", required=True)
    recorded.add_argument("--digest", required=True)
    recorded.set_defaults(run=record_replay)
    github = commands.add_parser("verify-github")
    github.add_argument("--artifact", type=Path, required=True)
    github.add_argument("--repository", required=True)
    github.add_argument("--repository-id", type=int, required=True)
    github.add_argument("--workflow", required=True)
    github.add_argument("--tag", required=True)
    github.add_argument("--app", action="store_true")
    github.set_defaults(run=verify_github)

    instance = commands.add_parser("write-instance")
    instance.add_argument("instance")
    instance.set_defaults(run=write_instance)
    return root


def main() -> int:
    args = parser().parse_args()
    if getattr(args, "repository_id", 1) <= 0:
        refuse("the repository ID must be a positive number")
    args.run(args)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (Refusal, OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        print(f"release-proof: {error}", file=sys.stderr)
        raise SystemExit(1)
