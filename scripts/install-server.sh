#!/usr/bin/env bash
# Set up a Bilibi server: one or more instances (dev, staging, production) under
# /opt/bilibi, each with its own database, system service, address and release
# folders. Run as root on Ubuntu, Debian, Rocky Linux 10 or RHEL 10. Safe to
# rerun: it keeps what exists.
# The layout, and why it is this way, is in docs/server-install.md.
#
#   install-server.sh create <instance> --role dev|staging|production --site <staff-host> [--public-site <public-host> | --no-public-site] [--public-tenant <id>] [--staff-bind <address> | --no-staff-bind] [--public-bind <address>] [--port <n>] [--tls internal|cloudflare] [--cloudflare-token-stdin]
#   install-server.sh release <instance> <setup archive> --repo <owner/name> [--repo-id <n>] --tag <setup-vX.Y.Z>
#   install-server.sh mail <instance> --host <relay> --port <n> --username <user> --tls starttls|implicit_tls --from-address <address> [--from-name <name>] --password-stdin
#   install-server.sh create-owner <instance> --email <owner address> [--name <name>] [--company-name <name>]
#   install-server.sh tenant-provision <instance> --tenant-name <name> --company-name <name> --company-code <code> --owner-email <address>
#   install-server.sh two-factor-reset <instance> --email <address>
#   install-server.sh identity-recover <instance> --email <old> --new-email <new>
#   install-server.sh identity-delete <instance> --email <address> --confirm <address>
#   install-server.sh identity-retention <instance>
#   install-server.sh installer <bilibi-installer binary> --repo <owner/name> [--repo-id <n>] --tag <setup-vX.Y.Z>
#   install-server.sh github-app --client-id <id> --repo <owner/name>   (the App private key on standard input)
#   install-server.sh github-app --check | --remove
#   install-server.sh verify <file> --repo <owner/name> [--repo-id <n>] --tag <setup-vX.Y.Z>
#   install-server.sh switch <instance> <release number>
#   install-server.sh status [<instance>]
#   install-server.sh --json <command> ...   one JSON line per step, for an AI agent or script
#
# Only the layout, the first release and the installer service live here.
# Build releases in a separate development checkout, then install a tested release here.
# Backing up before a switch, checking the new release, switching back and Undo
# are the installer's work once a server owner asks on the Software page
# (apps/installer/README.md).
set -euo pipefail
shopt -s inherit_errexit

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=scripts/lib/versions.sh
source "$here/lib/versions.sh"
root=/opt/bilibi
unit_name='bilibi@'
installer_unit='bilibi-installer@'
installer_bin=$root/build/bin/bilibi-installer
# The GitHub App that reads a private application repository: one per server,
# shared by its instances (they serve the same application). root:bilibi 0640
# like env, because each instance's program mints from it too.
github_app=$root/github-app.json
pg=$POSTGRESQL_MAJOR
log=/var/log/bilibi-install-server.log
os_family=
pg_bin=
pg_service=

