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
        for image, bundles in [('initramfs', {'bundles/base', 'bundles/cli-utils'}),
                               ('rootfs', {'bundles/full'})]:
            layers = manifest['images'][image]['layers']
            self.assertEqual(layers[0], {'kind': 'bundle', 'path': 'bundles/sgfx-maxwell.toml'})
            upstream = [layer for layer in layers if isinstance(layer.get('source'), dict)]
            self.assertEqual({layer['subdir'] for layer in upstream}, bundles)
            for layer in upstream:
                self.assertEqual(layer['kind'], 'bundle')
                self.assertEqual(layer['source']['git'], 'https://github.com/petitstrawberry/Scarlet')
                self.assertEqual(len(layer['source']['rev']), 40)
        self.assertNotIn('.scarlet/sources', (PROJECT / 'scarlet.toml').read_text())

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
        self.assertTrue(layers[0]['replace'])
        self.assertEqual(layers[0]['features'], ['h264-stateless-hw', 'mp4-aac'])


if __name__ == '__main__':
    unittest.main()
