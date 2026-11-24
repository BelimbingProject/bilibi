#!/usr/bin/env bash
set -euo pipefail
[[ $(cat /run/systemd/container) == systemd-nspawn ]]

for instance in dev production; do
    dir=/opt/bilibi/$instance
    app=$(systemctl show "bilibi@$instance" -p User --value)
    worker=$(systemctl show "bilibi-installer@$instance" -p User --value)
    other=production
    [[ $instance != production ]] || other=dev
    [[ $app != "$worker" && $app != nobody && $worker != nobody ]]
    [[ $app != "$(systemctl show "bilibi@$other" -p User --value)" ]]
    [[ $worker != "$(systemctl show "bilibi-installer@$other" -p User --value)" ]]
    [[ $(stat -c '%U:%G:%a' "$dir/trust") == root:bilibi:2750 ]]
    [[ $(stat -c '%U:%G:%a' "$dir/installer") == root:root:755 ]]
    for identity in "$app" "$worker"; do
        for target in "$instance" "$other"; do
            trust=/opt/bilibi/$target/trust
            runuser -u "$identity" -- test ! -w "$trust"
            for file in release.json release-state.json; do
                [[ $(stat -c '%U:%G:%a' "$trust/$file") == root:bilibi:640 ]]
                runuser -u "$identity" -- test -r "$trust/$file"
                runuser -u "$identity" -- test ! -w "$trust/$file"
            done
        done
        runuser -u "$identity" -- test ! -r "/opt/bilibi/$other/env"
    done
    command=(/usr/bin/python3 -I /opt/bilibi/release-proof.py write-instance)
    runuser -u "$worker" -- sudo -n -l "${command[@]}" "$instance" >/dev/null
    if runuser -u "$worker" -- sudo -n -l "${command[@]}" "$other" >/dev/null 2>&1 ||
       runuser -u "$app" -- sudo -n -l "${command[@]}" "$instance" >/dev/null 2>&1; then
        echo 'Unexpected trust write authority' >&2
        exit 1
    fi
done
printf '%s\n' 'Instance trust and replay are root-owned and sudo grants are instance-scoped.'