usage() {
    sed -n '2,/^set /p' "${BASH_SOURCE[0]}" | sed -n 's/^# \{0,1\}//p' >&2
    exit 2
}
# fail <message> [<what to do next>] [<CODE>]: the code is the stable name
# `--json` reports, documented in docs/install-with-ai/SKILL.md.
fail() {
    printf 'install-server: %s\n' "$1" >&2
    [[ -n ${2:-} ]] && printf 'Next: %s\n' "$2" >&2
    [[ -n ${3:-} ]] && printf 'Code: %s\n' "$3" >&2
    exit 1
}
say() { printf '%s\n' "$*"; }
need_root() { [[ $(id -u) -eq 0 ]] || fail 'run this as root' "sudo $0 $*" NOT_ROOT; }
valid_name() { [[ $1 =~ ^[a-z0-9][a-z0-9-]{0,31}$ ]]; }
valid_repository() { [[ $1 =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; }
valid_setup_tag() { [[ $1 =~ ^setup-v[0-9]+\.[0-9]+\.[0-9]+$ ]]; }
instance_dir() { printf '%s/%s' "$root" "$1"; }
instance_exists() { [[ -f $(instance_dir "$1")/env ]]; }
# env_value <instance> <KEY>: one value from the instance's env file.
env_value() { sed -n "s/^$2=//p" "$(instance_dir "$1")/env" | head -n1; }
# Installed roles live in settings. Only a database without a settings table
# still uses the install input. An unavailable database cannot announce Dev.
role_of() {
    local saved port has_settings
    port=$(postgres_port)
    if [[ -z $port ]] || ! has_settings=$(runuser -u postgres -- "$pg_bin/psql" -p "$port" -d "$(database_name "$1")" -Atq -c "SELECT to_regclass('public.base_settings') IS NOT NULL" 2>/dev/null); then
        echo unavailable
        return
    fi
    if [[ $has_settings == t ]]; then
        if ! saved=$(runuser -u postgres -- "$pg_bin/psql" -p "$port" -d "$(database_name "$1")" -Atq -c "SELECT value #>> '{}' FROM base_settings WHERE key='instance.role' AND tenant_id=0 AND scope_type='global' AND scope_id=0" 2>/dev/null); then
            echo unavailable
            return
        fi
        saved=${saved:-production}
    else
        saved=$(env_value "$1" BILIBI_ENV)
    fi
    case $saved in
        development) echo dev ;;
        staging) echo staging ;;
        production|'') echo production ;;
        *) echo unavailable ;;
    esac
}
# These three print what exists and succeed when nothing does, so a caller's
# command substitution never trips errexit on an empty server.
instances() { [[ -d $root ]] || return 0; find "$root" -mindepth 2 -maxdepth 2 -name env -printf '%h\n' | xargs -rn1 basename | sort; }
release_numbers() { { find "$(instance_dir "$1")/releases" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' 2>/dev/null || true; } | { grep -E '^[0-9]+$' || true; } | sort -n; }
# `current` is a fixed root-owned link to `releases/current`; that inner link,
# owned by bilibi-build like the rest of `releases/`, names the release in
# service, so the installer can move it without owning the instance folder.
current_release() { local target; target=$(readlink -f "$(instance_dir "$1")/current" 2>/dev/null || true); if [[ -n $target && -d $target ]]; then basename "$target"; fi; }
point_current() { # <instance> <n>
    local dir; dir=$(instance_dir "$1")
    ln -sfn "$2" "$dir/releases/current.new" && chown -h bilibi-build:bilibi "$dir/releases/current.new" && mv -T "$dir/releases/current.new" "$dir/releases/current"
}
# ensure_incoming <instance>: `incoming/` is where the program (bilibi) puts a
# release that arrives from another machine or as an uploaded file, and where
# the installer (bilibi-build) takes it from: both write, nobody else reads.
ensure_incoming() { install -d -m 2770 -o bilibi -g bilibi-build "$(instance_dir "$1")/incoming"; }
# ensure_current_link <instance>: the two-link shape above; an instance made
# before it (`current -> releases/<n>`) is converted without a pause.
ensure_current_link() {
    local dir link; dir=$(instance_dir "$1"); link=$(readlink "$dir/current" 2>/dev/null || true)
    [[ $link == releases/current ]] && return 0
    if [[ $link =~ ^releases/([0-9]+)$ ]]; then point_current "$1" "${BASH_REMATCH[1]}"; fi
    ln -sfn releases/current "$dir/current.new" && mv -T "$dir/current.new" "$dir/current"
}
service_state() { systemctl is-active "$unit_name$1" 2>/dev/null || true; }
installer_state() { systemctl is-active "$installer_unit$1" 2>/dev/null || true; }

# check_dns <site>: a public site gets its certificate from Let's Encrypt,
# which needs the name to reach this server. Refuse before installing
# anything while the name does not resolve; warn when it resolves to an
# address this machine does not hold (normal behind a cloud NAT).
check_dns() {
    local site=$1 resolved local_addresses address
    resolved=$({ getent ahosts "$site" 2>/dev/null || true; } | awk '{print $1}' | sort -u)
    [[ -n $resolved ]] || fail "DNS for $site does not resolve yet" "add an A (and AAAA, if the server has IPv6) record for $site pointing at this server's public address, wait until it resolves, then rerun the same command" DNS_NOT_FOUND
    local_addresses=$(ip -o addr show 2>/dev/null | awk '{sub(/\/.*/, "", $4); print $4}')
    while IFS= read -r address; do
        grep -qxF -- "$address" <<<"$local_addresses" && return 0
    done <<<"$resolved"
    say "Warning: $site resolves to $(tr '\n' ' ' <<<"$resolved")which is not an address of this machine. That is fine behind a provider's NAT; otherwise fix the DNS record, or the certificate cannot be issued."
}

# ---------------------------------------------------------------- packages
detect_os() {
    [[ -r /etc/os-release ]] || fail 'cannot detect this operating system: /etc/os-release is missing'
    # shellcheck disable=SC1091
    source /etc/os-release
    case " ${ID:-} ${ID_LIKE:-} " in
        *' debian '*) os_family=debian; pg_bin=/usr/bin; pg_service=postgresql ;;
        *' rocky '*|*' rhel '*|*' centos '*|*' almalinux '*)
            [[ ${VERSION_ID%%.*} == 10 ]] || fail "${PRETTY_NAME:-This RHEL-family system} is not supported" 'use Rocky Linux 10 or RHEL 10' OS_UNSUPPORTED
            os_family=rhel; pg_bin="/usr/pgsql-$pg/bin"; pg_service="postgresql-$pg"
            ;;
        *) fail "${PRETTY_NAME:-This operating system} is not supported" 'use Ubuntu, Debian, Rocky Linux 10 or RHEL 10' OS_UNSUPPORTED ;;
    esac
}
apt_installed() { dpkg-query -W -f='${Status}' "$1" 2>/dev/null | grep -q '^install ok installed$'; }
apt_candidate() { [[ -n $(apt-cache policy "$1" 2>/dev/null | awk '/Candidate:/ && $2 != "(none)" {print $2}') ]]; }
apt_updated=
# Package tool output goes to the log; the terminal gets one line per step.
apt_update() { [[ -n $apt_updated ]] || { apt-get update -qq >>"$log" 2>&1; apt_updated=1; }; }
apt_install() {
    local p missing=()
    for p in "$@"; do apt_installed "$p" || missing+=("$p"); done
    ((${#missing[@]})) || return 0
    say "installing ${missing[*]} (details in $log)"
    apt_update
    apt-get install -y -qq --no-install-recommends "${missing[@]}" >>"$log" 2>&1 ||
        fail "could not install ${missing[*]}" "see $log, then rerun the same command" PACKAGE_INSTALL_FAILED
}
# apt_repo <name> <key url> <sources line with @KEYRING@> [asc|gpg]: the key file extension matches its format
apt_repo() {
    # The key's own format decides the name: apt (sqv on Ubuntu 26.04) reads
    # an .asc file only when it is ASCII-armored, a binary key only as .gpg.
    local ring=/usr/share/keyrings/$1.${4:-asc}
    [[ -f /etc/apt/sources.list.d/$1.list ]] && return 0
    apt_install ca-certificates curl gnupg lsb-release
    install -d /usr/share/keyrings
    curl -fsSL -o "$ring" "$2" || fail "could not download the signing key for $1" "check the server's internet access, then rerun the same command" NETWORK_UNREACHABLE
    echo "${3//@KEYRING@/$ring}" >"/etc/apt/sources.list.d/$1.list"
    apt_updated=
    apt_update
}
install_debian_packages() {
    export DEBIAN_FRONTEND=noninteractive
    apt_install ca-certificates curl openssl python3 iproute2 sudo
    if ! apt_candidate "postgresql-$pg"; then
        apt_repo pgdg https://www.postgresql.org/media/keys/ACCC4CF8.asc \
            "deb [signed-by=@KEYRING@] https://apt.postgresql.org/pub/repos/apt $(lsb_release -cs)-pgdg main"
    fi
    apt_install "postgresql-$pg" "postgresql-client-$pg"
    if ! apt_candidate caddy; then
        apt_repo caddy-stable https://dl.cloudsmith.io/public/caddy/stable/gpg.key \
            'deb [signed-by=@KEYRING@] https://dl.cloudsmith.io/public/caddy/stable/deb/debian any-version main'
    fi
    apt_install caddy
}
dnf_install() {
    local p missing=()
    for p in "$@"; do rpm -q "$p" >/dev/null 2>&1 || missing+=("$p"); done
    ((${#missing[@]})) || return 0
    say "installing ${missing[*]} (details in $log)"
    dnf install -y -q "${missing[@]}" >>"$log" 2>&1 || fail "could not install ${missing[*]}" "see $log, then rerun the same command" PACKAGE_INSTALL_FAILED
}
install_rhel_packages() {
    dnf_install ca-certificates curl openssl python3 iproute sudo policycoreutils-python-utils firewalld dnf-plugins-core
    if ! rpm -q pgdg-redhat-repo >/dev/null 2>&1; then
        say "installing the PostgreSQL PGDG repository (details in $log)"
        dnf install -y -q "https://download.postgresql.org/pub/repos/yum/reporpms/EL-10-$(uname -m)/pgdg-redhat-repo-latest.noarch.rpm" >>"$log" 2>&1 ||
            fail 'could not install the PostgreSQL PGDG repository' "see $log, then rerun the same command" PACKAGE_INSTALL_FAILED
    fi
    # Migrations use trusted extensions such as btree_gist from PGDG's contrib package.
    dnf_install "postgresql${pg}" "postgresql${pg}-server" "postgresql${pg}-contrib"
    if ! rpm -q caddy >/dev/null 2>&1; then
        say "enabling the official Caddy COPR repository (details in $log)"
        dnf copr enable -y @caddy/caddy >>"$log" 2>&1 || fail 'could not enable the Caddy repository' "see $log, then rerun the same command" PACKAGE_INSTALL_FAILED
        dnf_install caddy
    fi
}
install_packages() {
    case $os_family in
        debian) install_debian_packages ;;
        rhel) install_rhel_packages ;;
    esac
}
ensure_github_cli() {
    gh attestation verify --help >/dev/null 2>&1 && return 0
    say "installing GitHub CLI with artifact-attestation support (details in $log)"
    case $os_family in
        debian)
            apt_repo github-cli https://cli.github.com/packages/githubcli-archive-keyring.gpg \
                "deb [arch=$(dpkg --print-architecture) signed-by=@KEYRING@] https://cli.github.com/packages stable main" gpg
            apt-get install -y -qq --no-install-recommends gh >>"$log" 2>&1 ||
                fail 'could not install GitHub CLI' "see $log, then rerun the same command" PACKAGE_INSTALL_FAILED
            ;;
        rhel)
            [[ -f /etc/yum.repos.d/gh-cli.repo ]] ||
                dnf config-manager --add-repo https://cli.github.com/packages/rpm/gh-cli.repo >>"$log" 2>&1 ||
                fail 'could not add the GitHub CLI repository' "see $log, then rerun the same command" PACKAGE_INSTALL_FAILED
            dnf install -y -q gh >>"$log" 2>&1 || fail 'could not install GitHub CLI' "see $log, then rerun the same command" PACKAGE_INSTALL_FAILED
            ;;
    esac
    gh attestation verify --help >/dev/null 2>&1 ||
        fail 'the installed GitHub CLI cannot verify release provenance' "install a current gh from https://cli.github.com/packages (see $log), then rerun the same command" PACKAGE_INSTALL_FAILED
}
hold_caddy_package() {
    if [[ $os_family == debian ]]; then
        apt-mark hold caddy >>"$log" 2>&1 || fail 'could not hold the caddy package' "see $log"
    else
        dnf_install python3-dnf-plugin-versionlock
        dnf versionlock add caddy >>"$log" 2>&1 || fail 'could not lock the caddy package' "see $log"
    fi
}
caddy_restart=
ensure_cloudflare_module() {
    hold_caddy_package
    caddy list-modules 2>/dev/null | grep -qx dns.providers.cloudflare && return 0
    say "adding Caddy's Cloudflare DNS module (details in $log)"
    caddy add-package github.com/caddy-dns/cloudflare >>"$log" 2>&1 || fail "could not add Caddy's Cloudflare DNS module" "see $log"
    caddy_restart=1
    if [[ $os_family == rhel ]] && selinuxenabled; then
        semanage fcontext -a -t bin_t /usr/bin/caddy 2>/dev/null || semanage fcontext -m -t bin_t /usr/bin/caddy
        restorecon /usr/bin/caddy
    fi
}
# ---------------------------------------------------------------- users
ensure_users() {
    # RHEL useradd does not create missing parents of a new home directory.
    install -d -m 0755 -o root -g root "$root"
    getent group bilibi >/dev/null || groupadd --system bilibi
    getent passwd bilibi >/dev/null ||
        useradd --system --gid bilibi --no-create-home --home-dir "$root" --shell /usr/sbin/nologin bilibi
    getent passwd bilibi-build >/dev/null ||
        useradd --system --user-group --create-home --home-dir "$root/build" --shell /bin/bash bilibi-build
    # bilibi-build reads each instance's env (the database address) to migrate.
    id -nG bilibi-build | tr ' ' '\n' | grep -qx bilibi || usermod -aG bilibi bilibi-build
    install -d -m 0750 -o bilibi-build -g bilibi-build "$root/build"
    install_sudoers
}
# install_sudoers: the rule for bilibi-build, naming every instance (and the
# one `create` is making), replaced only when it differs. The new rule is
# validated before it is moved in, so a refused rule never reaches
# /etc/sudoers.d where it would break sudo.
install_sudoers() {
    [[ -d /etc/sudoers.d ]] || return 0
    local name commands='/usr/bin/systemctl reload caddy' rendered
    for name in $({ instances; [[ -z ${creating_instance:-} ]] || echo "$creating_instance"; } | sort -u); do
        commands+=", /usr/bin/systemctl start bilibi@$name, /usr/bin/systemctl stop bilibi@$name, /usr/bin/systemctl restart bilibi@$name"
    done
    rendered=$(mktemp /etc/sudoers.d/.bilibi-build.XXXXXX)
    sed "s|@COMMANDS@|$commands|" "$here/server/sudoers" >"$rendered"
    chmod 0440 "$rendered"
    if cmp -s "$rendered" /etc/sudoers.d/bilibi-build; then rm -f -- "$rendered"; return 0; fi
    if ! visudo -cf "$rendered" >/dev/null 2>>"$log"; then
        rm -f -- "$rendered"
        fail 'the sudoers rule for bilibi-build did not validate; nothing was changed' "see $log" SUDOERS_INVALID
    fi
    mv -T "$rendered" /etc/sudoers.d/bilibi-build
}
# retire_worker: servers that once built software ran bilibi-worker@<instance>.
# Stop and disable each, then remove the unit template and the old sudoers
# rule. Does nothing once the template is gone, so fresh installs skip it.
retire_worker() {
    local template=/etc/systemd/system/bilibi-worker@.service instance stuck=
    [[ -f $template ]] || { install_sudoers; return 0; }
    for instance in $(instances); do
        systemctl disable --now "bilibi-worker@$instance" >/dev/null 2>&1 || stuck=1
    done
    [[ -z $stuck ]] || fail 'could not stop bilibi-worker@<instance>, which Bilibi no longer uses' 'run: systemctl disable --now "bilibi-worker@*", then rerun this command'
    rm -f "$template"
    systemctl daemon-reload
    install_sudoers
    say 'retired the obsolete bilibi-worker@<instance> services'
}

# ---------------------------------------------------------------- database
postgres_port() {
    case $os_family in
        debian) pg_lsclusters -h | awk -v v="$pg" '$1 == v {print $3; exit}' ;;
        rhel) runuser -u postgres -- "$pg_bin/psql" -d postgres -Atq -c 'SHOW port' 2>/dev/null || true ;;
    esac
}
ensure_cluster() {
    case $os_family in
        debian)
            if ! pg_lsclusters -h | awk '{print $1}' | grep -qx "$pg"; then
                pg_createcluster "$pg" main >>"$log" 2>&1
            fi
            pg_ctlcluster "$pg" main start 2>/dev/null || true
            systemctl enable --now "$pg_service" >/dev/null 2>&1 || true
            ;;
        rhel)
            if [[ ! -s "/var/lib/pgsql/$pg/data/PG_VERSION" ]]; then
                "$pg_bin/postgresql-$pg-setup" initdb >>"$log" 2>&1 || fail "could not initialize PostgreSQL $pg" "see $log"
            fi
            systemctl enable --now "$pg_service" >/dev/null 2>&1 || fail "could not start PostgreSQL $pg" "check systemctl status $pg_service"
            ;;
    esac
    local port
    port=$(postgres_port)
    [[ -n $port ]] || fail "PostgreSQL $pg has no cluster" "check systemctl status $pg_service"
    "$pg_bin/pg_isready" -q -h 127.0.0.1 -p "$port" || fail "PostgreSQL $pg is not accepting connections on port $port" "check systemctl status $pg_service"
    echo "$port"
}
# database_name <instance>
database_name() { printf 'bilibi_%s' "${1//-/_}"; }
# ensure_database <instance> <password>: a role and a database of its own.
# psql interpolates :variables only from standard input, never from -c.
ensure_database() {
    local name psql=(runuser -u postgres -- "$pg_bin/psql" -v ON_ERROR_STOP=1 -Atq)
    name=$(database_name "$1")
    if [[ $("${psql[@]}" -c "SELECT 1 FROM pg_roles WHERE rolname = '$name'") != 1 ]]; then
        echo "CREATE ROLE :\"name\" LOGIN PASSWORD :'password'" | "${psql[@]}" -v name="$name" -v password="$2" >/dev/null
    fi
    if [[ $("${psql[@]}" -c "SELECT 1 FROM pg_database WHERE datname = '$name'") != 1 ]]; then
        echo 'CREATE DATABASE :"name" OWNER :"name"' | "${psql[@]}" -v name="$name" >/dev/null
    fi
}

