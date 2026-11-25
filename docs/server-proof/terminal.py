#!/usr/bin/env python3
"""Drive one interactive server command without disclosing its 2FA code."""
import errno
import os
from pathlib import Path
import pty
import select
import subprocess
import sys
import time

if len(sys.argv) < 4:
    raise SystemExit('usage: terminal.py <private-code-file> <command> <arg>...')

secret_file = Path(sys.argv[1]).resolve()
if secret_file.parent != Path('/data/private'):
    raise SystemExit('the code file must be directly under /data/private')
secret = secret_file.read_text().strip().encode()
if not secret or len(secret) > 256:
    raise SystemExit('the private code file is empty or invalid')

master, slave = pty.openpty()
child = subprocess.Popen(sys.argv[2:], stdin=slave, stdout=slave, stderr=slave, close_fds=True)
os.close(slave)
prompt = b'Current 2FA code:'
pending = b''
sent = False
deadline = time.monotonic() + 600
try:
    while True:
        if time.monotonic() >= deadline:
            child.terminate()
            raise SystemExit('terminal command did not finish within ten minutes')
        ready, _, _ = select.select([master], [], [], 0.25)
        if ready:
            try:
                chunk = os.read(master, 8192)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                chunk = b''
            if not chunk:
                break
            pending += chunk
            if not sent and prompt in pending:
                os.write(master, secret + b'\n')
                sent = True
            if secret in pending:
                pending = pending.replace(secret, b'[redacted]')
            keep = max(len(prompt), len(secret))
            if len(pending) > keep:
                os.write(sys.stdout.fileno(), pending[:-keep])
                pending = pending[-keep:]
    if pending:
        os.write(sys.stdout.fileno(), pending.replace(secret, b'[redacted]'))
    status = child.wait(timeout=max(0, deadline - time.monotonic()))
finally:
    os.close(master)
    secret_file.unlink(missing_ok=True)

if not sent:
    raise SystemExit('the command never asked for a 2FA code')
raise SystemExit(status)
