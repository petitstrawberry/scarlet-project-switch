#!/usr/bin/env python3
"""Check that Switch manifests use cargo-scarlet source declarations."""
from pathlib import Path
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / 'projects/aarch64-switch-l4t-console'


class SourceTests(unittest.TestCase):
    def test_sources_are_owned_by_cargo_scarlet(self):
        self.assertFalse((ROOT / 'scripts/project_sources.py').exists())
        self.assertFalse((ROOT / 'source-pins.toml').exists())
        manifest = tomllib.loads((PROJECT / 'scarlet.toml').read_text())
        source = manifest['bsp']['kernel']['source']
        self.assertEqual(source['git'], 'https://github.com/petitstrawberry/Scarlet')
        self.assertRegex(source['rev'], r'^[0-9a-f]{40}$')
        for image, bundles in [('initramfs', {'base', 'cli-utils', 'wireless'}),
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
        self.assertNotIn('.scarlet/sources', (PROJECT / 'scarlet.toml').read_text())
        self.assertNotIn('../../../Scarlet', (PROJECT / 'scarlet.toml').read_text())
        self.assertNotIn('Scarlet/kernel', (PROJECT / 'bsp/Cargo.toml').read_text())

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