# ---------------------------------------------------------------- ports
port_in_use() {
    [[ -n $(ss -H -ltn "sport = :$1") ]] && return 0
    local other
    for other in $(instances); do
        [[ $(env_value "$other" BIND_ADDR) == *":$1" || $(env_value "$other" PUBLIC_BIND_ADDR) == *":$1" ]] && return 0
    done
    return 1
}
free_port() {
    local port=4300
    while port_in_use "$port"; do ((port += 1)); ((port <= 65535)) || fail 'no free local port'; done
    echo "$port"
}

# ---------------------------------------------------------------- files
render_waiting_page() { # <instance> <site> <out>
    # This file is static while the app is offline. Identify its address and
    # folder, never cache a server role that an operator can change in settings.
    python3 - "$here/waiting-page.html" "$3" "$2" "$1" <<'PY'
import html, sys
source, out, site, instance = sys.argv[1:]
page = open(source, encoding="utf-8").read()
def swap(old, new):
    global page
    if page.count(old) != 1:
        sys.exit(f"waiting-page.html no longer has exactly one {old[:40]!r}; update install-server.sh")
    page = page.replace(old, new)
swap('<title>Bilibi is starting</title>',
     '<title>Bilibi is updating</title>')
swap('<p class="instance" id="waiting-instance" hidden><span class="role" id="waiting-role"></span><span id="waiting-site"></span></p>',
     f'<p class="instance" id="waiting-instance"><span class="role" id="waiting-role">{html.escape(instance)}</span><span id="waiting-site">{html.escape(site)}</span></p>')
swap('<h1 id="waiting-heading">Bilibi is starting</h1>', f'<h1 id="waiting-heading">Bilibi is updating</h1>')
swap('<p id="waiting-text">You can continue when it\'s ready; this page will reload automatically.</p>',
     f'<p id="waiting-text">Bilibi is switching to a new release or restarting. Back in about a minute. You can continue when it\'s ready; this page will reload automatically.</p>')
swap('<p class="hint" id="waiting-hint">Taking longer than usual. If you started Bilibi, check the <code>./bilibi start</code> terminal for errors.</p>',
     f'<p class="hint" id="waiting-hint">Taking longer than usual. The server owner can check the service with <code>systemctl status bilibi@{html.escape(instance)}</code> and <code>journalctl -u bilibi@{html.escape(instance)}</code>.</p>')
open(out, "w", encoding="utf-8").write(page)
PY
}
cloudflare_env_key() { printf 'BILIBI_CLOUDFLARE_API_TOKEN_%s' "$(tr '[:lower:]-' '[:upper:]_' <<<"$1")"; }
write_caddy_site() { # <instance> <site> <port> <tls> <caddy env> [public site] [public port] [staff bind] [public bind]
    local tls='' token_key
    case $4 in
        internal) tls='tls internal' ;;
        cloudflare)
            token_key=$(cloudflare_env_key "$1")
            tls=$(printf 'tls {\n\t\tdns cloudflare {env.%s}\n\t}' "$token_key")
            ;;
    esac
    install -d /etc/caddy/sites
    python3 - "$here/server/site.caddy" "/etc/caddy/sites/bilibi-$1.caddy" "$1" "$2" "$3" "$tls" "${6:-}" "${7:-}" "${8:-}" "${9:-}" <<'PY'
import sys
source, out, instance, site, port, tls, public_site, public_port, staff_bind, public_bind = sys.argv[1:]
text = open(source, encoding="utf-8").read()
public = ""
if public_site:
    tls_line = f"\t{tls}\n" if tls else ""
    bind_line = f"\tbind {public_bind}\n" if public_bind else ""
    public = f'''\n{public_site} {{
{tls_line}{bind_line}\tencode zstd gzip
\treverse_proxy 127.0.0.1:{public_port}
\timport bilibi_waiting_{instance}
}}\n'''
staff_bind = f"bind {staff_bind}" if staff_bind else ""
for old, new in (("@INSTANCE@", instance), ("@SITE@", site), ("@PORT@", port), ("@TLS@", tls), ("@STAFF_BIND@", staff_bind), ("@PUBLIC_BLOCK@", public)):
    text = text.replace(old, new)
