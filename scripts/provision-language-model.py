#!/usr/bin/env python3
"""Explicit deployment-only provisioning of the pinned fastText asset."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import tempfile
import urllib.request

MODULE = Path(__file__).resolve().parent.parent / 'apps/base/language_detection'


def provision(directory):
    asset = json.loads((MODULE / 'model.json').read_text())
    directory.mkdir(parents=True, exist_ok=True)
    target = directory / asset['filename']
    if target.exists():
        with target.open('rb') as source:
            digest = hashlib.file_digest(source, 'sha256').hexdigest()
        if target.stat().st_size != asset['bytes'] or digest != asset['sha256']:
            raise ValueError(f'{target} is not the pinned model; move it aside explicitly before provisioning')
    else:
        # Atomic publication without overwriting an existing deployment asset.
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(dir=directory, delete=False) as output:
                temporary = Path(output.name)
                digest = hashlib.sha256()
                size = 0
                with urllib.request.urlopen(asset['url'], timeout=60) as response:
                    if not response.geturl().startswith('https://'):
                        raise ValueError('model download redirected away from HTTPS')
                    while chunk := response.read(1024 * 1024):
                        size += len(chunk)
                        if size > asset['bytes']:
                            raise ValueError('model download exceeds the pinned size')
                        digest.update(chunk)
                        output.write(chunk)
                if size != asset['bytes'] or digest.hexdigest() != asset['sha256']:
                    raise ValueError('model size or SHA-256 does not match the pinned asset')
                output.flush()
                os.fsync(output.fileno())
            temporary.chmod(0o644)
            os.link(temporary, target)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
    shutil.copyfile(MODULE / 'NOTICE.md', directory / 'NOTICE.md')
    shutil.copyfile(MODULE / 'model.json', directory / 'model.json')
    print(f"{asset['version']}: {target} ({asset['bytes']} bytes), SHA-256 {asset['sha256']}")
    print('Model unchanged. Attribution and license: ' + str(directory / 'NOTICE.md'))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path, help='explicit deployment directory for this asset version')
    args = parser.parse_args()
    try:
        provision(args.directory.resolve())
    except (OSError, ValueError) as error:
        parser.exit(1, f'Language model provisioning failed: {error}\n')


if __name__ == '__main__':
    main()
