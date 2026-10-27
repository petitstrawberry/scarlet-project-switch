#!/usr/bin/env python3
"""Import hash-pinned Wi-Fi files from the local Switchroot rootfs or archive."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / 'projects/aarch64-switch-l4t-console'


def digest(path):
    result = hashlib.sha256()
    with path.open('rb') as stream:
        while block := stream.read(4 * 1024 * 1024):
            result.update(block)
    return result.hexdigest()


def import_files(directory, pins):
    destination = PROJECT / 'firmware/wifi'
    for name, expected in pins['files'].items():
        source = directory / name
        if not source.is_file() or source.stat().st_size != expected['size'] or digest(source) != expected['sha256']:
            raise ValueError(f'missing or mismatched firmware: {name}')
        target = destination / name
        if target.exists() and digest(target) != expected['sha256']:
            raise ValueError(f'refusing to replace a different local firmware: {target}')
    destination.mkdir(parents=True, exist_ok=True)
    for name in pins['files']:
        if (directory / name).resolve() != (destination / name).resolve():
            shutil.copyfile(directory / name, destination / name)
        if digest(destination / name) != pins['files'][name]['sha256']:
            raise ValueError(f'import readback mismatch: {name}')
    print(f'Imported and verified {len(pins["files"])} Wi-Fi files: {destination}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument('--directory', type=Path, help='directory containing the two pinned files')
    source.add_argument('--archive', type=Path, help='original Switchroot archive; requires tar and debugfs')
    parser.add_argument('--debugfs', default='debugfs')
    args = parser.parse_args()
    pins = json.loads((PROJECT / 'wifi-firmware.json').read_text())
    if args.directory:
        import_files(args.directory, pins)
        return
    if digest(args.archive) != pins['archive_sha256']:
        raise ValueError('Switchroot archive hash mismatch')
    cache = ROOT / '.cache'
    cache.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='wifi-firmware-', dir=cache) as temporary:
        work = Path(temporary)
        image = work / 'root.ext4'
        with image.open('wb') as stream:
            for part in pins['rootfs_parts']:
                subprocess.run(['tar', '-xOf', str(args.archive), part], stdout=stream, check=True)
        for name in pins['files']:
            subprocess.run([args.debugfs, '-R', f'dump {pins["source_directory"]}/{name} {work / name}',
                            str(image)], check=True, capture_output=True)
        import_files(work, pins)


if __name__ == '__main__':
    main()
