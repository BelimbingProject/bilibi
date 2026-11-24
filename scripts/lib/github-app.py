#!/usr/bin/env python3
"""The server's GitHub App: read a private application repository without a token that expires.

install-server.sh calls this; docs/server-install.md ("Private application
repositories") explains it for people.

  github-app.py save <file> --client-id <id> --repo <owner/name>   private key on standard input
  github-app.py check <file>
  github-app.py show <file>
  github-app.py run <file> --repo <owner/name> [--log <file>] -- <command...>

The file holds the App's client ID, the repository it was put here for and its
private key (root:bilibi, 0640, like an instance's env file, because the
Software page mints from it too). Each use signs a JSON Web Token with the key,
asks GitHub for an installation token that can only read that one repository
(Contents and Attestations, read-only), and hands it to one command as
GH_TOKEN. The token lives in memory only and expires within the hour; nothing
here prints, logs or writes it.
"""
import base64
import grp
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request

API = 'https://api.github.com'
# Read-only and nothing else: release files and the build provenance of each.
PERMISSIONS = {'contents': 'read', 'attestations': 'read'}
CLIENT_ID = re.compile(r'^(Iv[0-9A-Za-z.]{1,40}|[1-9][0-9]{0,15})$')
REPOSITORY = re.compile(r'^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$')
KEY_BYTES = 16 * 1024
GUIDE = 'docs/server-install.md#private-application-repositories'


class Refusal(Exception):
    """A plain sentence that says what is wrong and what to do."""


class NotInstalled(Refusal):
    """The App exists and its key works, but it is not installed on the repository."""


class NoRedirects(urllib.request.HTTPRedirectHandler):
    # A redirect would carry the Authorization header to another address.
    def redirect_request(self, *args, **kwargs):
        return None


OPENER = urllib.request.build_opener(NoRedirects)


def b64(data):
    return base64.urlsafe_b64encode(data).rstrip(b'=').decode()


def with_key_fd(key, run):
    """Run `run(path)` with the private key readable at /dev/fd/<n>, never on disk or a command line."""
    read, write = os.pipe()
    try:
        os.write(write, key.encode())
        os.close(write)
        write = -1
        return run(f'/dev/fd/{read}', read)
    finally:
        os.close(read)
        if write >= 0:
            os.close(write)


def json_web_token(client_id, key):
    now = int(time.time())
    # GitHub allows at most ten minutes; a minute back covers a clock a little ahead.
    claims = {'iat': now - 60, 'exp': now + 540, 'iss': client_id}
    signing = f'{b64(json.dumps({"alg": "RS256", "typ": "JWT"}).encode())}.{b64(json.dumps(claims).encode())}'

    def sign(path, fd):
        return subprocess.run(['openssl', 'dgst', '-sha256', '-sign', path], input=signing.encode(),
                              pass_fds=(fd,), capture_output=True)
    signed = with_key_fd(key, sign)
    if signed.returncode != 0 or not signed.stdout:
        raise Refusal('The GitHub App private key on this server cannot sign. Put a new key here with '
                      f'install-server.sh github-app ({GUIDE}).')
    return f'{signing}.{b64(signed.stdout)}'


def call(method, path, credential, body=None):
    request = urllib.request.Request(API + path, method=method, headers={
        'Accept': 'application/vnd.github+json',
        'Authorization': f'Bearer {credential}',
        'X-GitHub-Api-Version': '2022-11-28',
        'User-Agent': 'bilibi-install-server',
    }, data=None if body is None else json.dumps(body).encode())
    try:
        with OPENER.open(request, timeout=30) as reply:
            return reply.status, json.load(reply)
    except urllib.error.HTTPError as error:
        try:
            return error.code, json.load(error)
        except ValueError:
            return error.code, {}
    except (urllib.error.URLError, OSError, ValueError) as error:
        reason = getattr(error, 'reason', error)
        raise Refusal(f'GitHub could not be reached ({reason}). Check this server can open https://api.github.com, then retry.') from None


def installation(app, repository):
    """The App's installation on the repository, refused unless it can only read."""
    status, found = call('GET', f'/repos/{repository}/installation', json_web_token(app['client_id'], app['private_key']))
    if status == 401:
        raise Refusal(f'GitHub refused the private key of the GitHub App {app["client_id"]}: the App or this key was '
                      f'deleted. Generate a new key on the App settings page and put it here with install-server.sh github-app ({GUIDE}).')
    if status == 404:
        raise NotInstalled(f'The GitHub App {app["client_id"]} is not installed on {repository}. Install it on that '
                           'repository only ("Only select repositories"), then retry.')
    if status != 200 or not isinstance(found.get('id'), int):
        raise Refusal(f'GitHub answered {status} when asked for the App installation on {repository}. Retry later.')
    granted = found.get('permissions') or {}
    writes = sorted(name for name, level in granted.items() if level != 'read')
    if writes:
        raise Refusal(f'The GitHub App {app["client_id"]} can change {", ".join(writes)} of {repository}. A server needs '
                      'it to read only: on the App settings page, set every permission except Contents and '
                      'Attestations to No access and those two to Read-only, then accept the change on the install page.')
    missing = sorted(name for name in PERMISSIONS if granted.get(name) != 'read')
    if missing:
        raise Refusal(f'The GitHub App {app["client_id"]} cannot read {" and ".join(missing)} of {repository}. On the App '
                      'settings page, set Contents and Attestations to Read-only, then accept the change on the install page.')
    if found.get('repository_selection') != 'selected':
        raise Refusal(f'The GitHub App {app["client_id"]} is installed on every repository of {repository.split("/")[0]}. '
                      f'On its install page choose "Only select repositories" and select only {repository}.')
    return found


