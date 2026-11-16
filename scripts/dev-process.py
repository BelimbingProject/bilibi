#!/usr/bin/env python3
"""Own the dev app outside cargo-watch's replaceable rebuild process group.

start-app.sh runs `watch`; dev-run.sh requests `running`, `stop` and `start`;
./bilibi stop runs `down`. Only this owner signals the children it created,
never a stale PID from disk. The owner is the launcher's small parent: on a
closed terminal (SIGHUP), SIGTERM or SIGINT it stops cargo-watch and the app,
and everything in their sessions, before it exits. Children write to a pipe or
pty of the owner's, which forwards to the real output and ignores a failed
write, so a closed terminal or a dead `tee` can never make cargo-watch abort
on a broken pipe. This is Bilibi launcher plumbing.
"""
import errno
import fcntl
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import pty
import signal
import socket
import subprocess
import sys
import termios
import threading
import time


spec = importlib.util.spec_from_file_location('dev_status', Path(__file__).with_name('dev-status.py'))
status = importlib.util.module_from_spec(spec)
spec.loader.exec_module(status)


class StatusHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        # No CORS, arbitrary file serving or environment/runtime log endpoint.
        # Reject DNS rebinding; Caddy sets this loopback Host on its proxy hop.
        if self.headers.get('Host') != f'127.0.0.1:{self.server.server_port}':
            self.send_error(403)
            return
        if self.path != '/__bilibi_dev/status':
            self.send_error(404)
            return
        current = status.read()
        current['elapsed'] = (current.get('finished_at') or time.time()) - current['started_at']
        body = json.dumps(current).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Cache-Control', 'no-store')
        self.send_header('X-Content-Type-Options', 'nosniff')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, _format, *_args):
        pass


DEV = Path(".local/dev")
SOCKET = DEV / "control.sock"
PID = DEV / "app.pid"
LOCK = DEV / "owner.lock"
OWNER_PID = DEV / "owner.pid"
# start-app.sh prepares these after the owner starts. Transfer only these values
# over the private control socket; never persist credentials or log the payload.
APP_ENVIRONMENT = {'DATABASE_URL', 'APP_ORIGIN', 'BIND_ADDR'}


def reply(connection, result):
    try:
        connection.sendall(str(result).encode())
    except (BrokenPipeError, ConnectionResetError):
        pass  # cargo-watch replaced the rebuild that made this request.
    finally:
        connection.close()


def say(message, **options):
    try:
        print(message, flush=True, **options)
    except OSError:
        pass  # The terminal is gone; the launcher still has to stop its children.


def forward(reader, destination):
    """Copy a child's output to ours. Once ours fails, keep draining the child."""
    alive = True
    while True:
        try:
            chunk = os.read(reader, 8192)
        except OSError as error:
            if error.errno == errno.EIO:  # A pty reports EIO when its writer closes.
                break
            raise
        if not chunk:
            break
        remaining = memoryview(chunk)
        while alive and remaining:
            try:
                remaining = remaining[os.write(destination, remaining):]
            except OSError:
                alive = False
    os.close(reader)


def relay(destination):
    """Return a descriptor for a child's output, forwarded to descriptor `destination`.

    A terminal gets a pty of the same size, so Cargo and the app keep their
    colours and width; anything else gets a pipe.
    """
    if os.isatty(destination):
        reader, writer = pty.openpty()
        try:
            fcntl.ioctl(writer, termios.TIOCSWINSZ, fcntl.ioctl(destination, termios.TIOCGWINSZ, b'\0' * 8))
        except OSError:
            pass
        attributes = termios.tcgetattr(writer)
        attributes[1] &= ~termios.OPOST
        termios.tcsetattr(writer, termios.TCSANOW, attributes)
    else:
        reader, writer = os.pipe()
    threading.Thread(target=forward, args=(reader, destination), daemon=True).start()
    return writer


def spawn(command, **options):
    outputs = [relay(1), relay(2)]
    try:
        return subprocess.Popen(command, stdout=outputs[0], stderr=outputs[1], **options)
    finally:
        for output in outputs:
            os.close(output)


def session_members(sid):
    """Live processes in session `sid`, read from Linux /proc (none elsewhere)."""
    members = []
    try:
        entries = os.listdir('/proc')
    except OSError:
        return members
    for entry in entries:
        if not entry.isdigit():
            continue
        try:
            with open(f'/proc/{entry}/stat') as stream:
                fields = stream.read().rsplit(')', 1)[1].split()
        except (OSError, IndexError):
            continue
        if fields[0] != 'Z' and int(fields[3]) == sid:
            members.append(int(entry))
    return members


def stop(process, first=signal.SIGTERM):
    """Stop a child and every process left in its session, then wait for them."""
    if process is None:
        return
    if process.poll() is None:
        process.send_signal(first)
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
    # cargo-watch's rebuild and the app's own helpers run in the same session.
    for sending in (signal.SIGTERM, signal.SIGKILL):
        deadline = time.monotonic() + 5
        while True:
            members = session_members(process.pid)
            if not members:
                return
            if time.monotonic() >= deadline:
                break
            for member in members:
                try:
                    os.kill(member, sending)
                except ProcessLookupError:
                    pass
            time.sleep(0.1)


