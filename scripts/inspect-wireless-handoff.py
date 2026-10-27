#!/usr/bin/env python3
"""Resolve the captured ODIN PCIe/PHY/power resources without accessing hardware."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import struct


def nodes(data):
    magic, size, structure, strings, _, version, _, _, strings_size, structure_size = struct.unpack_from(">10I", data)
    if magic != 0xD00DFEED or size != len(data) or version < 17:
        raise ValueError("invalid FDT header")
    if strings + strings_size > size or structure + structure_size > size:
        raise ValueError("FDT block exceeds total size")
    names = data[strings:strings + strings_size]
    block = data[structure:structure + structure_size]
    result, stack, offset = {}, [], 0
    while offset + 4 <= len(block):
        token, = struct.unpack_from(">I", block, offset)
        offset += 4
        if token == 1:
            end = block.index(0, offset)
            stack.append(block[offset:end].decode())
            offset = (end + 4) & ~3
            result["/".join(stack) or "/"] = {}
        elif token == 2:
            stack.pop()
        elif token == 3:
            length, name = struct.unpack_from(">II", block, offset)
            offset += 8
            if not stack or offset + length > len(block) or name >= len(names):
                raise ValueError("invalid FDT property")
            key = names[name:names.index(0, name)].decode()
            result["/".join(stack) or "/"][key] = block[offset:offset + length]
            offset = (offset + length + 3) & ~3
        elif token == 4:
            pass
        elif token == 9:
            if stack:
                raise ValueError("unclosed FDT node")
            return result
        else:
            raise ValueError(f"unexpected FDT token {token}")
    raise ValueError("missing FDT end token")


def cells(data):
    if len(data) % 4:
        raise ValueError("unaligned DT cells")
    return list(struct.unpack(f">{len(data) // 4}I", data))


def value(data):
    if not data:
        return True
    if data[0] != 0 and data.endswith(b"\0") and all(byte == 0 or 32 <= byte < 127 for byte in data):
        return data[:-1].decode().split("\0")
    return cells(data) if len(data) % 4 == 0 else data.hex()


def inspect(dtb, serial):
    tree = nodes(dtb)
    if "nvidia,odin" not in value(tree["/"]["compatible"]):
        raise ValueError("expected ODIN root compatible")
    phandles = {cells(p["phandle"])[0]: path for path, p in tree.items() if "phandle" in p}
    host = tree["/pcie@1003000"]

    def provider(handle):
        path = phandles[handle]
        return {"path": path, "properties": {name: value(data) for name, data in tree[path].items()}}

    supplies = {name: provider(cells(data)[0]) for name, data in host.items() if name.endswith("-supply")}
    ports = {}
    for path, props in tree.items():
        if path.startswith("/pcie@1003000/pci@") and path.count("/") == 2:
            ports[path] = {"properties": {name: value(data) for name, data in props.items()},
                           "phys": [provider(handle) for handle in cells(props.get("phys", b""))]}
    registers = {name: int(v, 16) for name, v in re.findall(r"^REG (\w+) address=0x[0-9a-f]+ value=(0x[0-9a-f]+)$", serial, re.M)}
    # IDs 70/72/74 belong to CAR bank U (64..95), not bank H (32..63).
    state = {}
    if "CAR_CLK_U" in registers and "CAR_RST_U" in registers:
        state = {"pex_clock_enabled": bool(registers["CAR_CLK_U"] & (1 << 6)),
                 "afi_clock_enabled": bool(registers["CAR_CLK_U"] & (1 << 8)),
                 "pex_reset_asserted": bool(registers["CAR_RST_U"] & (1 << 6)),
                 "afi_reset_asserted": bool(registers["CAR_RST_U"] & (1 << 8)),
                 "pcie_x_reset_asserted": bool(registers["CAR_RST_U"] & (1 << 10))}
    if "PMC_PWRGATE_STATUS" in registers:
        partition = cells(tree[phandles[cells(host["power-domains"])[0]]]["partition-id"])[0]
        state["power_partition"] = partition
        state["partition_powered"] = bool(registers["PMC_PWRGATE_STATUS"] & (1 << partition))
    if "PLLE_MISC" in registers:
        state["plle_lock"] = bool(registers["PLLE_MISC"] & (1 << 11))
    chosen = tree["/chosen"]
    mac = chosen.get("nvidia,wifi-mac", b"").rstrip(b"\0").decode()
    return {"dtb_sha256": hashlib.sha256(dtb).hexdigest(),
            "host": {name: value(data) for name, data in host.items()},
            "power_domain": provider(cells(host["power-domains"])[0]), "supplies": supplies,
            "ports": ports, "registers": registers, "observed_car_state": state,
            "dt_wifi_mac": mac, "dt_wifi_mac_is_placeholder": mac.lower() == "11:22:33:44:55:77",
            "pci_endpoint_id": None, "firmware_loaded": False,
            "phy_parent": {name: value(data) for name, data in tree["/xusb_padctl@7009f000"].items()},
            "note": "DT describes BCM4356; endpoint identity has not been read from PCI config space."}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    directory = parser.parse_args().directory
    result = inspect((directory / "handoff.dtb").read_bytes(), (directory / "serial.log").read_text())
    (directory / "hardware.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result["observed_car_state"], indent=2))