open(out, "w", encoding="utf-8").write(text)
PY
    local caddyfile=/etc/caddy/Caddyfile import='import /etc/caddy/sites/*.caddy'
    if [[ ! -f $caddyfile ]] || grep -q "Set this path to your site's directory" "$caddyfile"; then
        [[ ! -f $caddyfile || -f $caddyfile.dist ]] || cp "$caddyfile" "$caddyfile.dist"
        printf '# Managed by Bilibi scripts/install-server.sh\n%s\n' "$import" >"$caddyfile"
    elif ! grep -qF "$import" "$caddyfile"; then
        printf '\n# Added by Bilibi scripts/install-server.sh\n%s\n' "$import" >>"$caddyfile"
    fi
    local validate=(caddy validate --config "$caddyfile")
    [[ ! -f $5 ]] || validate+=(--envfile "$5")
    "${validate[@]}" >/dev/null 2>&1 ||
        fail "/etc/caddy/Caddyfile does not validate" "run: caddy validate --config $caddyfile"
    systemctl enable caddy >/dev/null 2>&1
    if [[ -n $caddy_restart ]] || ! systemctl is-active --quiet caddy; then
        systemctl restart caddy
    else
        systemctl reload caddy
    fi
}
configure_selinux() { # <instance>
    [[ $os_family == rhel ]] || return 0
    selinuxenabled || fail 'SELinux must stay enabled on Rocky Linux and RHEL' 'enable SELinux enforcing, reboot, then rerun this command'
    [[ $(getenforce) == Enforcing ]] || fail "SELinux is $(getenforce), not Enforcing" 'set SELINUX=enforcing, reboot, then rerun this command'
    getsebool httpd_can_network_connect | grep -q -- '--> on' || setsebool -P httpd_can_network_connect on
    local dir; dir=$(instance_dir "$1")
    semanage fcontext -a -t var_lib_t "$dir/data(/.*)?" 2>/dev/null || semanage fcontext -m -t var_lib_t "$dir/data(/.*)?"
    semanage fcontext -a -t var_lib_t "$dir/incoming(/.*)?" 2>/dev/null || semanage fcontext -m -t var_lib_t "$dir/incoming(/.*)?"
    semanage fcontext -a -t httpd_sys_content_t "$dir/site(/.*)?" 2>/dev/null || semanage fcontext -m -t httpd_sys_content_t "$dir/site(/.*)?"
    restorecon -R "$dir/data" "$dir/incoming" "$dir/site"
}
configure_caddy_environment() {
    install -d -m 0755 /etc/systemd/system/caddy.service.d
    local dropin=/etc/systemd/system/caddy.service.d/bilibi.conf new
    new=$(mktemp)
    printf '%s\n' \
        '[Service]' \
        'EnvironmentFile=-/opt/bilibi/*/caddy-env' \
        '# The packaged unit uses --environ, which would write DNS tokens to the journal.' \
        'ExecStart=' \
        'ExecStart=/usr/bin/caddy run --config /etc/caddy/Caddyfile' \
        >"$new"
    cmp -s "$new" "$dropin" || { install -m 0644 "$new" "$dropin"; caddy_restart=1; }
    rm -f "$new"
    systemctl daemon-reload
}
configure_firewall() {
    [[ $os_family == rhel ]] || return 0
    systemctl enable --now firewalld >/dev/null 2>&1 || fail 'could not start firewalld' 'check systemctl status firewalld'
    local changed=
    firewall-cmd --permanent --query-service=http >/dev/null || { firewall-cmd --permanent --add-service=http >/dev/null; changed=1; }
    firewall-cmd --permanent --query-service=https >/dev/null || { firewall-cmd --permanent --add-service=https >/dev/null; changed=1; }
    [[ -z $changed ]] || firewall-cmd --reload >/dev/null
}
install_unit() {
    local name reload=
    for name in "$unit_name" "$installer_unit"; do
        if ! cmp -s "$here/server/$name.service" "/etc/systemd/system/$name.service"; then
            install -m 0644 "$here/server/$name.service" "/etc/systemd/system/$name.service"
            reload=1
        fi
    done
    [[ -z $reload ]] || systemctl daemon-reload
}
# start_installer <instance>: the installer service for an instance, once the
# program is in place (`installer` puts it there).
start_installer() {
    [[ -x $installer_bin ]] || return 0
    systemctl enable --now "$installer_unit$1" >/dev/null 2>&1 || say "Warning: could not start $installer_unit$1; see: systemctl status $installer_unit$1"
}
# ================================================================ create
create() {
    local instance=${1:-} role='' site='' public_site='' staff_bind='' public_bind='' public_tenant='' remove_site='' remove_staff_bind='' port='' public_port='' tls='' cloudflare_token_stdin='' cloudflare_token=''
    [[ -n $instance ]] || usage
    need_root create "$@"
    shift
    while (($#)); do
        case $1 in
            --role) role=${2:-}; shift 2 ;;
            --site) site=${2:-}; shift 2 ;;
            --public-site) [[ -n ${2:-} ]] || usage; public_site=$2; shift 2 ;;
            --no-public-site) remove_site=1; shift ;;
            --public-tenant) [[ -n ${2:-} ]] || usage; public_tenant=$2; shift 2 ;;
            --no-staff-bind) remove_staff_bind=1; shift ;;
            --public-bind) [[ -n ${2:-} ]] || usage; public_bind=$2; shift 2 ;;
            --staff-bind) [[ -n ${2:-} ]] || usage; staff_bind=$2; shift 2 ;;
            --port) port=${2:-}; shift 2 ;;
            --tls) tls=${2:-}; shift 2 ;;
            --cloudflare-token-stdin) cloudflare_token_stdin=1; shift ;;
            *) usage ;;
        esac
    done
    valid_name "$instance" || fail "instance name must be 1 to 32 lower-case letters, digits or hyphens: $instance" '' INVALID_INPUT
    [[ $role =~ ^(dev|staging|production)$ ]] || fail '--role must be dev, staging or production' '' INVALID_INPUT
    [[ $site =~ ^[a-z0-9.-]+$ ]] || fail '--site must be a host name, like erp.example.com' '' INVALID_INPUT
    [[ -z $remove_staff_bind || ( -z $staff_bind && -z $public_bind ) ]] || usage
    [[ -z $remove_site || ( -z $public_site && -z $public_tenant && -z $public_bind ) ]] || usage
    local old_origin='' old_public_origin='' old_public_bind='' old_public_tenant=''
    if [[ -f $(instance_dir "$instance")/env ]]; then
        old_origin=$(env_value "$instance" APP_ORIGIN)
        old_public_origin=$(env_value "$instance" PUBLIC_ORIGIN)
        old_public_bind=$(env_value "$instance" PUBLIC_BIND_ADDR)
        old_public_tenant=$(env_value "$instance" PUBLIC_TENANT_ID)
        staff_bind=${staff_bind:-$(env_value "$instance" CADDY_STAFF_BIND)}
        public_bind=${public_bind:-$(env_value "$instance" CADDY_PUBLIC_BIND)}
        tls=${tls:-$(env_value "$instance" CADDY_TLS)}
    fi
    if [[ -n $remove_site ]]; then
        public_bind=''
    else
        public_site=${public_site:-${old_public_origin#https://}}
        if [[ -n $public_site ]]; then
            public_tenant=${public_tenant:-${old_public_tenant:-1}}
        else
            [[ -z $public_tenant && -z $public_bind ]] || usage
        fi
    fi
    if [[ -n $remove_staff_bind ]]; then
        staff_bind=''
        public_bind=''
    fi
    [[ -z $public_site || $public_site =~ ^[a-z0-9.-]+$ ]] || fail '--public-site must be a host name, like shop.example.com' '' INVALID_INPUT
    [[ -z $public_site || $public_site != "$site" ]] || fail '--public-site must differ from --site' '' INVALID_INPUT
    [[ -z $public_tenant || $public_tenant =~ ^[1-9][0-9]*$ ]] || fail '--public-tenant must be a positive tenant ID' '' INVALID_INPUT
    [[ -z $port || $port =~ ^[0-9]+$ ]] || fail '--port must be a number' '' INVALID_INPUT
    [[ -z $port || ( $port -ge 1 && $port -le 65535 ) ]] || fail '--port must be between 1 and 65535' '' INVALID_INPUT
    [[ -z $tls || $tls == internal || $tls == cloudflare ]] || fail '--tls takes "internal" or "cloudflare"' '' INVALID_INPUT
    [[ -z $cloudflare_token_stdin || $tls == cloudflare ]] || fail '--cloudflare-token-stdin requires --tls cloudflare' '' INVALID_INPUT
    creating_instance=$instance
    [[ -f $here/waiting-page.html && -f $here/server/site.caddy ]] || fail "run this from a Bilibi checkout: $here/server is missing"
    [[ -n $tls ]] || check_dns "$site"
    [[ -n $tls || -z $public_site ]] || check_dns "$public_site"

    install_packages
    python3 - "$staff_bind" "$public_bind" "$public_site" "$public_tenant" <<'PYTHON'
import ipaddress
import sys

staff, public, site, tenant = sys.argv[1:]
try:
    staff = ipaddress.ip_address(staff) if staff else None
    public = ipaddress.ip_address(public) if public else None
except ValueError as error:
    sys.exit(f"Invalid Caddy bind address: {error}")

if any(ip.is_unspecified or getattr(ip, "ipv4_mapped", None) or getattr(ip, "scope_id", None) for ip in (staff, public) if ip):
    sys.exit("Caddy bind addresses must identify specific interfaces")
if site and ((staff is None) != (public is None) or (staff is not None and staff == public)):
    sys.exit("Use distinct --staff-bind and --public-bind addresses with private staff ingress")
if tenant and int(tenant) > 2**63 - 1:
    sys.exit("--public-tenant is outside the supported tenant ID range")
PYTHON
    ensure_github_cli
    [[ $tls == cloudflare ]] && ensure_cloudflare_module
    ensure_users
    install_unit
    retire_worker
    local dir env caddy_env created=
    dir=$(instance_dir "$instance"); env=$dir/env; caddy_env=$dir/caddy-env
    install -d -m 0755 "$root" "$dir" "$dir/site"
    install -d -m 0755 -o bilibi-build -g bilibi "$dir/releases"
    install -d -m 0750 -o bilibi-build -g bilibi-build "$dir/backups"
    ensure_incoming "$instance"
    ensure_current_link "$instance"
    install -d -m 0700 -o bilibi -g bilibi "$dir/data" "$dir/data/media" "$dir/data/mailbox"

    local pg_port password database
    pg_port=$(ensure_cluster)
    database=$(database_name "$instance")
    if [[ -f $env ]]; then
        password=$(sed -n 's|^DATABASE_URL=postgres://[^:]*:\([^@]*\)@.*|\1|p' "$env")
    else
        password=$(openssl rand -hex 24)
    fi
    [[ -n $password ]] || fail "$env has no DATABASE_URL password" "restore the original database credentials in $env before retrying"
    local token_key; token_key=$(cloudflare_env_key "$instance")
    if [[ $tls == cloudflare && -n $cloudflare_token_stdin ]]; then
        IFS= read -r cloudflare_token || true
        [[ $cloudflare_token =~ ^[A-Za-z0-9_-]+$ ]] || fail 'the Cloudflare API token on standard input is empty or invalid'
    fi
    if [[ $tls == cloudflare && -z $cloudflare_token && ! -f $caddy_env ]]; then
        fail '--tls cloudflare needs a zone-scoped API token on standard input' 'pipe the token to this command and add --cloudflare-token-stdin'
    fi
    if [[ -f $env ]]; then
        local saved_bind
        saved_bind=$(env_value "$instance" BIND_ADDR)
        [[ -z $port || $saved_bind == "127.0.0.1:$port" ]] || fail "$instance already listens on $saved_bind; the port is fixed at creation"
        port=${saved_bind##*:}
    else
        [[ -n $port ]] || port=$(free_port)
        port_in_use "$port" && fail "port $port is in use" 'pick another with --port' PORT_IN_USE
    fi
    local public_origin='' public_backend=''
    if [[ -n $public_site ]]; then
        public_port=${old_public_bind##*:}
        if [[ -z $public_port ]]; then
            public_port=$((port + 1))
            while ((public_port <= 65535)) && port_in_use "$public_port"; do
                ((public_port += 1))
            done
            ((public_port <= 65535)) || fail 'no free local port for --public-site'
        fi
        public_origin=https://$public_site
        public_backend=127.0.0.1:$public_port
    fi
    if [[ ! -f $env ]]; then
        created=1
        local bilibi_env
        case $role in dev) bilibi_env=development ;; *) bilibi_env=$role ;; esac
        umask 027
        local env_new
        env_new=$(mktemp "$dir/.env.XXXXXX")
        cat >"$env_new" <<EOF
# Bilibi instance "$instance". Read by bilibi@$instance.service and the release commands.
# BILIBI_ENV initializes the role at install. Later changes use Instance settings.
# The saved Dev role skips ordinary 2FA checks; role and software changes always ask.
BILIBI_ENV=$bilibi_env
APP_ORIGIN=https://$site
BIND_ADDR=127.0.0.1:$port
DATABASE_URL=postgres://$database:$password@127.0.0.1:$pg_port/$database
BILIBI_SETTINGS_KEY=$(openssl rand -hex 32)
BILIBI_USER_SIGNING_KEY=$(openssl rand -hex 32)
BILIBI_MEDIA_SIGNING_KEY=$(openssl rand -hex 32)
BILIBI_MAIL_DIR=$dir/data/mailbox
# Where the program receives releases and the installer keeps them.
BILIBI_INSTANCE_DIR=$dir
EOF
        chown root:bilibi "$env_new"
        chmod 0640 "$env_new"
        mv -T "$env_new" "$env"
        umask 022
    else
        local kept_role; kept_role=$(role_of "$instance")
        [[ $kept_role == "$role" ]] || say "$instance keeps its saved role ($kept_role). Change the role in Instance settings with 2FA."
        role=$kept_role
        sed -i "s|^APP_ORIGIN=.*|APP_ORIGIN=https://$site|" "$env"
        # Older layouts gain the instance path used by release transport.
        grep -q '^BILIBI_INSTANCE_DIR=' "$env" || printf 'BILIBI_INSTANCE_DIR=%s\n' "$dir" >>"$env"
    fi
    sed -i '/^PUBLIC_ORIGIN=/d; /^PUBLIC_BIND_ADDR=/d; /^PUBLIC_TENANT_ID=/d; /^CADDY_STAFF_BIND=/d; /^CADDY_PUBLIC_BIND=/d; /^CADDY_TLS=/d' "$env"
    if [[ -n $public_site ]]; then
        printf 'PUBLIC_ORIGIN=%s\nPUBLIC_BIND_ADDR=%s\nPUBLIC_TENANT_ID=%s\n' "$public_origin" "$public_backend" "$public_tenant" >>"$env"
    fi
    printf 'CADDY_STAFF_BIND=%s\nCADDY_PUBLIC_BIND=%s\nCADDY_TLS=%s\n' "$staff_bind" "$public_bind" "$tls" >>"$env"
    # Persist the generated credentials before creating the database role. A
    # retry after interruption must use the same password as an existing role.
    ensure_database "$instance" "$password"
    if [[ -n $cloudflare_token ]]; then
        [[ $(cat "$caddy_env" 2>/dev/null) == "$token_key=$cloudflare_token" ]] || caddy_restart=1
        umask 027
        printf '%s=' "$token_key" >"$caddy_env"
        printf '%s\n' "$cloudflare_token" >>"$caddy_env"
        umask 022
    elif [[ $tls != cloudflare && -f $caddy_env ]]; then
        rm -f "$caddy_env"
        caddy_restart=1
    fi
    if [[ $tls == cloudflare ]] && ! grep -q "^$token_key=" "$caddy_env"; then
        fail '--tls cloudflare needs a zone-scoped API token on standard input' 'pipe the token to this command and add --cloudflare-token-stdin'
    fi
    chown root:bilibi "$env"; chmod 0640 "$env"
    [[ ! -f $caddy_env ]] || { chown root:caddy "$caddy_env"; chmod 0640 "$caddy_env"; }

    render_waiting_page "$instance" "$site" "$dir/site/waiting.html"
    chmod 0644 "$dir/site/waiting.html"
    configure_selinux "$instance"
    configure_caddy_environment
    write_caddy_site "$instance" "$site" "$port" "$tls" "$caddy_env" "$public_site" "$public_port" "$staff_bind" "$public_bind"
    if [[ $old_origin != "https://$site" || $old_public_origin != "$public_origin" || $old_public_bind != "$public_backend" || $old_public_tenant != "$public_tenant" ]]; then
        if systemctl is-active --quiet "$unit_name$instance"; then
            systemctl restart "$unit_name$instance"
            wait_healthy "$instance" || fail "$instance did not become ready with the new listener configuration"
        fi
    fi
    configure_firewall
    systemctl enable "$unit_name$instance" >/dev/null 2>&1
    start_installer "$instance"

    say "$instance: $([[ -n $created ]] && echo created || echo updated) as $role at https://$site (backend 127.0.0.1:$port), files under $dir"
    [[ -z $public_site ]] || say "$instance: public site at https://$public_site (backend 127.0.0.1:$public_port)"
    if [[ -z $(current_release "$instance") ]]; then
        say "Next: put a verified release into service with: $0 release $instance <setup archive> --repo <owner/name> [--repo-id <n>] --tag <setup-vX.Y.Z>"
    fi
    if [[ ! -x $installer_bin ]]; then
        say "Then let the Software page install releases here with: $0 installer <bilibi-installer> --repo <owner/name> [--repo-id <n>] --tag <setup-vX.Y.Z>"
    fi
}

# expected_repository_id <repository> <given id>: the GitHub repository number
# the signature must come from, so a reused repository name cannot pass. Only
# the official repositories are pinned here; an application repository needs
# its number from --repo-id, copied from its GitHub Settings page or another
# trusted record before the install, never looked up from its name at install time.
expected_repository_id() {
    local repository=$1 given=$2 pinned=''
    case $repository in
        BelimbingProject/bilibi) pinned=1418519406 ;;
        BelimbingProject/bilibi-app-maker) pinned=1426391840 ;;
    esac
    if [[ -n $given ]]; then
        [[ $given =~ ^[1-9][0-9]*$ ]] || fail '--repo-id must be the GitHub repository number' '' INVALID_INPUT
        [[ -z $pinned || $given == "$pinned" ]] || fail "--repo-id $given is not the official number for $repository; nothing was installed"
        printf '%s' "$given"
    elif [[ -n $pinned ]]; then
        printf '%s' "$pinned"
    else
        fail "$repository is not an official repository, so its GitHub repository number is needed" "rerun with --repo-id <number>, taken from a trusted source"
    fi
}