def watch(command):
    DEV.mkdir(parents=True, exist_ok=True)
    # A second launcher must not unlink the active owner's socket or PID record.
    with LOCK.open("w") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            sys.exit("This checkout already has a dev launcher. Stop it before starting another.")
        OWNER_PID.write_text(f"{os.getpid()}\n")
        status.write('prepare')
        (DEV / 'seed-needed').touch()
        Path('.local/dev-run.state').unlink(missing_ok=True)
        http = ThreadingHTTPServer(('127.0.0.1', 0), StatusHandler)
        http.daemon_threads = True
        threading.Thread(target=http.serve_forever, daemon=True).start()
        environment = {**os.environ, 'BILIBI_DEV_STATUS_PORT': str(http.server_port)}
        stopping = False

        def shutdown(_signum, _frame):
            nonlocal stopping
            stopping = True

        signal.signal(signal.SIGINT, shutdown)
        signal.signal(signal.SIGTERM, shutdown)
        signal.signal(signal.SIGHUP, shutdown)
        app = watcher = None
        SOCKET.unlink(missing_ok=True)
        PID.unlink(missing_ok=True)
        with socket.socket(socket.AF_UNIX) as server:
            server.bind(str(SOCKET))
            SOCKET.chmod(0o600)
            server.listen()
            server.settimeout(0.2)
            try:
                # Neither child shares the terminal/rebuild process group. The
                # launcher forwards Ctrl-C to cargo-watch and reaps its own app.
                watcher = spawn(command, start_new_session=True, env=environment)
                while not stopping and watcher.poll() is None:
                    if app is not None and app.poll() is not None:
                        result = app.returncode
                        state = Path(".local/dev-run.state")
                        if result == -signal.SIGTERM:
                            cause = "another process stopped it (SIGTERM), not a file change"
                            advice = (
                                "To start it again, save any file or run `touch Cargo.toml`, "
                                "or press Ctrl-C and run ./bilibi start again."
                            )
                        else:
                            cause = f"the app stopped (exit {128 - result if result < 0 else result})"
                            advice = (
                                "Fix the error and save a file to try again, or press Ctrl-C "
                                "and run ./bilibi start again."
                            )
                        if state.exists() and state.read_text().strip() == "serve":
                            state.write_text("ended\n")
                            status.write("serve", failed=True, finished=True, output=cause[0].upper() + cause[1:] + ".")
                        reason = f"{cause}. {advice}"
                        say(f"Bilibi is not running: {reason}")
                        app = None
                        PID.unlink(missing_ok=True)
                    try:
                        connection, _ = server.accept()
                    except socket.timeout:
                        continue
                    connection.settimeout(1)
                    try:
                        with connection.makefile("rb") as stream:
                            action = stream.readline(32).strip()
                            app_environment = json.loads(stream.readline(16384)) if action == b'start' else {}
                            if (not isinstance(app_environment, dict)
                                    or app_environment.keys() - APP_ENVIRONMENT
                                    or any(not isinstance(value, str) for value in app_environment.values())):
                                raise ValueError('Invalid app environment')
                    except (TimeoutError, ConnectionResetError, ValueError):
                        connection.close()
                        continue
                    if action == b"running":
                        reply(connection, 0 if app is not None and app.poll() is None else 1)
                    elif action == b"stop":
                        stop(app)
                        app = None
                        PID.unlink(missing_ok=True)
                        reply(connection, 0)
                    elif action == b"start" and app is None:
                        try:
                            app = spawn(
                                [str(DEV.resolve() / "bilibi-dev"), "run"],
                                start_new_session=True, stdin=subprocess.DEVNULL,
                                env={**os.environ, **app_environment},
                            )
                            PID.write_text(f"{app.pid}\n")
                            reply(connection, 0)
                        except OSError as error:
                            stop(app)
                            app = None
                            PID.unlink(missing_ok=True)
                            status.write('start', failed=True, finished=True)
                            say(f"Could not start Bilibi: {error}", file=sys.stderr)
                            reply(connection, 1)
                    else:
                        reply(connection, 1)
            finally:
                # Ask cargo-watch to stop (it stops the rebuild it runs), then the app.
                stop(watcher, signal.SIGINT)
                stop(app)
                PID.unlink(missing_ok=True)
                OWNER_PID.unlink(missing_ok=True)
                SOCKET.unlink(missing_ok=True)
                http.shutdown()
                http.server_close()
        return 130 if stopping else watcher.returncode


def down():
    """./bilibi stop: ask this checkout's owner to stop everything, and wait for it."""
    DEV.mkdir(parents=True, exist_ok=True)
    with LOCK.open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            pass
        else:
            print("Bilibi is not running from this checkout.")
            return 0
        # The owner holds the lock, so its recorded PID is current.
        try:
            os.kill(int(OWNER_PID.read_text()), signal.SIGTERM)
        except (OSError, ValueError):
            print("The dev launcher is starting or already stopping; try again in a moment.", file=sys.stderr)
            return 1
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                time.sleep(0.2)
                continue
            print("Stopped Bilibi: the launcher, cargo-watch, any build and the app have exited.")
            return 0
    print("Bilibi did not stop within 60 seconds.", file=sys.stderr)
    return 1


def request(action):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.connect(str(SOCKET))
        payload = action.encode() + b"\n"
        if action == 'start':
            payload += json.dumps({key: os.environ[key] for key in APP_ENVIRONMENT if key in os.environ}).encode() + b'\n'
        connection.sendall(payload)
        return int(connection.recv(32))


if __name__ == "__main__":
    if sys.argv[1] == "watch":
        sys.exit(watch(sys.argv[2:]))
    if sys.argv[1] == "down":
        sys.exit(down())
    sys.exit(request(sys.argv[1]))
