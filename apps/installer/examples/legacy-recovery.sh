#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../.."
installer=${1:-target/debug/bilibi-installer}
root_dir=$(mktemp -d "$PWD/.legacy-recovery.XXXXXX")
cleanup() {
    if [[ -n ${health_pid:-} ]]; then kill "$health_pid"; fi
    if [[ -f $root_dir/pg/postmaster.pid ]]; then
        pg_ctl -D "$root_dir/pg" -m immediate stop >/dev/null
    fi
    rm -rf -- "$root_dir"
}
trap cleanup EXIT
mkdir -p "$root_dir"/{bin,archive,worker/dev/releases/{1,2,3},shell/dev/releases/1}
export FIXTURE_EVENTS=$root_dir/events
export FIXTURE_CURRENT=$root_dir/worker/dev/current
export BILIBI_RELEASE_USER=fixture

cat > "$root_dir/bin/systemctl" <<'STUB'
#!/usr/bin/env bash
case $1 in
    stop) echo stopped >> "$FIXTURE_EVENTS";;
    start) readlink -f "$FIXTURE_CURRENT" >> "$FIXTURE_EVENTS";;
    is-active)
        if [[ $(tail -n1 "$FIXTURE_EVENTS") == stopped ]]; then
            echo inactive
        else
            echo active
        fi;;
    show) echo 0;;
    *) exit 2;;
esac
STUB
cat > "$root_dir/bin/sudo" <<'STUB'
#!/usr/bin/env bash
if [[ $2 == systemctl ]]; then
    shift
else
    shift 5
fi
exec "$@"
STUB
cat > "$root_dir/legacy" <<'STUB'
#!/usr/bin/env bash
case $1 in
    status) printf '%s\n' 'Bilibi 0.1.0' 'Domains: none mounted' 'Extensions:' '  legacy unpinned, modules: custom';;
    login) echo legacy-release;;
    *) exit 2;;
esac
STUB
cat > "$root_dir/archive/bilibi-web" <<'STUB'
#!/usr/bin/env bash
case $1 in
    status) printf '%s\n' 'Bilibi 0.2.0' 'Domains: none mounted' 'Extensions: none mounted';;
    migrate) echo 'fixture migration refused' >&2; exit 1;;
    *) exit 2;;
