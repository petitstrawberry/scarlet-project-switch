#!/usr/bin/env python3
"""Build a local-kernel RAM-only physical Wi-Fi integration test.

No device operations, SD writes, production pin changes or copied dependency
caches. Temporary fixture sources are removed after preserving build evidence.
"""
import argparse
import gzip
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile
ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / 'projects/aarch64-switch-l4t-console'

def cpio(entries):
    data = bytearray()
    for inode, (name, mode, payload) in enumerate(entries + [('TRAILER!!!', 0, b'')], 1):
        name = name.encode() + b'\0'
        fields = [inode, mode, 0, 0, 1, 0, len(payload), 0, 0, 0, 0, len(name), 0]
        data.extend(b'070701' + ''.join(f'{v:08x}' for v in fields).encode())
        data.extend(name); data.extend(b'\0' * (-len(data) % 4))
        data.extend(payload); data.extend(b'\0' * (-len(data) % 4))
    return data

def prepare(scarlet, output, target_dir, ssid=None, psk=None):
    if output.exists(): raise ValueError('Choose a fresh evidence directory')
    output.mkdir(parents=True)
    env = os.environ.copy()
    env['CARGO_HOME'] = str(PROJECT / '.scarlet/cache/cargo-home')
    env['CARGO_TARGET_DIR'] = str(target_dir)
    target_dir.mkdir(parents=True, exist_ok=True)
    def run(args, name, cwd=ROOT, env=env):
        with (output / name).open('w') as log:
            subprocess.run(args, cwd=cwd, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
    with tempfile.TemporaryDirectory(prefix='wifi-native-fixture-', dir=ROOT / '.cache') as temporary:
        work = Path(temporary); (work / 'src').mkdir(); (work / 'guest/src').mkdir(parents=True)
        for name in ['early.rs', 'entry.rs']: shutil.copyfile(PROJECT / 'bsp/src' / name, work / 'src' / name)
        shutil.copyfile(ROOT / 'tests/wireless-native/guest/src/main.rs', work / 'guest/src/main.rs')
        modules = {'soc': ROOT / 'drivers/soc/tegra210', 'pmic': ROOT / 'drivers/rtc/max77620',
                   'pcie': ROOT / 'drivers/pcie/tegra210', 'brcmfmac': ROOT / 'drivers/network/brcmfmac',
                   'rng': ROOT / 'drivers/random/tegra210-se'}
        packages = {'soc': 'scarlet-driver-tegra210', 'pmic': 'scarlet-driver-max77620', 'pcie': 'scarlet-driver-tegra210-pcie',
                    'brcmfmac': 'scarlet-driver-brcmfmac', 'rng': 'scarlet-driver-tegra210-se-rng'}
        deps = '\n'.join(f'{k} = {{ package = {json.dumps(packages[k])}, path = {json.dumps(str(v))} }}' for k,v in modules.items())
        (work / 'Cargo.toml').write_text(f'''[package]
name = "wifi-native-kernel"
version = "0.1.0"
edition = "2024"
[dependencies]
scarlet = {{ path = {json.dumps(str(scarlet / 'kernel'))}, default-features = false, features = ["linux-boot", "network", "user-fpu", "user-vector"] }}
fdt = "0.1.5"
font8x8 = {{ version = "0.3", default-features = false, features = ["unicode"] }}
{deps}
[patch."https://github.com/petitstrawberry/Scarlet"]
scarlet = {{ path = {json.dumps(str(scarlet / 'kernel'))} }}
[profile.release]
panic = "abort"
''')
        links = ' '.join(f'{name}::force_link();' for name in modules)
        (work / 'src/main.rs').write_text('#![no_std]\n#![no_main]\nmod early;\nmod scarlet_modules { pub use scarlet; pub fn force_link() { '+links+' } }\ninclude!("entry.rs");\n')
        target = json.loads((PROJECT / 'bsp/targets/aarch64-switch-none-elf.json').read_text())
        target.pop('pre-link-args', None); (work / 'aarch64-wifi-none.json').write_text(json.dumps(target))
        kernel_env = env.copy(); kernel_env['RUSTFLAGS'] = f'-C link-arg=-T{PROJECT / "bsp/lds/aarch64_linux.ld"} -C no-vectorize-loops -C no-vectorize-slp'
        run(['cargo', 'build', '--manifest-path', str(work / 'Cargo.toml'), '--release', '--target', str(work / 'aarch64-wifi-none.json'), '-Zunstable-options', '-Zbuild-std=core,alloc,compiler_builtins', '-Zbuild-std-features=compiler-builtins-mem'], 'kernel-build.log', env=kernel_env)
        elf = target_dir / 'aarch64-wifi-none/release/wifi-native-kernel'
        shutil.copyfile(elf, output / 'kernel.elf')
        subprocess.run(['llvm-objcopy', '-O', 'binary', str(elf), str(output / 'Image')], check=True)
        deps = '\n'.join(f'{name} = {{ path = {json.dumps(str(scarlet / "user/lib" / path))} }}' for name,path in [('scarlet-std','std'),('scarlet-os','scarlet-os')])
        (work / 'guest/Cargo.toml').write_text(f'[package]\nname="wifi-native-qa"\nversion="0.1.0"\nedition="2024"\n[workspace]\n[dependencies]\n{deps}\n[profile.release]\npanic="abort"\n')
        run(['cargo', 'build', '--manifest-path', str(work / 'guest/Cargo.toml'), '--release', '--target', 'aarch64-unknown-scarlet'], 'guest-build.log')
        if ssid is not None:
            (work / 'connect/src').mkdir(parents=True)
            shutil.copyfile(ROOT / 'tests/wireless-native/connect/src/main.rs', work / 'connect/src/main.rs')
            cdeps = deps + "\nscarlet-wpa = { path = " + json.dumps(str(scarlet / "user/lib/scarlet-wpa")) + ", features = [\"native\"] }\nzeroize = \"1\"\nscarlet-abi = { path = " + json.dumps(str(scarlet / "user/lib/scarlet-abi")) + " }"
            (work / 'connect/Cargo.toml').write_text(f'[package]\nname="wifi-connect-qa"\nversion="0.1.0"\nedition="2024"\n[workspace]\n[dependencies]\n{cdeps}\n[profile.release]\npanic="abort"\n')
            run(['cargo', 'build', '--manifest-path', str(work / 'connect/Cargo.toml'), '--release', '--target', 'aarch64-unknown-scarlet'], 'connect-build.log')
        run(['cargo', 'build', '--manifest-path', str(scarlet / 'user/bin/Cargo.toml'), '--no-default-features', '--bin', 'init', '--release', '--target', 'aarch64-unknown-scarlet'], 'init-build.log', cwd=scarlet)
        run(['cargo', 'build', '--manifest-path', str(scarlet / 'user/bin/Cargo.toml'), '--no-default-features', '--bin', 'netcfgd', '--release', '--target', 'aarch64-unknown-scarlet'], 'netcfgd-build.log', cwd=scarlet)
        run(['cargo', 'build', '--manifest-path', str(scarlet / 'user/scarlet-ld/Cargo.toml'), '--release', '--target', 'aarch64-unknown-scarlet'], 'loader-build.log', cwd=scarlet)
    spec = importlib.util.spec_from_file_location('package_l4t', PROJECT / 'tools/package_l4t.py')
    package = importlib.util.module_from_spec(spec); spec.loader.exec_module(package)
    package.validate_image((output / 'Image').read_bytes(), (output / 'kernel.elf').read_bytes())
    (output / 'uImage').write_bytes(package.legacy_image(gzip.compress((output / 'Image').read_bytes(), mtime=0), 2, 'Scarlet native Wi-Fi QA', package.LOAD, package.LOAD, 1))
    entries = [(n, 0o40755, b'') for n in ['bin','dev','etc','tmp','root','mnt','mnt/newroot','lib','lib/firmware','lib/firmware/brcm']]
    for name, binary in [('init','init'),('bin/netcfgd','netcfgd'),('bin/wifi-native-qa','wifi-native-qa'),('bin/scarlet-ld','scarlet-ld')]:
        entries.append((name, 0o100755, (target_dir / 'aarch64-unknown-scarlet/release' / binary).read_bytes()))
    if ssid is not None:
        entries.append(('bin/wifi-connect-qa', 0o100755, (target_dir / 'aarch64-unknown-scarlet/release/wifi-connect-qa').read_bytes()))
        conf = 'network={\n    ssid="' + ssid + '"\n    psk="' + (psk or '') + '"\n}\n'
        entries.append(('etc/wifi.conf', 0o100600, conf.encode()))
    pins = json.loads((PROJECT / 'wifi-firmware.json').read_text())
    for name, pin in pins['files'].items():
        payload = (PROJECT / 'firmware/wifi' / name).read_bytes()
        if len(payload) != pin['size'] or hashlib.sha256(payload).hexdigest() != pin['sha256']: raise ValueError('firmware hash mismatch')
        entries.append(('lib/firmware/brcm/'+name, 0o100644, payload))
    payload = cpio(entries)
    (output / 'initramfs').write_bytes(package.legacy_image(payload, 3, 'Scarlet Wi-Fi QA initramfs'))
    stack = PROJECT / 'firmware/bootstack'; pins = json.loads((PROJECT / 'bootstack.json').read_text())
    for name in ['bl33.bin','nx-plat.dtimg']:
        payload = (stack / name).read_bytes()
        if hashlib.sha256(payload).hexdigest() != pins['files'][name]: raise ValueError('bootstack hash mismatch')
        if name == 'bl33.bin':
            old = b'bootcmd=run distro_bootcmd\0'; new = b'bootcmd=source 8fe00000'
            if payload.count(old) != 1: raise ValueError('unexpected U-Boot environment')
            payload = payload.replace(old, new.ljust(len(old)-1,b' ')+b'\0')
        (output / name).write_bytes(payload)
    shutil.copyfile(PROJECT / '.scarlet/switchvisor/switchroot/scarlet-switchvisor/usb-uart.dtbo', output / 'usb-uart.dtbo')
    script = '''setenv fdt_high 0xffffffffffffffff
setenv initrd_high 0xffffffffffffffff
if test "${t210b01}" = 0 -a "${sku}" = 0; then
    if dtimg load 0xa8000000 ${sku} 0x8d000000 fdtrsize; then
        fdt addr 0x8d000000 ${fdtrsize}
        fdt resize 32768
        if fdt apply 0x8c000000; then
            fdt set /chosen scarlet,boot-mode kernel
            fdt set /chosen nvidia,wifi-mac ${device_wifi_mac}
            fdt set /pcie@1003000 status okay
            fdt set /pcie@1003000 scarlet,pcie-phy-owned
            fdt set /pcie@1003000 scarlet,pcie-direct-dma
            fdt rm /pcie@1003000 iommus
            fdt rm /pcie@1003000 iommu-map
            fdt rm /pcie@1003000 iommu-map-mask
            setenv bootargs "init=/init init.exec=/bin/{{QA_BIN}} maxcpus=1 keep_bootcon"
            bootm 0xa0000000 0x92000000 0x8d000000
        fi
    fi
fi
echo WIFI_NATIVE_BOOT_FAILED
'''
    script = script.replace("{{QA_BIN}}", "wifi-connect-qa" if ssid is not None else "wifi-native-qa")
    (output / 'boot.cmd').write_text(script)
    raw = script.encode(); (output / 'boot.scr').write_bytes(package.legacy_image(struct.pack('>II',len(raw),0)+raw, 6, 'Scarlet native Wi-Fi RAM QA', arch=2))
    images = [{'path':n,'address':hex(a),**({'runtime_size':'0x68200'} if n=='bl33.bin' else {})} for n,a in [('bl33.bin',0xaa000000),('uImage',0xa0000000),('initramfs',0x92000000),('boot.scr',0x8fe00000),('nx-plat.dtimg',0xa8000000),('usb-uart.dtbo',0x8c000000)]]
    ranges = sorted((int(e['address'],0), int(e['address'],0)+int(e.get('runtime_size',str((output/e['path']).stat().st_size)),0)) for e in images)
    if any(a[1]>b[0] for a,b in zip(ranges,ranges[1:])): raise ValueError('upload regions overlap')
    (output / 'bundle.json').write_text(json.dumps({'version':1,'entry':'0xaa000000','images':images},indent=2)+'\n')
    files = [output/e['path'] for e in images]
    (output / 'build-result.json').write_text(json.dumps({'scarlet_checkout':str(scarlet),'hardware_tested':False,'sha256':{str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in files}},indent=2)+'\n')
    print(output / 'bundle.json')
if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__); p.add_argument('--scarlet',type=Path,required=True); p.add_argument('--output',type=Path,required=True); p.add_argument('--target-dir',type=Path,default=ROOT/'.cache/wifi-native-target')
    p.add_argument('--ssid'); p.add_argument('--psk-file',type=Path)
    a=p.parse_args()
    psk=None
    if a.psk_file is not None:
        psk=a.psk_file.read_text().strip()
    prepare(a.scarlet.resolve(),a.output.resolve(),a.target_dir.resolve(),a.ssid,psk)
