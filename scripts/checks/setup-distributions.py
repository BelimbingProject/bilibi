#!/usr/bin/env python3
"""Exercise setup's package selection without changing the host system."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
with tempfile.TemporaryDirectory(prefix="bilibi-setup-check-") as tmp:
    root = Path(tmp)
    shutil.copytree(ROOT / "scripts/lib", root / "scripts/lib")
    shutil.copytree(ROOT / "scripts/setup-steps", root / "scripts/setup-steps")
    release = root / "os-release"
    install = root / "scripts/lib/install.sh"
    install.write_text(install.read_text().replace('/etc/os-release', str(release)))
    fake = root / "bin"
    fake.mkdir()
    log = root / "calls"
    scripts = {
        "id": 'echo 0',
        "pacman": '''echo "pacman $*" >> "$CALLS"
case "$1" in
    -Q) [[ ${PACMAN_INSTALLED:-0} == 1 ]] ;;
    -Si) echo "Version : ${PACMAN_VERSION:-18.6-1}" ;;
    -Syu) [[ ${PACMAN_FAIL:-0} != 1 ]] ;;
esac''',
        "apt-get": 'echo "apt-get $*" >> "$CALLS"',
        "dpkg-query": 'exit 1',
        "postgres": "echo 'postgres (PostgreSQL) 18.6'",
    }
    for name, body in scripts.items():
        path = fake / name
        path.write_text("#!/usr/bin/env bash\n" + body + "\n")
        path.chmod(0o755)
    env = dict(os.environ, PATH=f"{fake}:{os.environ['PATH']}", CALLS=str(log))

    def run(step):
        return subprocess.run(["bash", str(root / "scripts/setup-steps" / step)], env=env,
                              text=True, capture_output=True, check=True)

    release.write_text('ID=omarchy\nID_LIKE=arch\n')
    run('10-packages.sh')
    run('30-postgresql.sh')
    calls = log.read_text()
    assert 'pacman -Syu --needed --noconfirm' in calls
    assert 'base-devel' in calls and 'pkgconf' in calls and 'postgresql' in calls
    assert 'apt-get ' not in calls

    log.write_text('')
    env['PACMAN_INSTALLED'] = '1'
    run('10-packages.sh')
    run('30-postgresql.sh')
    assert 'pacman -Syu' not in log.read_text()
    del env['PACMAN_INSTALLED']

    env['PACMAN_VERSION'] = '19.0-1'
    result = subprocess.run(['bash', str(root / 'scripts/setup-steps/30-postgresql.sh')],
                            env=env, text=True, capture_output=True)
    assert result.returncode != 0 and 'not 18' in result.stderr
    del env['PACMAN_VERSION']
    env['PACMAN_FAIL'] = '1'
    result = subprocess.run(['bash', str(root / 'scripts/setup-steps/10-packages.sh')],
                            env=env, text=True, capture_output=True)
    assert result.returncode != 0 and 'pacman could not install' in result.stderr
    del env['PACMAN_FAIL']

    log.write_text('')
    release.write_text('ID=ubuntu\nID_LIKE=debian\n')
    run('10-packages.sh')
    calls = log.read_text()
    assert 'apt-get update -qq' in calls
    assert 'apt-get install -y -qq --no-install-recommends' in calls
    assert 'build-essential' in calls and 'libssl-dev' in calls
    assert 'pacman ' not in calls

    release.write_text('ID=unsupported\n')
    result = subprocess.run(['bash', str(root / 'scripts/setup-steps/10-packages.sh')],
                            env=env, text=True, capture_output=True)
    assert result.returncode != 0 and 'unsupported is not supported' in result.stderr
    # The same lookup feeds both the front door and the launcher.
    debian_bin = root / 'debian-pg'
    arch_bin = root / 'arch-bin'
    debian_bin.mkdir()
    arch_bin.mkdir()
    prereqs = root / 'scripts/lib/prerequisites.sh'
    prereqs.write_text(prereqs.read_text().replace('/usr/lib/postgresql/18/bin', str(debian_bin))
                       .replace('/usr/bin', str(arch_bin)))
    def pg_bin():
        return subprocess.check_output(['bash', '-c', 'source "$1"; postgres_bin', '_', str(prereqs)],
                                       env=env, text=True).strip()
    env.pop('PG18_BIN', None)
    assert pg_bin() == str(arch_bin)
    initdb = debian_bin / 'initdb'
    initdb.write_text('#!/bin/sh\n')
    initdb.chmod(0o755)
    assert pg_bin() == str(debian_bin)
    env['PG18_BIN'] = '/custom/pg'
    assert pg_bin() == '/custom/pg'
    print('Setup distribution checks passed (Arch derivative, Ubuntu, unsupported).')