esac
STUB
chmod +x "$root_dir"/bin/* "$root_dir/legacy" "$root_dir/archive/bilibi-web"
export PATH="$root_dir/bin:$PATH"
for instance in "$root_dir"/{worker,shell}/dev; do
    cp "$root_dir/legacy" "$instance/releases/1/bilibi-web"
    ln -s releases/current "$instance/current"
    ln -s 1 "$instance/releases/current"
done
cp "$root_dir/archive/bilibi-web" "$root_dir/worker/dev/releases/2/"
(
    cd "$root_dir/archive"
    sha256sum bilibi-web > SHA256SUMS
    tar -czf "$root_dir/bilibi-setup-v0.2.0-fixture.tar.gz" bilibi-web SHA256SUMS
)
python3 - "$root_dir" <<'PY' &
import http.server
import pathlib
import socket
import subprocess
import sys

root = pathlib.Path(sys.argv[1])

class Health(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        active = (root / "events").read_text().splitlines()[-1:]
        program = pathlib.Path(active[0]) / "bilibi-web" if active else None
        if self.path != "/login" or not program or not program.is_file():
            self.send_error(503)
            return
        output = subprocess.check_output([program, "login"])
        self.send_response(200)
        self.end_headers()
        self.wfile.write(output)

server = http.server.HTTPServer(("127.0.0.1", 0), Health)
with socket.socket() as database:
    database.bind(("127.0.0.1", 0))
    (root / "ports").write_text(f"{server.server_port} {database.getsockname()[1]}\n")
server.serve_forever()
PY
health_pid=$!
for attempt in {1..50}; do
    [[ ! -s $root_dir/ports ]] || break
    sleep 0.1
done
read -r health_port pg_port < "$root_dir/ports"
origin="http://127.0.0.1:$health_port"
for instance in "$root_dir"/{worker,shell}/dev; do
    printf 'BIND_ADDR=127.0.0.1:%s\nAPP_ORIGIN=%s\n' "$health_port" "$origin" > "$instance/env"
done
initdb -D "$root_dir/pg" -A trust -U installer --no-locale >/dev/null
pg_ctl -D "$root_dir/pg" -l "$root_dir/postgres.log" \
    -o "-h 127.0.0.1 -p $pg_port -c unix_socket_directories=" -w start >/dev/null
export DATABASE_URL="postgresql://installer@127.0.0.1:$pg_port/postgres"
sql() { psql -XAt -v ON_ERROR_STOP=1 "$DATABASE_URL" "$@"; }
migrations=apps/base/software/migrations
sql -f "$migrations/001-jobs.sql" -f "$migrations/005-paused.sql" \
    -f "$migrations/006-installer-heartbeat.sql" >/dev/null
sql >/dev/null <<'SQL'
CREATE TABLE bilibi_migrations (id text);
INSERT INTO bilibi_migrations VALUES ('baseline');
INSERT INTO base_software_jobs (tenant_id,kind,request,review,requested_by)
VALUES (1,'install','{}','{}',1);
INSERT INTO base_software_steps (tenant_id,job_id,seq,key,label,state,detail) VALUES
  (1,1,1,'fetch','Fetch','done','{"release":2,"previous":1}'),
  (1,1,2,'backup','Backup','pending',NULL),
  (1,1,3,'migrate','Migrate','pending',NULL);
SQL

systemctl start bilibi@dev
timeout 30 "$installer" "$root_dir/worker/dev"
[[ $(sql -c "SELECT state FROM base_software_jobs WHERE id=1 AND result->>'migrated'='false' AND result->>'message' LIKE '%fixture migration refused%'") == failed ]]
[[ $(sql -c 'TABLE bilibi_migrations') == baseline ]]
[[ $(readlink -f "$FIXTURE_CURRENT") == "$root_dir/worker/dev/releases/1" ]]
[[ $(curl -fsS "$origin/login") == legacy-release ]]

export FIXTURE_CURRENT=$root_dir/shell/dev/current
systemctl start bilibi@dev
if bash -s -- "$root_dir" >"$root_dir/shell.log" 2>&1 <<'SHELL'
    root_dir=$1
    source scripts/install-server.sh
    root=$root_dir/shell
    staging=$root_dir/staging
    release_proof=$root_dir/provenance.py
    mkdir "$staging"
    touch "$release_proof"
    need_root() { :; }
    lock_release_policy() { :; }
    stage_source() { staged=$1; }
    verify_provenance() { :; }
    install_unit() { :; }
    ensure_instance_access() { :; }
    install_release_proof() { :; }
    retire_worker() { :; }
    lock_installation() { :; }
    read_installation_mode() { installation_mode=shared; }
    runuser() { shift 3; "$@"; }
    release dev "$root_dir/bilibi-setup-v0.2.0-fixture.tar.gz" --tag setup-v0.2.0
SHELL
then
    cat "$root_dir/shell.log"
    exit 1
fi
cat "$root_dir/shell.log"
grep -q 'fixture migration refused' "$root_dir/shell.log"
grep -q 'Code: MIGRATION_FAILED' "$root_dir/shell.log"
[[ $(readlink -f "$FIXTURE_CURRENT") == "$root_dir/shell/dev/releases/1" ]]
[[ $(curl -fsS "$origin/login") == legacy-release ]]

export FIXTURE_CURRENT=$root_dir/worker/dev/current
ln -sfn 3 "$root_dir/worker/dev/releases/current"
sql >/dev/null <<'SQL'
INSERT INTO base_software_jobs
  (tenant_id,kind,state,request,review,requested_by,worker,paused_at)
VALUES (1,'install','running','{}','{}',1,'installer@dev',now()),
       (2,'install','running','{}','{}',1,'installer@dev',now());
INSERT INTO base_software_steps (tenant_id,job_id,seq,key,label,state,detail) VALUES
  (1,2,1,'fetch','Fetch','done','{"release":2,"previous":1}'),
  (1,2,2,'migrate','Migrate','failed','{"migrated":true}'),
  (2,3,1,'fetch','Fetch','done','{"release":2,"previous":1}');
SQL
mkdir -p "$root_dir/worker/dev/backups/job-3"
printf '{"dump":"unused.dump","history":{"current_release":1}}\n' \
    > "$root_dir/worker/dev/backups/job-3/history-switch_back.json"
: > "$FIXTURE_EVENTS"
timeout 30 "$installer" "$root_dir/worker/dev"
[[ ! -s $FIXTURE_EVENTS ]]
[[ $(sql -c 'TABLE bilibi_migrations') == baseline ]]
[[ $(readlink -f "$FIXTURE_CURRENT") == "$root_dir/worker/dev/releases/3" ]]
[[ $(sql -c "SELECT count(*) FROM base_software_jobs WHERE state='interrupted'") == 2 ]]
echo 'PASS: both installers recover the answering legacy release and stale recovery leaves current alone.'
