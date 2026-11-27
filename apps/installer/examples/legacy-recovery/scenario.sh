set -euo pipefail

installer=$1
shift
root_dir=$(mktemp -d "$PWD/.legacy-recovery.XXXXXX")
cleanup() {
    if [[ -n ${health_pid:-} ]]; then kill "$health_pid"; fi
    rm -rf -- "$root_dir"
}
trap cleanup EXIT
mkdir -p "$root_dir"/{bin,archive,worker/dev/releases/{1,2},shell/dev/releases/1}
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
(root / "port").write_text(f"{server.server_port}\n")
server.serve_forever()
PY
health_pid=$!
for attempt in {1..50}; do
    [[ ! -s $root_dir/port ]] || break
    sleep 0.1
done
health_port=$(cat "$root_dir/port")
origin="http://127.0.0.1:$health_port"
for instance in "$root_dir"/{worker,shell}/dev; do
    printf 'BIND_ADDR=127.0.0.1:%s\nAPP_ORIGIN=%s\n' "$health_port" "$origin" > "$instance/env"
done
sql() { psql -XAt -v ON_ERROR_STOP=1 "$DATABASE_URL" "$@"; }
[[ $(sql -c "SELECT to_regclass('public.base_software_jobs') IS NULL") == t ]]
migrations=apps/base/software/migrations
sql -f "$migrations/001-jobs.sql" -f "$migrations/005-paused.sql" \
    -f "$migrations/006-installer-heartbeat.sql" \
    -f apps/base/settings/migrations/001-settings.sql >/dev/null
sql >/dev/null <<'SQL'
INSERT INTO base_settings (key, tenant_id, scope_type, scope_id, value, is_encrypted)
VALUES ('installation.mode', 0, 'global', 0, '"isolated"', false);
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
timeout 30 "$installer" --worker "$root_dir/worker/dev"
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
    read_installation_mode() { installation_mode=isolated; }
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

mkdir "$root_dir/shell/dev/releases/3" "$root_dir/worker/dev/releases/3"
cat > "$root_dir/shell/dev/releases/3/bilibi-web" <<'STUB'
#!/usr/bin/env bash
case $1 in
    status) printf '%s\n' 'Bilibi 0.3.0' 'Domains: none mounted' 'Extensions:' '  local unpinned, code: customer, modules: custom';;
    migrate) echo "customer migration must not run"; exit 2;;
    login) echo customer-release;;
    *) exit 2;;
esac
STUB
chmod +x "$root_dir/shell/dev/releases/3/bilibi-web"
cp "$root_dir/shell/dev/releases/3/bilibi-web" "$root_dir/worker/dev/releases/3/"
cat > "$root_dir/shared" <<'STUB'
#!/usr/bin/env bash
case $1 in
    status) printf '%s\n' 'Bilibi 0.4.0' 'Domains: none mounted' 'Extensions: none mounted';;
    login) echo shared-release;;
    *) exit 2;;
esac
STUB
chmod +x "$root_dir/shared"
for instance in "$root_dir"/{worker,shell}/dev; do
    mkdir "$instance/releases/4" "$instance/releases/5"
    cp "$root_dir/shared" "$instance/releases/4/bilibi-web"
    sed 's/ unpinned, modules:/ unpinned, code:reviewed, modules:/' "$root_dir/legacy" > "$instance/releases/5/bilibi-web"
    chmod +x "$instance/releases/5/bilibi-web"
    ln -sfn 4 "$instance/releases/current"
done
: > "$FIXTURE_EVENTS"
sql -c "UPDATE base_settings SET value='\"shared\"' WHERE key='installation.mode'" >/dev/null
systemctl start bilibi@dev
for target in 1 3 5; do
    refusal='requires an isolated installation'
    if [[ $target == 5 ]]; then refusal='must declare reviewed or customer code'; fi
    (cd "$root_dir/shell/dev/releases/$target" && sha256sum bilibi-web > SHA256SUMS)
    if bash -s -- "$root_dir" "$target" >"$root_dir/switch.log" 2>&1 <<'SHELL'
    root_dir=$1
    source scripts/install-server.sh
    root=$root_dir/shell
    need_root() { :; }
    lock_release_policy() { :; }
    lock_installation() { :; }
    read_installation_mode() { installation_mode=shared; }
    runuser() { shift 3; "$@"; }
    switch dev "$2"
SHELL
    then
        cat "$root_dir/switch.log"
        exit 1
    fi
    grep -Fq "$refusal" "$root_dir/switch.log"
    [[ -z $(grep -Fx "$root_dir/shell/dev/releases/$target" "$FIXTURE_EVENTS" || true) ]]
    [[ $(readlink -f "$FIXTURE_CURRENT") == "$root_dir/shell/dev/releases/4" ]]
    [[ $(curl -fsS "$origin/login") == shared-release ]]
done

export FIXTURE_CURRENT=$root_dir/worker/dev/current
systemctl start bilibi@dev
for target in 1 3 5; do
    refusal='requires an isolated installation'
    if [[ $target == 5 ]]; then refusal='must declare reviewed or customer code'; fi
    job=$(sql -c "INSERT INTO base_software_jobs (tenant_id,kind,request,review,requested_by)
        VALUES (1,'install','{}','{}',1) RETURNING id" -q)
    sql >/dev/null <<SQL
INSERT INTO base_software_steps (tenant_id,job_id,seq,key,label,state,detail) VALUES
  (1,$job,1,'fetch','Fetch','done','{"release":$target,"previous":4}'),
  (1,$job,2,'backup','Backup','pending',NULL),
  (1,$job,3,'migrate','Migrate','pending',NULL);
SQL
    timeout 30 "$installer" --worker "$root_dir/worker/dev"
    [[ $(sql -c "SELECT state FROM base_software_jobs WHERE id=$job AND result->>'message' LIKE '%$refusal%'") == failed ]]
    [[ -z $(grep -Fx "$root_dir/worker/dev/releases/$target" "$FIXTURE_EVENTS" || true) ]]
    [[ $(readlink -f "$FIXTURE_CURRENT") == "$root_dir/worker/dev/releases/4" ]]
    [[ $(curl -fsS "$origin/login") == shared-release ]]
done

echo "PASS: both installers recover legacy code in Isolated mode and refuse legacy and explicit customer code in Shared mode."
