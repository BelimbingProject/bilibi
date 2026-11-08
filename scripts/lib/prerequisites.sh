# shellcheck shell=bash
# Local development prerequisites.
# shellcheck source=scripts/lib/versions.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/versions.sh"

# A parent Cargo or make advertises its jobserver in these variables as inherited
# pipe descriptors. A launcher that closes those descriptors (Python Popen, a
# cargo-watch shell) leaves Cargo printing "failed to connect to jobserver".
# Call this first in every launcher path that starts cargo; Cargo then makes its own.
clear_stale_jobserver() {
    unset CARGO_MAKEFLAGS MAKEFLAGS MFLAGS
}

activate_cargo() {
    clear_stale_jobserver
    # shellcheck disable=SC1091
    if [[ -f $HOME/.cargo/env ]]; then source "$HOME/.cargo/env"; fi
}

postgres_bin() {
    if [[ -n ${PG18_BIN:-} ]]; then
        printf '%s\n' "$PG18_BIN"
    elif [[ ! -x /usr/lib/postgresql/18/bin/initdb && -x /usr/bin/initdb ]]; then
        echo /usr/bin
    else
        echo /usr/lib/postgresql/18/bin
    fi
}

development_prerequisites() {
    local tool current pinned pg_bin
    for tool in git python3 curl ss flock openssl cc pkg-config rustup rustc cargo psql pg_isready node npm; do
        command -v "$tool" >/dev/null || { echo "Missing prerequisite: $tool" >&2; return 1; }
    done
    python3 -c 'import tomllib' 2>/dev/null || { echo 'Python 3.11+ is required. See README.md (supported environment).' >&2; return 1; }
    # shellcheck disable=SC2154 # root is set by the sourcing script
    pinned=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$root/rust-toolchain.toml")
    current=$(rustc --version | awk '{print $2}') || return 1
    [[ $current == "$pinned" ]] || { echo "Rust $pinned is required, found $current. Run ./bilibi setup." >&2; return 1; }
    for tool in 'cargo --version' 'cargo clippy --version' 'cargo fmt --version' 'cargo watch --version' \
        'git --version' 'curl --version' 'ss -V' 'flock --version' 'openssl version' \
        'cc --version' 'psql --version' 'pg_isready --version' 'npm --version'; do
        # Each word here is a fixed command, never request input.
        $tool >/dev/null 2>&1 || { echo "Prerequisite failed: $tool" >&2; return 1; }
    done
    pkg-config --exists openssl || { echo 'OpenSSL development libraries are required.' >&2; return 1; }
    node -e "process.exit(Number(process.versions.node.split('.')[0]) < $NODE_MIN_MAJOR ? 1 : 0)" || { echo "Node.js $NODE_MIN_MAJOR+ is required." >&2; return 1; }
    if [[ -z ${DATABASE_URL:-} ]]; then
        pg_bin=$(postgres_bin)
        [[ $("$pg_bin/initdb" --version 2>/dev/null | awk '{print $NF}' | cut -d. -f1) == "$POSTGRESQL_MAJOR" ]] &&
            [[ $("$pg_bin/pg_ctl" --version 2>/dev/null | awk '{print $NF}' | cut -d. -f1) == "$POSTGRESQL_MAJOR" ]] || {
            echo "PostgreSQL $POSTGRESQL_MAJOR binaries are required at $pg_bin (or set DATABASE_URL)." >&2; return 1;
        }
    fi
}