# stage_source <file>: copy the operator's file once into a private folder only
# root can enter, set $staged to that copy, and remove the folder on every exit.
# Verify and use only the copy, so the original cannot change in between.
staging=''
staged=''
stage_source() {
    staging=$(mktemp -d /tmp/bilibi-stage.XXXXXX) || fail 'cannot create a private staging folder'
    trap 'rm -rf -- "$staging"' EXIT
    staged=$staging/artifact
    install -m 0700 -o root -g root "$1" "$staged" || fail "cannot copy $1 for verification; nothing was installed"
}

# verify_provenance <artifact> <repository> <repository id> <tag>: GitHub
# verifies the file's digest, signing repository, release workflow and tag, and
# the signing repository's number must equal the expected one. Public
# repositories do not need a login. A private one is read with the server's
# GitHub App (a fresh read-only token for this one command, lib/github-app.py),
# or with a read token the operator passes as GH_TOKEN; either is inherited
# only by gh and never printed.
verify_provenance() {
    local artifact=$1 repository=$2 repository_id=$3 tag=$4 ids id found=0
    command -v gh >/dev/null || fail 'GitHub CLI is not installed, so release provenance cannot be checked' 'rerun create to install the server packages, then retry'
    valid_repository "$repository" || fail 'the release repository must be owner/name' '' INVALID_INPUT
    valid_setup_tag "$tag" || fail 'the release tag must be setup-vX.Y.Z' '' INVALID_INPUT
    repository_id=$(expected_repository_id "$repository" "$repository_id")
    local check=(gh attestation verify "$artifact" --repo "$repository"
        --signer-workflow "$repository/.github/workflows/release.yml"
        --source-ref "refs/tags/$tag"
        --format json --jq '.[].verificationResult.signature.certificate.sourceRepositoryIdentifier')
    if [[ -z ${GH_TOKEN:-} && -f $github_app ]]; then
        ids=$(python3 "$here/lib/github-app.py" run "$github_app" --repo "$repository" --log "$log" -- "${check[@]}") ||
            fail "GitHub build provenance did not verify for $repository tag $tag with the server's GitHub App; nothing was installed" "check the App with: $0 github-app --check, then download the asset from the official Release setup workflow and retry" SIGNATURE_INVALID
    else
        ids=$("${check[@]}" 2>>"$log") ||
            fail "GitHub build provenance did not verify for $repository tag $tag; nothing was installed" 'download the asset from the official Release setup workflow and retry' SIGNATURE_INVALID
    fi
    while IFS= read -r id; do
        [[ -n $id ]] || continue
        found=1
        [[ $id == "$repository_id" ]] || fail "the release was signed by a different GitHub repository number than $repository_id; nothing was installed" 'use the asset from the repository you intend, or check the --repo-id you gave' SIGNATURE_WRONG_REPOSITORY
    done <<<"$ids"
    ((found)) || fail "GitHub returned no verified provenance for $repository tag $tag; nothing was installed" 'download the asset from the official Release setup workflow and retry' SIGNATURE_INVALID
}

