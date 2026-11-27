#!/usr/bin/env python3
"""Check that Switch manifests use cargo-scarlet source declarations."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / 'projects/aarch64-switch-l4t'


class SourceTests(unittest.TestCase):
    def test_sources_are_owned_by_cargo_scarlet(self):
        self.assertFalse((ROOT / 'scripts/project_sources.py').exists())
        self.assertFalse((ROOT / 'source-pins.toml').exists())
        manifest = tomllib.loads((PROJECT / 'scarlet.toml').read_text())
        source = manifest['bsp']['kernel']['source']
        self.assertEqual(source['git'], 'https://github.com/petitstrawberry/Scarlet')
        self.assertRegex(source['rev'], r'^[0-9a-f]{40}$')
        for image, bundles in [('initramfs', {'base', 'cli-utils', 'wireless', 'bluetooth'}),
                               ('rootfs', {'full'})]:
            layers = manifest['images'][image]['layers']
            self.assertEqual(layers[0], {'kind': 'bundle', 'path': 'bundles/sgfx-maxwell.toml'})
            upstream = [layer for layer in layers
                        if isinstance(layer.get('source'), dict)
                        and layer['source'].get('git') == source['git']]
            self.assertEqual({layer['subdir'] for layer in upstream},
                             {f'bundles/{b}' for b in bundles})
            self.assertTrue(all(layer['source'] == source for layer in upstream))
            self.assertIn({'kind':'bundle','path':'bundles/wifi-firmware.toml'}, layers)
            self.assertIn({'kind': 'bundle', 'path': 'bundles/bluetooth-firmware.toml'}, layers)
        self.assertNotIn('.scarlet/sources', (PROJECT / 'scarlet.toml').read_text())
        self.assertNotIn('../../../Scarlet', (PROJECT / 'scarlet.toml').read_text())
        self.assertNotIn('Scarlet/kernel', (PROJECT / 'bsp/Cargo.toml').read_text())

    def test_bluetooth_patch_import_and_bundle_reject_corruption(self):
        # Run the real CLI and bundle entry point with disposable source data,
        # so CI can exercise both checks without distributing the radio patch.
        bundle_dir = PROJECT / 'bundles'
        layer, = tomllib.loads((bundle_dir / 'bluetooth-firmware.toml').read_text())['layers']
        self.assertEqual((layer['kind'], layer['to']), ('script', '/lib/firmware/brcm'))
        installer = (bundle_dir / layer['source']).resolve()
        pins = json.loads((PROJECT / 'bluetooth-firmware.json').read_text())
        name, = pins['files']
        self.assertEqual(name, 'BCM4356A3.hcd')
        data = b'fixture patch'
        pins['files'][name] = {'size': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            scripts = root / 'scripts'
            scripts.mkdir()
            importer = scripts / 'import-wifi-firmware.py'
            for src in (ROOT / 'scripts' / importer.name, ROOT / 'scripts/install-wifi-firmware.py', installer):
                shutil.copy2(src, scripts / src.name)
            project = root / PROJECT.relative_to(ROOT)
            project.mkdir(parents=True)
            (project / 'bluetooth-firmware.json').write_text(json.dumps(pins))
            source = root / 'extracted'
            source.mkdir()
            source_file = source / name
            source_file.write_bytes(data)
            run_import = [sys.executable, str(importer), '--radio', 'bluetooth', '--directory', str(source)]
            subprocess.run(run_import, check=True, capture_output=True)
            output = root / 'image' / layer['to'].lstrip('/')
            run_install = [scripts / installer.name, output]
            subprocess.run(run_install, check=True, capture_output=True)
            self.assertEqual((output / name).read_bytes(), data)

            imported = project / 'firmware/bluetooth' / name
            corrupt = bytes(len(data))
            source_file.write_bytes(corrupt)
            bad = subprocess.run(run_import, capture_output=True)
            self.assertNotEqual(bad.returncode, 0)
            self.assertIn(b'mismatched firmware', bad.stderr)
            self.assertEqual(imported.read_bytes(), data)
            imported.write_bytes(corrupt)
            bad = subprocess.run(run_install, capture_output=True)
            self.assertNotEqual(bad.returncode, 0)
            self.assertIn(b'firmware hash mismatch', bad.stderr)
            self.assertEqual((output / name).read_bytes(), data)

    def test_gpu_firmware_is_prepared_by_the_image(self):
        manifest = tomllib.loads((PROJECT / 'scarlet.toml').read_text())
        for image in ('initramfs', 'rootfs'):
            self.assertIn({'kind': 'bundle', 'path': 'bundles/gpu-firmware.toml'}, manifest['images'][image]['layers'])
        layer = tomllib.loads((PROJECT / 'bundles/gpu-firmware.toml').read_text())['layers'][0]
        self.assertEqual(layer['kind'], 'script')
        self.assertTrue((PROJECT / 'bundles' / layer['source']).resolve().is_file())

    def test_video_override_uses_a_standard_git_source(self):
        layers = tomllib.loads((PROJECT / 'bundles/nvdec-player.toml').read_text())['layers']
        self.assertEqual(len(layers), 1)
        self.assertEqual(layers[0]['subdir'], 'user/video_player')
        self.assertEqual(layers[0]['source']['git'], 'https://github.com/petitstrawberry/Scarlet')
        manifest = tomllib.loads((PROJECT / 'scarlet.toml').read_text())
        self.assertEqual(layers[0]['source'], manifest['bsp']['kernel']['source'])
        self.assertTrue(layers[0]['replace'])
        self.assertEqual(layers[0]['features'], ['h264-stateless-hw', 'mp4-aac'])

    def test_driver_and_userspace_scarlet_dependencies_share_the_kernel_revision(self):
        manifest = tomllib.loads((PROJECT / 'scarlet.toml').read_text())
        source = manifest['bsp']['kernel']['source']
        checked = 0
        for directory in ('drivers', 'userspace', 'tests'):
            for path in (ROOT / directory).rglob('Cargo.toml'):
                if any(part in ('target', '.scarlet') for part in path.parts):
                    continue
                for line in path.read_text().splitlines():
                    if source['git'] in line:
                        self.assertIn(source['rev'], line, str(path))
                        checked += 1
        self.assertGreater(checked, 20)


if __name__ == '__main__':
    unittest.main()
