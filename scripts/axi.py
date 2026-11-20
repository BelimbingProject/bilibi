"""Bilibi's remote AXI client. Business actions go through /api/v1."""
import argparse
from decimal import Decimal
import json
import re
import sys
import time
from urllib.error import HTTPError
from urllib.parse import quote, urlencode, urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

DEVICE_GRANT = "urn:ietf:params:oauth:grant-type:device_code"


class Refused(Exception):
    def __init__(self, status, body):
        self.status = status
        self.body = body


class NoRedirect(HTTPRedirectHandler):
    # A redirect must never forward an OAuth code or bearer to another origin.
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def origin(value):
    parts = urlsplit(value)
    loopback = parts.hostname in ("localhost", "127.0.0.1", "::1")
    if (
        not parts.hostname or parts.username is not None
        or parts.path not in ("", "/") or parts.query or parts.fragment
        or not (parts.scheme == "https" or (parts.scheme == "http" and loopback))
    ):
        raise argparse.ArgumentTypeError("use an HTTPS origin or an HTTP loopback origin")
    return value.rstrip("/")


def request(server, path, data=None, token=None):
    headers = {"Accept": "application/json", "User-Agent": "Bilibi AXI"}
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    if data is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(data).encode()
    try:
        with build_opener(NoRedirect()).open(
            Request(server + path, data=data, headers=headers), timeout=15
        ) as response:
            return json.load(response, parse_float=Decimal)
    except HTTPError as error:
        with error:
            try:
                body = json.load(error, parse_float=Decimal)
            except (ValueError, UnicodeError):
                body = {"error": "invalid_response"}
            raise Refused(error.code, body) from None


def authorize(server):
    metadata = request(server, "/.well-known/bilibi")
    client = metadata["client_id"]
    grant = request(server, "/oauth/device_authorization", {
        "client_id": client,
        "agent_id": "Bilibi AXI",
    })
    print(
        "Approve Bilibi AXI at " + json.dumps(grant["verification_uri"])
        + " with code " + json.dumps(grant["user_code"]),
        file=sys.stderr, flush=True,
    )
    deadline = time.monotonic() + grant["expires_in"]
    interval = max(1, grant["interval"])
    while time.monotonic() + interval < deadline:
        time.sleep(interval)
        try:
            response = request(server, "/oauth/token", {
                "client_id": client,
                "grant_type": DEVICE_GRANT,
                "device_code": grant["device_code"],
            })
            return response["access_token"]
        except Refused as error:
            code = error.body.get("error") if isinstance(error.body, dict) else None
            if code == "slow_down":
                interval += 5
            elif code != "authorization_pending":
                raise
    raise Refused(400, {"error": "expired_token"})


def scalar(value):
    if isinstance(value, Decimal):
        return format(value, "f")
    return json.dumps(value, ensure_ascii=False)


def toon(value, depth=0, prefix="", field=False):
    """Render TOON with inline primitive arrays and expanded structured arrays."""
    indent = "  " * depth
    if isinstance(value, list):
        header = f"{indent}{prefix}[{len(value)}]:"
        if all(not isinstance(item, (list, dict)) for item in value):
            inline = ",".join(map(scalar, value))
            return f"{header} {inline}" if inline else header
        return "\n".join([header, *(toon(item, depth + 1, "- ") for item in value)])
    if isinstance(value, dict):
        fields = [
            (key if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_.]*", key) else scalar(key), item)
            for key, item in value.items()
        ]
        if prefix == "- ":
            return "\n".join(
                toon(item, depth + (index > 0), ("- " if index == 0 else "") + key, True)
                for index, (key, item) in enumerate(fields)
            ) or indent + "-"
        lines = [indent + prefix + ":"] if field else []
        lines.extend(toon(item, depth + field, key, True) for key, item in fields)
        return "\n".join(lines)
    return indent + prefix + (": " if field else "") + scalar(value)


def json_argument(value):
    try:
        return json.loads(value)
    except ValueError:
        raise argparse.ArgumentTypeError("expected JSON") from None


def main():
    parser = argparse.ArgumentParser(prog="./bilibi axi", description="Call Bilibi with remote device authorization")
    parser.add_argument("--origin", required=True, type=origin)
    parser.add_argument("--json", action="store_true", help="preserve the API's JSON output")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("hello", help="verify the connection").add_argument("message", nargs="?")
    commands.add_parser("search", help="find available operations").add_argument("query")
    commands.add_parser("describe", help="inspect an operation").add_argument("key")
    read = commands.add_parser("read", help="call a registry read")
    read.add_argument("key")
    read.add_argument("input", type=json_argument, nargs="?", default={})
    for command in ("plan", "commit"):
        action = commands.add_parser(command, help=f"{command.capitalize()} an ordered list of operations")
        action.add_argument("operations", type=json_argument)
        if command == "commit":
            action.add_argument("--step-up-stdin", action="store_true", help="read the step-up code from stdin")
    args = parser.parse_args()

    path = "/api/v1/"
    data = None
    if args.command == "hello":
        path += "hello"
        if args.message is not None:
            path += "?" + urlencode({"message": args.message})
    elif args.command == "search":
        path += "search?" + urlencode({"q": args.query})
    elif args.command in ("describe", "read"):
        if not re.fullmatch(r"[a-zA-Z0-9_]+(?:[.-][a-zA-Z0-9_]+)*", args.key):
            parser.error("invalid operation key")
        path += "operations/" + quote(args.key, safe="")
        if args.command == "read":
            path += "/call"
            data = {"input": args.input}
    else:
        path += "plans" if args.command == "plan" else "plans/commit"
        data = {"operations": args.operations}

    try:
        token = authorize(args.origin)
    except Refused as error:
        code = error.body.get("error") if isinstance(error.body, dict) else None
        known = {"access_denied", "expired_token", "invalid_client", "invalid_request",
                 "slow_down", "temporarily_unavailable"}
        print(f"Device authorization failed ({code if isinstance(code, str) and code in known else error.status}).",
              file=sys.stderr)
        return 1
    except (OSError, ValueError, KeyError, TypeError):
        print("Could not complete device authorization.", file=sys.stderr)
        return 1

    if args.command == "commit" and args.step_up_stdin:
        data["step_up_code"] = sys.stdin.readline().strip()
    try:
        answer = request(args.origin, path, data, token)
        status = 0
    except Refused as error:
        answer, status = error.body, 1
    except (OSError, ValueError):
        print("Could not read the operation response.", file=sys.stderr)
        return 1
    if args.json:
        # Keep exact JSON numbers, including decimal values from the API.
        print(json_output(answer))
    else:
        print(toon(answer))
    return status


def json_output(value):
    if isinstance(value, dict):
        return "{" + ",".join(f"{scalar(key)}:{json_output(item)}" for key, item in value.items()) + "}"
    if isinstance(value, list):
        return "[" + ",".join(map(json_output, value)) + "]"
    return scalar(value)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
