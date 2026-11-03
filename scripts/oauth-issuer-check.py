#!/usr/bin/env python3
"""Check a running Bilibi's OAuth issuer; no unit-test runner.

Fetches the authorization-server and protected-resource metadata from ORIGIN
(the exact `APP_ORIGIN`) and fails unless the issuer equals ORIGIN, equals the
protected resource's authorization server, and the server advertises the `iss`
response parameter. Pass `--callback-url` (the redirect an authorization
response sent to the client, without its `code`) to also require the matching
`iss`. Prints no tokens.
"""
import argparse
import json
import sys
import urllib.parse
import urllib.request


def fetch(url):
    with urllib.request.urlopen(url, timeout=10) as response:
        return json.load(response)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('origin', help='The exact APP_ORIGIN, such as https://bilibi.localhost')
    parser.add_argument('--callback-url', help='A real authorization response redirect to check for the matching iss.')
    args = parser.parse_args()
    origin = args.origin.rstrip('/')
    server = fetch(f'{origin}/.well-known/oauth-authorization-server')
    resource = fetch(f'{origin}/.well-known/oauth-protected-resource/mcp')
    failures = []
    if server.get('issuer') != origin:
        failures.append(f"issuer {server.get('issuer')!r} is not {origin!r}")
    if resource.get('authorization_servers') != [server.get('issuer')]:
        failures.append(f"protected resource names {resource.get('authorization_servers')!r}, not the issuer")
    if server.get('authorization_response_iss_parameter_supported') is not True:
        failures.append('iss response parameter is not advertised')
    if args.callback_url:
        query = urllib.parse.parse_qs(urllib.parse.urlparse(args.callback_url).query)
        if query.get('iss') != [server.get('issuer')]:
            failures.append(f"callback iss {query.get('iss')!r} does not match the issuer")
    for failure in failures:
        print(f'FAIL {failure}', file=sys.stderr)
    if failures:
        sys.exit(1)
    print('OK issuer, protected-resource metadata and iss agree')


if __name__ == '__main__':
    main()
