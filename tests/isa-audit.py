#!/usr/bin/env python3
"""Regression checks for the Cortex-A57 disassembly audit."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('isa', Path(__file__).with_name('check-isa.py'))
isa = importlib.util.module_from_spec(spec)
spec.loader.exec_module(isa)

OUTLINE = '''0000000000224de8 <__aarch64_cas8_acq_rel>:
  224de8: adrp x16, 0x235000
  224dec: ldrb w16, [x16, #0xdf8]
  224df0: cbz w16, 0x224dfc <__aarch64_cas8_acq_rel+0x14>
  224df4: casal x0, x1, [x2]
  224df8: ret
  224dfc: mov x16, x0
  224e00: ldaxr x0, [x2]
  224e04: cmp x0, x16
  224e08: b.ne 0x224e14
  224e0c: stlxr w17, x1, [x2]
  224e10: cbnz w17, 0x224e00
  224e14: ret
'''

class AuditTests(unittest.TestCase):
    def test_userspace_outline_with_fallback(self):
        self.assertEqual(isa.audit_disassembly(OUTLINE, True), (12, 1))

    def test_kernel_must_remain_lse_free(self):
        with self.assertRaises(ValueError): isa.audit_disassembly(OUTLINE)

    def test_byte_and_halfword_fallbacks(self):
        for suffix, size in [('b', '1'), ('h', '2')]:
            asm = OUTLINE.replace('cas8', 'cas' + size)
            asm = asm.replace('casal ', 'casal' + suffix + ' ')
            asm = asm.replace('ldaxr ', 'ldaxr' + suffix + ' ')
            asm = asm.replace('stlxr ', 'stlxr' + suffix + ' ')
            self.assertEqual(isa.audit_disassembly(asm, True), (12, 1))

    def test_arbitrary_function_cannot_hide_lse(self):
        with self.assertRaises(ValueError): isa.audit_disassembly(OUTLINE.replace('__aarch64_cas8_acq_rel', 'main'), True)

    def test_wrong_branch_polarity(self):
        with self.assertRaises(ValueError): isa.audit_disassembly(OUTLINE.replace('cbz w16', 'cbnz w16'), True)

    def test_branch_into_lse_is_not_a_fallback(self):
        with self.assertRaises(ValueError): isa.audit_disassembly(OUTLINE.replace('w16, 0x224dfc', 'w16, 0x224df4'), True)

    def test_missing_store_exclusive_is_rejected(self):
        with self.assertRaises(ValueError): isa.audit_disassembly(OUTLINE.replace('stlxr', 'str'), True)

if __name__ == '__main__': unittest.main()