# unpack_release <archive> <folder>: accept only the release archive's plain
# files and verify its own checksum list before root copies anything from it.
unpack_release() {
    python3 - "$1" "$2" <<'PY'
import hashlib
from pathlib import Path, PurePosixPath
import shutil
import sys
import tarfile

archive, target = Path(sys.argv[1]), Path(sys.argv[2])
allowed = {'bilibi-web', 'SHA256SUMS', 'manifest.txt', 'Cargo.lock', 'release.json', 'bundle.json'}
seen = set()
with tarfile.open(archive, 'r:gz') as source:
    members = source.getmembers()
    for member in members:
        name = member.name.removeprefix('./')
        path = PurePosixPath(name)
        parts = path.parts
        safe_plugin = len(parts) > 1 and parts[0] == 'plugins' and all(
            part and not part.startswith('.') and len(part) <= 128 and
            all(c.isalnum() or c in '._-' for c in part) for part in parts[1:])
        if path.is_absolute() or '..' in parts or (name not in allowed and name != 'plugins' and not safe_plugin):
            raise SystemExit(f'Release archive contains an unexpected path: {member.name}')
        if name in seen or not (member.isfile() or (member.isdir() and (name == 'plugins' or safe_plugin))):
            raise SystemExit(f'Release archive contains an unsafe or duplicate entry: {member.name}')
        seen.add(name)
    if not {'bilibi-web', 'SHA256SUMS'} <= seen:
        raise SystemExit('Release archive is missing bilibi-web or SHA256SUMS')
    target.mkdir(mode=0o700)
    for member in members:
        name = member.name.removeprefix('./')
        destination = target.joinpath(*PurePosixPath(name).parts)
        if member.isdir():
            destination.mkdir(parents=True, exist_ok=True)
            continue
        destination.parent.mkdir(parents=True, exist_ok=True)
        opened = source.extractfile(member)
        if opened is None:
            raise SystemExit(f'Release archive entry cannot be read: {name}')
        with opened, destination.open('wb') as output:
            shutil.copyfileobj(opened, output)
        destination.chmod(0o755 if name == 'bilibi-web' else 0o644)

checks = {}
for line in (target / 'SHA256SUMS').read_text().splitlines():
    digest, separator, name = line.partition('  ')
    if not separator or len(digest) != 64 or any(c not in '0123456789abcdef' for c in digest):
        raise SystemExit('Release checksum list is invalid')
    if name not in seen or name in checks or not (target / name).is_file():
        raise SystemExit(f'Release checksum list names an unexpected file: {name}')
    checks[name] = digest
    with (target / name).open('rb') as checked:
        actual = hashlib.file_digest(checked, 'sha256').hexdigest()
    if actual != digest:
        raise SystemExit(f'Release checksum failed for {name}')
if 'bilibi-web' not in checks:
    raise SystemExit('Release checksum list does not name bilibi-web')
PY
}

# ================================================================ installer
# The installer program, shared by every instance on this machine, and one
# service per instance. Rerun with a newer program to update it: the services
# restart, and a job one of them was running is settled by the rules in
# apps/installer/README.md ("If the server restarts mid-way").
installer() {
    local source=${1:-} repository='' repository_id='' tag=''
    [[ -n $source ]] || usage; shift
    while (($#)); do
        case $1 in
            --repo) repository=${2:-}; shift 2 ;;
            --repo-id) repository_id=${2:-}; shift 2 ;;
            --tag) tag=${2:-}; shift 2 ;;
            *) usage ;;
        esac
    done
    [[ -n $repository && -n $tag ]] || usage
    need_root installer "$@"
    # A downloaded release asset has no execute bit; the verified copy is
    # installed with its own mode below.
    [[ -f $source ]] || fail "$source is not a file" "download bilibi-installer-x86_64-unknown-linux-gnu from the release and give its path" INVALID_INPUT
    stage_source "$source"
    verify_provenance "$staged" "$repository" "$repository_id" "$tag"
    [[ -d $root/build ]] || fail "no instance exists yet" "create one first: $0 create <instance> --role ... --site ..."
    install_unit
    retire_worker
    install -d -m 0755 -o root -g root "$root/build/bin"
    install -m 0755 -o root -g root "$staged" "$installer_bin.new" && mv -T "$installer_bin.new" "$installer_bin"
    say "installer: $installer_bin updated"
    local instance
    for instance in $(instances); do
        ensure_current_link "$instance"
        ensure_incoming "$instance"
        systemctl enable "$installer_unit$instance" >/dev/null 2>&1
        systemctl restart "$installer_unit$instance" || say "Warning: $installer_unit$instance did not start; see: systemctl status $installer_unit$instance"
        say "$instance: $installer_unit$instance $(installer_state "$instance")"
    done
}

# ================================================================ GitHub App
# github-app: put the server's GitHub App here, check it, or remove it. The
# key comes on standard input only, so it never enters the shell history or
# the process list; it is checked against GitHub before it is kept.
github_app_command() {
    local client_id='' repository='' action=save
    while (($#)); do
        case $1 in
            --client-id) client_id=${2:-}; shift 2 ;;
            --repo) repository=${2:-}; shift 2 ;;
            --check) action=check; shift ;;
            --remove) action=remove; shift ;;
            *) usage ;;
        esac
    done
    need_root github-app
    case $action in
        check) python3 "$here/lib/github-app.py" check "$github_app" ;;
        remove)
            [[ -f $github_app ]] || { say 'No GitHub App is set up on this server; nothing was removed.'; return; }
            rm -f -- "$github_app"
            say "Removed the GitHub App from this server. Private releases now need GH_TOKEN, and Software › Releases uses its saved read token. Delete the App, or its key, on GitHub too."
            ;;
        save)
            [[ -n $client_id && -n $repository ]] || usage
            [[ ! -t 0 ]] || fail 'pipe the GitHub App private key to this command' "$0 github-app --client-id $client_id --repo $repository < app.pem"
            getent group bilibi >/dev/null || fail 'no instance exists yet' "create one first: $0 create <instance> --role ... --site ..."
            python3 "$here/lib/github-app.py" save "$github_app" --client-id "$client_id" --repo "$repository"
            ;;
    esac
}
# verify: check a file the way release and installer do, without installing
# it. Use it on a newer bilibi-server-tools.tar.gz before extracting it.
verify_command() {
    local source=${1:-} repository='' repository_id='' tag=''
    [[ -n $source ]] || usage; shift
    while (($#)); do
        case $1 in
            --repo) repository=${2:-}; shift 2 ;;
            --repo-id) repository_id=${2:-}; shift 2 ;;
            --tag) tag=${2:-}; shift 2 ;;
            *) usage ;;
        esac
    done
    [[ -n $repository && -n $tag ]] || usage
    need_root verify
    [[ -f $source ]] || fail "$source is not a file"
    stage_source "$source"
    verify_provenance "$staged" "$repository" "$repository_id" "$tag"
    say "$source: GitHub build provenance verified for $repository tag $tag."
}