def mint(app, repository):
    """A fresh installation token that can only read `repository`."""
    found = installation(app, repository)
    status, minted = call('POST', f'/app/installations/{found["id"]}/access_tokens',
                          json_web_token(app['client_id'], app['private_key']),
                          {'repositories': [repository.split('/')[1]], 'permissions': PERMISSIONS})
    token = minted.get('token')
    if status != 201 or not isinstance(token, str) or not token:
        raise Refusal(f'GitHub did not give the App a read token for {repository} (it answered {status}). Retry later.')
    return token


def load(path):
    try:
        with open(path, encoding='utf-8') as source:
            app = json.load(source)
    except FileNotFoundError:
        raise Refusal(f'No GitHub App is set up on this server ({path}). Put one here with install-server.sh github-app ({GUIDE}).') from None
    except (OSError, ValueError):
        raise Refusal(f'{path} cannot be read as a GitHub App. Put the App here again with install-server.sh github-app.') from None
    if not (isinstance(app, dict) and CLIENT_ID.match(str(app.get('client_id', '')))
            and REPOSITORY.match(str(app.get('repository', ''))) and isinstance(app.get('private_key'), str)):
        raise Refusal(f'{path} is not a complete GitHub App. Put the App here again with install-server.sh github-app.')
    return app


def can_read(app, repository):
    token = mint(app, repository)
    status, found = call('GET', f'/repos/{repository}', token)
    if status != 200 or not isinstance(found.get('id'), int):
        raise Refusal(f'The GitHub App {app["client_id"]} got a token, but GitHub answered {status} for {repository}.')
    return found['id']


def save(path, client_id, repository):
    if not CLIENT_ID.match(client_id):
        raise Refusal('--client-id must be the App client ID from its settings page, like Iv23li0a1b2c3d4e5f6g.')
    if not REPOSITORY.match(repository):
        raise Refusal('--repo must be owner/name, like your-company/my-bilibi.')
    key = sys.stdin.buffer.read(KEY_BYTES + 1)
    if len(key) > KEY_BYTES or not key.strip():
        raise Refusal('Pipe the App private key (.pem) to this command on standard input.')
    key = key.decode('ascii', 'replace').strip() + '\n'

    def check(path, fd):
        return subprocess.run(['openssl', 'pkey', '-noout', '-in', path], pass_fds=(fd,), capture_output=True)
    # The format GitHub hands out, and the one the Software page reads (PKCS #1 RSA).
    if not key.startswith('-----BEGIN RSA PRIVATE KEY-----') or with_key_fd(key, check).returncode != 0:
        raise Refusal('Standard input is not a GitHub App private key. Pipe the .pem file GitHub gave for the App.')
    app = {'client_id': client_id, 'repository': repository, 'private_key': key}
    note = ''
    try:
        number = can_read(app, repository)
        note = f'It can read {repository} (repository {number}) and cannot change anything.'
    except NotInstalled as missing:
        note = f'{missing} Then check it with install-server.sh github-app --check.'
    group = grp.getgrnam('bilibi').gr_gid
    partial = f'{path}.new'
    descriptor = os.open(partial, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, 'w', encoding='utf-8') as out:
        os.fchown(out.fileno(), 0, group)
        os.fchmod(out.fileno(), 0o640)
        json.dump(app, out)
        out.flush()
        os.fsync(out.fileno())
    os.replace(partial, path)
    print(f'GitHub App {client_id} saved in {path} for {repository}. {note}')


def run(path, repository, command, log):
    """Run `command` with GH_TOKEN for `repository`; its error output goes to `log`, its output stays its own.

    Without an App installed there (an official public repository), it runs without a login."""
    env = {name: value for name, value in os.environ.items() if name not in ('GH_TOKEN', 'GITHUB_TOKEN')}
    try:
        env['GH_TOKEN'] = mint(load(path), repository)
    except NotInstalled:
        print(f'The GitHub App on this server is not installed on {repository}, so GitHub is asked without a login.',
              file=sys.stderr)
    with open(log or os.devnull, 'a', encoding='utf-8') as errors:
        return subprocess.run(command, env=env, stderr=errors).returncode


def main(argv):
    if len(argv) < 2:
        sys.exit(__doc__)
    action, path, rest = argv[0], argv[1], argv[2:]
    options, command = {}, []
    while rest:
        if rest[0] == '--':
            command = rest[1:]
            break
        if rest[0] not in ('--client-id', '--repo', '--log') or len(rest) < 2:
            sys.exit(__doc__)
        options[rest[0]], rest = rest[1], rest[2:]
    try:
        if action == 'save' and '--client-id' in options and '--repo' in options:
            save(path, options['--client-id'], options['--repo'])
        elif action == 'check':
            app = load(path)
            number = can_read(app, app['repository'])
            print(f'GitHub App {app["client_id"]} works: it can read {app["repository"]} (repository {number}) and cannot change anything.')
        elif action == 'show':
            if os.path.exists(path):
                app = load(path)
                print(f'GitHub App {app["client_id"]} for {app["repository"]} ({path})')
            else:
                print('No GitHub App on this server; one is needed only for a private application repository.')
        elif action == 'run' and '--repo' in options and command:
            return run(path, options['--repo'], command, options.get('--log'))
        else:
            sys.exit(__doc__)
    except Refusal as refusal:
        print(f'install-server: {refusal}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
