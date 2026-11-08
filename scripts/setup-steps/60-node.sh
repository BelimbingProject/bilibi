#!/usr/bin/env bash
# Node.js, needed by Cargo to build embedded CSS (scripts/build-assets.sh).
set -euo pipefail
# shellcheck source=scripts/lib/install.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/../lib/install.sh"

major=$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null || echo 0)
if ((major == NODE_MAJOR)) && command -v npm >/dev/null; then
    present node
    exit 0
fi
if ((major != 0 && major != NODE_MAJOR)); then
    fail node "Node.js $NODE_MAJOR is required, found $major" "select Node.js $NODE_MAJOR, then rerun ./bilibi setup"
fi
if [[ $SETUP_PACKAGE_MANAGER == pacman ]]; then
    # Arch's nodejs package follows Current; use the Node 24 LTS package.
    if pacman_install nodejs-lts-krypton npm; then installed node; else present node; fi
else
    apt_repo nodesource https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key \
        "deb [signed-by=@KEYRING@] https://deb.nodesource.com/node_${NODE_MAJOR}.x nodistro main"
    apt_install nodejs || true
    installed node
fi
major=$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null || echo 0)
((major == NODE_MAJOR)) || fail node "Node.js $NODE_MAJOR is required, found $major" "select Node.js $NODE_MAJOR, then rerun ./bilibi setup"
command -v npm >/dev/null || fail node 'npm is missing' 'install npm, then rerun ./bilibi setup'