# ================================================================ release
with_env() { # <instance> <user> <command...>
    local instance=$1 user=$2; shift 2
    local dir; dir=$(instance_dir "$instance")
    # shellcheck disable=SC2016 # the inner shell expands $1 and $2
    runuser -u "$user" -- bash -c 'set -a; . "$1"; set +a; shift; export HOME="$2"; shift; exec "$@"' -- "$dir/env" "$dir/data" "$@"
}
lock_installation() {
    local instance=$1 reply
    coproc POLICY { runuser -u postgres -- "$pg_bin/psql" -p "$(postgres_port)" -d "$(database_name "$instance")" -X -qAt -v ON_ERROR_STOP=1; }
    policy_pid=$POLICY_PID
    policy_in=${POLICY[1]}
    policy_out=${POLICY[0]}
    trap 'exec {policy_in}>&-; wait "$policy_pid"' EXIT
    printf '%s\n' "SELECT 'locked' FROM pg_advisory_lock_shared(hashtextextended('base/system.installation-mode',0));" >&"$policy_in"
    read -r reply <&"$policy_out" || fail 'could not read installation policy'
    [[ $reply == locked ]] || fail 'could not lock installation policy'
}
read_installation_mode() {
    local reply
    printf '%s\n' "SELECT to_regclass('base_settings') IS NOT NULL;" >&"$policy_in"
    read -r reply <&"$policy_out" || fail 'could not read installation policy'
    installation_mode=shared
    if [[ $reply == t ]]; then
        printf '%s\n' "SELECT COALESCE((SELECT value #>> '{}' FROM base_settings WHERE key='installation.mode' AND tenant_id=0 AND scope_type='global' AND scope_id=0),'shared');" >&"$policy_in"
        read -r installation_mode <&"$policy_out" || fail 'could not read installation policy'
    elif [[ $reply != f ]]; then
        fail 'could not read installation policy'
    fi
    [[ $installation_mode == shared || $installation_mode == isolated ]] || fail 'invalid installation.mode'
}
admit_release() {
    local instance=$1 n=$2 dir
    dir=$(instance_dir "$instance")
    read_installation_mode || return 1
    with_env "$instance" bilibi-build "$dir/releases/$n/bilibi-web" status | python3 -c '
import sys
mode = sys.argv[1]
status = sys.stdin.read()
if mode not in {"shared", "isolated"} or not status.startswith("Bilibi "):
    sys.exit("Invalid installation policy or release composition")
_, found, extensions = status.partition("\nExtensions:")
if not found:
    sys.exit("The release does not report its Extension composition")
if extensions.strip() == "none mounted":
    sys.exit(0)
if not extensions.startswith("\n") or not extensions.strip():
    sys.exit("Invalid Extension composition")
for line in extensions.splitlines():
    if not line:
        continue
    declaration, modules, _ = line.partition(", modules:")
    identity, code_marker, code = declaration.rpartition(", code: ")
    if not line.startswith("  ") or not modules or not code_marker or " " not in identity.strip():
        sys.exit("Invalid or missing Extension code declaration")
    name = identity.strip().split(" ", 1)[0]
    if code not in {"reviewed", "customer"}:
        sys.exit(f"Extension {name} must declare reviewed or customer code")
    if code == "customer" and mode == "shared":
        sys.exit(f"Extension {name} requires an isolated installation; use an isolated installation, or prepare a code-free release")
' "$installation_mode"
}
listener_ready() {
    local authority=${2#*://}
    authority=${authority%%/*}
    [[ -n $1 && -n $authority ]] &&
        [[ $(curl -s -o /dev/null -w '%{http_code}' --max-time 2 -H "Host: $authority" "http://$1$3" 2>/dev/null) == 200 ]]
}
# wait_healthy <instance>: the backend answers on its own port within 60 s.
wait_healthy() {
    local staff_bind staff_origin public_bind public_origin _try
    staff_bind=$(env_value "$1" BIND_ADDR)
    staff_origin=$(env_value "$1" APP_ORIGIN)
    public_bind=$(env_value "$1" PUBLIC_BIND_ADDR)
    public_origin=$(env_value "$1" PUBLIC_ORIGIN)
    for _try in $(seq 1 60); do
        if listener_ready "$staff_bind" "$staff_origin" /login &&
            { [[ -z $public_bind && -z $public_origin ]] || listener_ready "$public_bind" "$public_origin" /portal/sign-in; }; then
            return 0
        fi
        [[ $(service_state "$1") == failed ]] && return 1
        sleep 1
    done
    return 1
}
# put_in_service <instance> <n> <previous>: stop, point current, start, check;
# on failure point back at the previous release and start that.
put_in_service() {
    local instance=$1 n=$2 previous=$3 dir
    dir=$(instance_dir "$instance")
    ensure_current_link "$instance"
    systemctl stop "$unit_name$instance"
    if ! admit_release "$instance" "$n"; then
        if [[ -n $previous ]]; then
            admit_release "$instance" "$previous" || return 1
            systemctl start "$unit_name$instance"
        fi
        return 1
    fi
    point_current "$instance" "$n"
    systemctl start "$unit_name$instance"
    if wait_healthy "$instance"; then
        say "$instance: release $n is in service ($(env_value "$instance" APP_ORIGIN))"
        return 0
    fi
    say "$instance: release $n did not answer within 60 s; see: journalctl -u $unit_name$instance" >&2
    if [[ -n $previous ]]; then
        systemctl stop "$unit_name$instance"
        admit_release "$instance" "$previous" || return 1
        point_current "$instance" "$previous"
        systemctl start "$unit_name$instance"
        if wait_healthy "$instance"; then
            say "$instance: switched back to release $previous, which answers" >&2
        else
            say "$instance: release $previous does not answer either" >&2
        fi
    fi
    return 1
}
release() {
    local instance=${1:-} source=${2:-} repository='' repository_id='' tag=''
    [[ -n $instance && -n $source ]] || usage; shift 2
    while (($#)); do
        case $1 in
            --repo) repository=${2:-}; shift 2 ;;
            --repo-id) repository_id=${2:-}; shift 2 ;;
            --tag) tag=${2:-}; shift 2 ;;
            *) usage ;;
        esac
    done
    [[ -n $repository && -n $tag ]] || usage
    need_root release "$@"
    instance_exists "$instance" || fail "no instance named $instance" "create it first: $0 create $instance --role production --site <host>" NO_INSTANCE
    retire_worker
    [[ -f $source ]] || fail "$source is not a setup release archive"
    stage_source "$source"
    verify_provenance "$staged" "$repository" "$repository_id" "$tag"
    local unpacked binary dir n previous source_digest
    source_digest=$(sha256sum "$staged" | cut -d' ' -f1)
    dir=$(instance_dir "$instance")
    previous=$(current_release "$instance")
    if [[ -n $previous && -f $dir/releases/$previous/source.sha256 ]] &&
        [[ $(cat "$dir/releases/$previous/source.sha256") == "$source_digest" ]]; then
        (cd "$dir/releases/$previous" && sha256sum -c --quiet SHA256SUMS) ||
            fail "current release $previous does not match its checksum" "restore the release before retrying"
        [[ $(service_state "$instance") == active ]] && wait_healthy "$instance" ||
            fail "current release $previous is not healthy" "check systemctl status $unit_name$instance and retry" UNHEALTHY
        say "$instance: the verified release is already in service"
        return 0
    fi
    unpacked=$staging/unpacked
    if ! unpack_release "$staged" "$unpacked"; then
        fail 'the setup release archive or its checksum is invalid; nothing was installed' 'download the asset again and retry' ARCHIVE_INVALID
    fi
    binary=$unpacked/bilibi-web
    n=$(( $(release_numbers "$instance" | tail -n1) + 1 ))
    install -d -m 0755 -o bilibi-build -g bilibi "$dir/releases/$n"
    install -m 0755 -o bilibi-build -g bilibi "$binary" "$dir/releases/$n/bilibi-web"
    # The release's recipe, when the build wrote one (scripts/discover-modules.py --pin).
    local extra
    for extra in manifest.txt Cargo.lock release.json bundle.json; do
        [[ -f $unpacked/$extra ]] && install -m 0644 -o bilibi-build -g bilibi "$unpacked/$extra" "$dir/releases/$n/$extra"
    done
    install -m 0644 -o bilibi-build -g bilibi "$unpacked/SHA256SUMS" "$dir/releases/$n/SHA256SUMS"
    if [[ -d $unpacked/plugins ]]; then
        cp -R -- "$unpacked/plugins" "$dir/releases/$n/plugins"
        chown -R bilibi-build:bilibi "$dir/releases/$n/plugins"
    fi
    printf '%s\n' "$source_digest" >"$dir/releases/$n/source.sha256"
    chown bilibi-build:bilibi "$dir/releases/$n/source.sha256"
    chmod 0644 "$dir/releases/$n/source.sha256"
    rm -rf -- "$unpacked"
    say "$instance: release $n copied to $dir/releases/$n"
    (
        lock_installation "$instance"
        [[ -n $previous ]] && systemctl stop "$unit_name$instance"
        if ! admit_release "$instance" "$n"; then
            if [[ -n $previous ]]; then
                admit_release "$instance" "$previous" || exit 1
                systemctl start "$unit_name$instance"
            fi
            fail "release $n does not satisfy this instance's installation policy" "use an isolated installation, or prepare a code-free release; a first release must be code-free"
        fi
        if ! with_env "$instance" bilibi-build "$dir/releases/$n/bilibi-web" migrate ||
            ! with_env "$instance" bilibi-build "$dir/releases/$n/bilibi-web" seed-production; then
            if [[ -n $previous ]]; then
                admit_release "$instance" "$previous" || exit 1
                systemctl start "$unit_name$instance"
            fi
            fail "release $n could not update the database" "see the output above; the previous release is back in service" MIGRATION_FAILED
        fi
        put_in_service "$instance" "$n" "$previous" ||
            fail "release $n did not start" "read journalctl -u $unit_name$instance; any previous release is back in service. Fix the cause and rerun the same command" UNHEALTHY
    )
    if [[ -z $previous ]]; then
        say "Next: set up outgoing mail with $0 mail $instance ..., then send the owner's claim link with: $0 create-owner $instance --email <owner address> --company-name <organisation>"
    fi
}
# ================================================================ mail
# plain_value <text>: no single quote and no line break, so the value can be
# written single-quoted into the env file.
plain_value() { [[ $1 != *"'"* && $1 != *$'\n'* ]]; }
# The SMTP relay a Staging or Production instance sends mail through (the
# owner's claim link first). Written into the instance's env; the password
# comes on standard input so it never enters shell history or the process
# list. Rerunning replaces the settings and restarts a running service.
mail() {
    local instance=${1:-} host='' port='' username='' tls='' from_address='' from_name='Bilibi' password_stdin='' password=''
    [[ -n $instance ]] || usage
    shift
    while (($#)); do
        case $1 in
            --host) host=${2:-}; shift 2 ;;
            --port) port=${2:-}; shift 2 ;;
            --username) username=${2:-}; shift 2 ;;
            --tls) tls=${2:-}; shift 2 ;;
            --from-address) from_address=${2:-}; shift 2 ;;
            --from-name) from_name=${2:-}; shift 2 ;;
            --password-stdin) password_stdin=1; shift ;;
            *) usage ;;
        esac
    done
    [[ $host =~ ^[A-Za-z0-9.-]+$ ]] || fail '--host must be the relay host name, like smtp.example.com' '' INVALID_INPUT
    [[ $port =~ ^[0-9]+$ ]] || fail '--port must be the relay port, usually 587 or 465' '' INVALID_INPUT
    [[ $tls == starttls || $tls == implicit_tls ]] || fail '--tls must be starttls (port 587) or implicit_tls (port 465)' '' INVALID_INPUT
    [[ -n $username ]] && plain_value "$username" || fail '--username is required and cannot contain quotes or line breaks' '' INVALID_INPUT
    [[ $from_address =~ ^[^@[:space:]\']+@[^@[:space:]\']+$ ]] || fail '--from-address must be an email address' '' INVALID_INPUT
    plain_value "$from_name" || fail '--from-name cannot contain quotes or line breaks' '' INVALID_INPUT
    [[ -n $password_stdin ]] || fail 'the relay password comes on standard input' 'pipe it to this command and add --password-stdin' INVALID_INPUT
    need_root mail "$instance"
    instance_exists "$instance" || fail "no instance named $instance" "create it first: $0 create $instance --role production --site <host>" NO_INSTANCE
    IFS= read -r password || true
    [[ -n $password ]] && plain_value "$password" || fail 'the relay password on standard input is empty or contains a quote or line break' 'pipe the password on one line' INVALID_INPUT
    local dir env env_new
    dir=$(instance_dir "$instance"); env=$dir/env
    umask 027
    env_new=$(mktemp "$dir/.env.XXXXXX")
    grep -v '^MAIL_' "$env" >"$env_new" || true
    # Single quotes: read the same way by bash and systemd's EnvironmentFile.
    printf "MAIL_HOST='%s'\nMAIL_PORT='%s'\nMAIL_USERNAME='%s'\nMAIL_PASSWORD='%s'\nMAIL_TLS_MODE='%s'\nMAIL_FROM_NAME='%s'\nMAIL_FROM_ADDRESS='%s'\n" \
        "$host" "$port" "$username" "$password" "$tls" "$from_name" "$from_address" >>"$env_new"
    chown root:bilibi "$env_new"; chmod 0640 "$env_new"
    mv -T "$env_new" "$env"
    umask 022
    if [[ $(service_state "$instance") == active ]]; then
        systemctl restart "$unit_name$instance"
        wait_healthy "$instance" || fail "$instance did not start with the new mail settings" "read journalctl -u $unit_name$instance, correct the settings and rerun" UNHEALTHY
    fi
    say "$instance: mail goes out through $host:$port as $from_address"
}
# ================================================================ create-owner
# The first account of a fresh instance: `bilibi-web create-owner` in the
# instance's environment. No password passes through here: the owner gets an
# emailed claim link, chooses their own password and sets up 2FA. Safe to
# repeat; the last line is `Owner claim: <state>`.
create_owner() {
    local instance=${1:-}
    [[ -n $instance ]] || usage
    need_root create-owner "$@"
    shift
    instance_exists "$instance" || fail "no instance named $instance" "create it first: $0 create $instance --role production --site <host>" NO_INSTANCE
    local dir; dir=$(instance_dir "$instance")
    [[ -x $dir/current/bilibi-web ]] || fail "$instance has no release in service" "put one in service first: $0 release $instance <setup archive> --repo <owner/name> [--repo-id <n>] --tag <setup-vX.Y.Z>" NO_RELEASE
    with_env "$instance" bilibi "$dir/current/bilibi-web" create-owner "$@"
}
# =========================================================== tenant-provision
# A customer tenant, primary company and first-owner invitation.
tenant_provision() {
    local instance=${1:-}
    [[ -n $instance ]] || usage
    shift
    local args=("$@")
    local -A options=()
    while (( $# )); do
        (( $# >= 2 )) || usage
        case $1 in
            --tenant-name|--company-name|--company-code|--owner-email) ;;
            *) usage ;;
        esac
        [[ ! ${options[$1]+present} ]] || fail "Repeated option: $1"
        options[$1]=$2
        shift 2
    done
    [[ -n ${options[--owner-email]:-} ]] || usage
    need_root tenant-provision "$instance" "${args[@]}"
    instance_exists "$instance" || fail "no instance named $instance" "create it first: $0 create $instance --role production --site <host>" NO_INSTANCE
    local dir; dir=$(instance_dir "$instance")
    [[ -x $dir/current/bilibi-web ]] || fail "$instance has no release in service" "put one in service first: $0 release $instance <setup archive> --repo <owner/name> [--repo-id <n>] --tag <setup-vX.Y.Z>" NO_RELEASE
    with_env "$instance" bilibi "$dir/current/bilibi-web" tenant-provision "${args[@]}"
}
# ========================================================= two-factor-reset
# Turns off one account's authenticator, for a person nobody else can reset
# (the only admin lost their phone): `bilibi-web two-factor-reset` in the
# instance's environment. Audited as the system principal.
two_factor_reset() {
    local instance=${1:-}
    [[ -n $instance ]] || usage
    need_root two-factor-reset "$@"
    shift
    instance_exists "$instance" || fail "no instance named $instance" "create it first: $0 create $instance --role production --site <host>" NO_INSTANCE
    local dir; dir=$(instance_dir "$instance")
    [[ -x $dir/current/bilibi-web ]] || fail "$instance has no release in service" "put one in service first: $0 release $instance <setup archive> --repo <owner/name> [--repo-id <n>] --tag <setup-vX.Y.Z>" NO_RELEASE
    with_env "$instance" bilibi "$dir/current/bilibi-web" two-factor-reset "$@"
}
# ========================================================= identity lifecycle
identity_lifecycle() {
    local command=${1:-} instance=${2:-}
    [[ -n $command && -n $instance ]] || usage
    shift 2
    need_root "$command" "$instance" "$@"
    instance_exists "$instance" || fail "no instance named $instance"
    local dir; dir=$(instance_dir "$instance")
    [[ -x $dir/current/bilibi-web ]] || fail "$instance has no release in service" "put one in service first: $0 release $instance <setup archive> --repo <owner/name> [--repo-id <n>] --tag <setup-vX.Y.Z>"
    with_env "$instance" bilibi-build "$dir/current/bilibi-web" "$command" "$@"
}
switch() {
    local instance=${1:-} n=${2:-}
    [[ -n $instance && $n =~ ^[0-9]+$ ]] || usage
    need_root switch "$@"
    instance_exists "$instance" || fail "no instance named $instance" "create it first: $0 create $instance --role production --site <host>" NO_INSTANCE
    local dir; dir=$(instance_dir "$instance")
    [[ -x $dir/releases/$n/bilibi-web ]] || fail "$instance has no release $n" "list them: $0 status $instance"
    (cd "$dir/releases/$n" && sha256sum -c --quiet SHA256SUMS) || fail "release $n does not match its checksum"
    # An older release may lack migrations the newer one applied; the database
    # keeps them (migrations are forward-only). Going back is the Software page's
    # Undo, which also restores a backup when tables changed.
    (
        lock_installation "$instance"
        put_in_service "$instance" "$n" "$(current_release "$instance")"
    )
}
status() {
    local instance list
    if run_in_progress; then
        say "A run is in progress: wait until this line is gone, then rerun the step it was doing."
    fi
    if [[ -n ${1:-} ]]; then list=$1; else list=$(instances); fi
    [[ -n $list ]] || { say "No instances under $root yet."; return; }
    python3 "$here/lib/github-app.py" show "$github_app" || true
    for instance in $list; do
        instance_exists "$instance" || fail "no instance named $instance" "create it first: $0 create $instance --role production --site <host>" NO_INSTANCE
        local dir; dir=$(instance_dir "$instance")
        local role mail_host mail
        role=$(role_of "$instance")
        mail_host=$(env_value "$instance" MAIL_HOST | tr -d "'")
        if [[ -n $mail_host ]]; then mail="via $mail_host"; elif [[ $role == dev ]]; then mail="to the Dev mailbox"; else mail="not set up"; fi
        printf '%s: role %s, %s, service %s, installer %s, mail %s, current release %s\n' "$instance" "$role" \
            "$(env_value "$instance" APP_ORIGIN)" "$(service_state "$instance")" \
            "$(installer_state "$instance")" "$mail" "$(current_release "$instance")"
        [[ -z $(env_value "$instance" PUBLIC_ORIGIN) ]] || printf '  public site  %s\n' "$(env_value "$instance" PUBLIC_ORIGIN)"
        local r
        for r in $(release_numbers "$instance"); do
            printf '  release %s  program %s\n' "$r" "$(awk '$2 == "bilibi-web" { print substr($1, 1, 12) }' "$dir/releases/$r/SHA256SUMS" 2>/dev/null)"
        done
    done
}

# JSON mode runs one operation as a step and prints one JSON object per line:
# {"step","state":"started"}, then {"step","state":"completed"|"failed",
# "code","next","claim","detail"}. `code` is OK or a stable name from
# `fail` (the list is in docs/install-with-ai/SKILL.md), `next` is the last
# "Next:" line, `claim` is create-owner's owner claim state. Child output goes
# into `detail`, never straight to stdout, so every line parses.
json_command() {
    local step=${1:-} output result
    case $step in
        create|release|installer|mail|create-owner|switch|status) ;;
        *) printf '%s\n' '{"step":"input","state":"failed","code":"INVALID_COMMAND","next":"Use create, release, installer, mail, create-owner, switch or status."}'; return 2 ;;
    esac
    if ! command -v python3 >/dev/null; then
        printf '%s\n' '{"step":"preflight","state":"failed","code":"PYTHON_MISSING","next":"Install Python 3 and retry."}'
        return 1
    fi
    output=$(mktemp) || return 1
    chmod 600 "$output"
    printf '{"step":"%s","state":"started"}\n' "$step"
    if "$0" "$@" >"$output" 2>&1; then result=0; else result=$?; fi
    python3 - "$step" "$result" "$output" <<'PYJSON'
import json, pathlib, sys
step, result, output = sys.argv[1:]
detail = pathlib.Path(output).read_text(encoding="utf-8", errors="replace")
lines = detail.splitlines()
def last(prefix):
    found = [line[len(prefix):].strip() for line in lines if line.startswith(prefix)]
    return found[-1] if found else None
failed = result != "0"
code = "OK" if not failed else (last("Code:") or ("USAGE" if result == "2" else step.upper().replace("-", "_") + "_FAILED"))
record = {"step": step, "state": "failed" if failed else "completed", "code": code,
          "next": last("Next:") or ("Read the detail, correct the problem, and rerun the same command." if failed else None)}
claim = last("Owner claim:")
if claim:
    record["claim"] = claim
record["detail"] = detail
print(json.dumps(record))
PYJSON
    rm -f -- "$output"
    return "$result"
}

if [[ ${1:-} == --json ]]; then
    shift
    json_command "$@"
    exit $?
fi

# One run at a time. A dropped or killed client does not stop a run (its
# children keep going), so a rerun waits for the earlier one instead of
# racing it on packages, the database or the release links.
run_lock=/run/bilibi-install-server.lock
run_in_progress() { [[ $(id -u) -eq 0 && -e $run_lock ]] && ! flock -n "$run_lock" true; }
hold_run_lock() {
    [[ $(id -u) -eq 0 ]] || return 0
    exec {run_lock_fd}>>"$run_lock"
    if ! flock -n "$run_lock_fd"; then
        say "Another run is still working; waiting for it to finish."
        flock "$run_lock_fd"
    fi
}

detect_os
case ${1:-} in
    create|release|mail|create-owner|tenant-provision|two-factor-reset|identity-recover|identity-delete|identity-retention|installer|github-app|switch) hold_run_lock ;;
esac
case ${1:-} in
    create) shift; create "$@" ;;
    release) shift; release "$@" ;;
    mail) shift; mail "$@" ;;
    create-owner) shift; create_owner "$@" ;;
    tenant-provision) shift; tenant_provision "$@" ;;
    two-factor-reset) shift; two_factor_reset "$@" ;;
    identity-recover|identity-delete|identity-retention) command=$1; shift; identity_lifecycle "$command" "$@" ;;
    installer) shift; installer "$@" ;;
    github-app) shift; github_app_command "$@" ;;
    verify) shift; verify_command "$@" ;;
    switch) shift; switch "$@" ;;
    status) shift; status "$@" ;;
    *) usage ;;
esac
