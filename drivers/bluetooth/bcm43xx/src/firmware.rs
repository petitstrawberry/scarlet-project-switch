// SPDX-License-Identifier: GPL-2.0-only
use alloc::{vec, vec::Vec};
use sha2::{Digest, Sha256};

const PATCH_SIZE: usize = 39008;
const DIGEST: [u8; 32] = [
    0x76, 0x1a, 0x2c, 0x26, 0xf2, 0xbc, 0xe5, 0xb0, 0xc4, 0xed, 0x6e, 0xfa, 0x8a, 0x3d, 0xc0, 0x7e,
    0x0c, 0xce, 0xa1, 0xdc, 0xd2, 0x84, 0xe8, 0x2c, 0xbf, 0xdb, 0x42, 0x19, 0x07, 0xd2, 0x7c, 0xab,
];

pub fn load() -> Result<Vec<u8>, &'static str> {
    let vfs = scarlet::fs::manager::get_global_vfs_manager_safe()
        .ok_or("Bluetooth firmware filesystem unavailable")?;
    let scarlet::object::KernelObject::File(file) = vfs
        .open("/lib/firmware/brcm/BCM4356A3.hcd", 0)
        .map_err(|_| "Bluetooth firmware unavailable")?
    else {
        return Err("invalid Bluetooth firmware file");
    };
    if file
        .metadata()
        .map_err(|_| "invalid Bluetooth firmware metadata")?
        .size
        != PATCH_SIZE
    {
        return Err("unexpected Bluetooth patch size");
    }
    let mut data = vec![0; PATCH_SIZE];
    let mut remaining = &mut data[..];
    while !remaining.is_empty() {
        let n = file
            .read(remaining)
            .map_err(|_| "Bluetooth patch read failed")?;
        if n == 0 {
            return Err("truncated Bluetooth patch");
        }
        remaining = remaining
            .get_mut(n..)
            .ok_or("invalid Bluetooth firmware read length")?;
    }
    if Sha256::digest(&data)[..] != DIGEST {
        return Err("Bluetooth firmware digest mismatch");
    }
    Ok(data)
}
