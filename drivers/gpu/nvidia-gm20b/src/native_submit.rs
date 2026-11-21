// SPDX-License-Identifier: GPL-2.0-only
//! Hostile-input grammar validation for native GM20B method streams (wire v1).
//!
//! Stage-one policy: userspace owns method encoding, the kernel owns authority.
//! Accepted packets are opcode 1 (incrementing) on subchannels 0 (3D) and
//! 3 (2D), plus opcode 5 (non-incrementing) writes to CB_POS on subchannel 0.
//! Every GPU address must arrive as a zero placeholder covered by exactly one
//! wire relocation; literal addresses are rejected. TIC/TSC descriptor
//! relocations inside CB_POS data and their semantic checks arrive with the
//! stage-two contract; stage one rejects them outright.

use core::cmp::Ordering;
use maxwell_native_submit_wire::{AddressEncoding, DecodedSubmit};

const SUB_3D: u32 = 0;
const SUB_2D: u32 = 3;
const OP_INCREMENT: u32 = 1;
const OP_NON_INCREMENT: u32 = 5;
const CB_POS: u32 = 0x238c;

/// Exact 3D method addresses emitted by the portable fixed and programmable
/// encoders. Sorted ascending; the unit tests verify ordering.
const METHODS_3D: &[u32] = &[
    0x0000, 0x0110, 0x0218, 0x021c, 0x02d0, 0x0300, 0x0308, 0x030c, //
    0x0360, 0x0364, 0x037c, 0x074c, 0x077c, //
    0x0800, 0x0804, 0x0808, 0x080c, 0x0810, 0x0814, 0x0818, 0x081c, 0x0820, //
    0x0a00, 0x0a04, 0x0a08, 0x0a0c, 0x0a10, 0x0a14, 0x0a18, //
    0x0c00, 0x0c04, 0x0c08, 0x0c0c, //
    0x0d64, 0x0d74, 0x0d78, 0x0d80, 0x0d84, 0x0d88, 0x0d8c, 0x0d90, //
    0x0dac, 0x0db0, 0x0de4, 0x0df8, 0x0dfc, //
    0x0e00, 0x0e04, 0x0e08, //
    0x0f84, 0x0f88, 0x0f90, 0x0fac, 0x0fdc, //
    0x0fe0, 0x0fe4, 0x0fe8, 0x0fec, 0x0ff0, 0x0ff4, 0x0ff8, //
    0x10cc, 0x10e0, 0x10e4, 0x10ec, 0x10f0, 0x10f8, 0x10fc, 0x1140, //
    0x1160, 0x1164, 0x1168, 0x116c, 0x1170, 0x1174, 0x1178, 0x117c, //
    0x1180, 0x1184, 0x1188, 0x118c, 0x1190, 0x1194, 0x1198, 0x119c, //
    0x121c, 0x1228, 0x122c, 0x1230, 0x1234, 0x1290, //
    0x12cc, 0x12d4, 0x12d8, 0x12dc, 0x12e4, 0x12e8, 0x12ec, //
    0x130c, 0x1330, 0x1334, 0x1338, 0x133c, //
    0x1340, 0x1344, 0x1348, 0x134c, 0x1350, 0x1358, 0x135c, //
    0x1360, 0x1380, 0x13ac, 0x1434, //
    0x1534, 0x1538, 0x153c, //
    0x1558, 0x155c, 0x1560, 0x1564, //
    0x1574, 0x1578, 0x157c, 0x1590, 0x15b4, 0x15cc, 0x15d0, 0x15e4, //
    0x1604, 0x1608, 0x160c, 0x1610, 0x1614, 0x1618, //
    0x164c, 0x16a8, 0x1794, 0x179c, //
    0x17c8, 0x17cc, 0x17d0, 0x17d4, 0x17d8, 0x17dc, 0x17e0, //
    0x1880, //
    0x1918, 0x191c, 0x1920, 0x192c, 0x193c, 0x194c, 0x197c, //
    0x19c0, 0x19c4, 0x19cc, 0x19d0, //
    0x19e0, 0x19e4, 0x19e8, 0x19ec, 0x19f0, 0x19f4, 0x19f8, 0x19fc, //
    0x1a00, //
    0x1c00, 0x1c04, 0x1c08, 0x1c0c, //
    0x1c10, 0x1c20, 0x1c30, 0x1c40, 0x1c50, 0x1c60, 0x1c70, 0x1c80, //
    0x1c90, 0x1ca0, 0x1cb0, 0x1cc0, 0x1cd0, 0x1ce0, 0x1cf0, //
    0x1f00, 0x1f04, //
    0x2000, 0x2004, 0x200c, 0x2040, 0x2044, 0x204c, //
    0x2080, 0x2084, 0x208c, 0x20c0, 0x20c4, 0x20cc, //
    0x2100, 0x2104, 0x210c, 0x2140, 0x2144, 0x214c, //
    0x2380, 0x2384, 0x2388, 0x238c, 0x2410, 0x2490, 0x2608,
];

/// Inclusive 2D method ranges emitted by the 902D copy template.
const METHODS_2D: &[(u32, u32)] = &[
    (0x0000, 0x0000),
    (0x0200, 0x0224),
    (0x0230, 0x0254),
    (0x0260, 0x0260),
    (0x026c, 0x026c),
    (0x0290, 0x029c),
    (0x02ac, 0x02ac),
    (0x0884, 0x088c),
    (0x08b0, 0x08dc),
];

/// TIC/TSC address-high words are literal zero: only the low dword carries a
/// sub-4-GiB GPU VA through one relocation.
const LITERAL_ZERO_METHODS: &[u32] = &[0x155c, 0x1574];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Half {
    /// High dword of one relocated GPU VA; its low partner sits at +4.
    Hi,
    /// Low dword of one relocated GPU VA; consumed with the preceding high.
    Lo,
    /// Single low dword of one sub-4-GiB relocated GPU VA.
    Lo32,
}

/// Address halves by (subchannel, method). Sorted ascending per table.
const ADDRESS_HALVES_3D: &[(u32, Half)] = &[
    (0x0800, Half::Hi),
    (0x0804, Half::Lo),
    (0x0f84, Half::Hi),
    (0x0f88, Half::Lo),
    (0x0fe0, Half::Hi),
    (0x0fe4, Half::Lo),
    (0x1560, Half::Lo32),
    (0x1578, Half::Lo32),
    (0x1608, Half::Hi),
    (0x160c, Half::Lo),
    (0x17c8, Half::Hi),
    (0x17cc, Half::Lo),
    (0x1c04, Half::Hi),
    (0x1c08, Half::Lo),
    (0x1f00, Half::Hi),
    (0x1f04, Half::Lo),
    (0x2384, Half::Hi),
    (0x2388, Half::Lo),
];

const ADDRESS_HALVES_2D: &[(u32, Half)] = &[
    (0x0220, Half::Hi),
    (0x0224, Half::Lo),
    (0x0250, Half::Hi),
    (0x0254, Half::Lo),
];

fn method_allowed_3d(method: u32) -> bool {
    METHODS_3D.binary_search(&method).is_ok()
}

fn method_allowed_2d(method: u32) -> bool {
    let mut index = 0usize;
    while index < METHODS_2D.len() {
        let (start, end) = METHODS_2D[index];
        if method < start {
            return false;
        }
        if method <= end {
            return true;
        }
        index += 1;
    }
    false
}

fn method_allowed(sub: u32, method: u32) -> bool {
    if sub == SUB_3D {
        method_allowed_3d(method)
    } else {
        method_allowed_2d(method)
    }
}

fn address_half(sub: u32, method: u32) -> Option<Half> {
    let table = if sub == SUB_3D {
        ADDRESS_HALVES_3D
    } else {
        ADDRESS_HALVES_2D
    };
    let mut low = 0usize;
    let mut high = table.len();
    while low < high {
        let middle = (low + high) / 2;
        match table[middle].0.cmp(&method) {
            Ordering::Less => low = middle + 1,
            Ordering::Greater => high = middle,
            Ordering::Equal => return Some(table[middle].1),
        }
    }
    None
}

fn expect_slot(
    decoded: &DecodedSubmit<'_>,
    word_offset: usize,
    encoding: AddressEncoding,
    next_relocation: &mut usize,
) -> Result<(), &'static str> {
    let relocation = decoded
        .relocation(*next_relocation)
        .ok_or("native address slot lacks a relocation")?;
    if relocation.method_word_offset as usize != word_offset || relocation.encoding != encoding {
        return Err("native relocation does not match an address slot");
    }
    *next_relocation += 1;
    Ok(())
}

/// Validate one decoded native method stream against the stage-one policy.
pub fn validate(decoded: &DecodedSubmit<'_>) -> Result<(), &'static str> {
    let words = decoded.method_len();
    if words == 0 {
        return Err("native method stream empty");
    }
    let mut cursor = 0usize;
    let mut next_relocation = 0usize;
    while cursor < words {
        let header = decoded
            .method_word(cursor)
            .ok_or("native method stream truncated")?;
        let opcode = header >> 29;
        let count = ((header >> 16) & 0x1fff) as usize;
        let sub = (header >> 13) & 7;
        let method = (header & 0x1fff) * 4;
        if count == 0 {
            return Err("native packet has zero length");
        }
        let Some(data_end) = (cursor + 1).checked_add(count) else {
            return Err("native packet overruns the method stream");
        };
        if data_end > words {
            return Err("native packet overruns the method stream");
        }
        let data_start = cursor + 1;
        if opcode == OP_NON_INCREMENT {
            if sub != SUB_3D || method != CB_POS {
                return Err("native non-incrementing packets must target CB_POS");
            }
        } else if opcode == OP_INCREMENT {
            if sub != SUB_3D && sub != SUB_2D {
                return Err("native packet subchannel unsupported");
            }
            for index in 0..count {
                let word_method = method + index as u32 * 4;
                if !method_allowed(sub, word_method) {
                    return Err("native method is not whitelisted");
                }
                let word = decoded
                    .method_word(data_start + index)
                    .ok_or("native method stream truncated")?;
                if sub == SUB_3D
                    && LITERAL_ZERO_METHODS.binary_search(&word_method).is_ok()
                    && word != 0
                {
                    return Err("native literal-zero method word must be zero");
                }
                match address_half(sub, word_method) {
                    Some(Half::Hi) => {
                        if index + 1 >= count
                            || address_half(sub, word_method + 4) != Some(Half::Lo)
                        {
                            return Err("native address high word lacks its low partner");
                        }
                        expect_slot(
                            decoded,
                            data_start + index,
                            AddressEncoding::GpuVa64HiLo,
                            &mut next_relocation,
                        )?;
                    }
                    Some(Half::Lo) => {
                        if index == 0 {
                            return Err("native address low word lacks its high partner");
                        }
                    }
                    Some(Half::Lo32) => {
                        expect_slot(
                            decoded,
                            data_start + index,
                            AddressEncoding::GpuVa32Lo,
                            &mut next_relocation,
                        )?;
                    }
                    None => {}
                }
            }
        } else {
            return Err("native packet opcode unsupported");
        }
        cursor = data_start + count;
    }
    if next_relocation != decoded.relocation_len() {
        return Err("native relocation does not match an address slot");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{
        validate, ADDRESS_HALVES_2D, ADDRESS_HALVES_3D, Half, LITERAL_ZERO_METHODS, METHODS_2D,
        METHODS_3D,
    };
    use maxwell_native_submit_wire::{
        AddressEncoding, Relocation, RelocationSource, Resource, Submit, ACCESS_READ,
        ACCESS_WRITE, decode, encode, encoded_len,
    };
    use std::vec;
    use std::vec::Vec;

    fn packet(words: &mut Vec<u32>, opcode: u32, sub: u32, method: u32, data: &[u32]) {
        words.push(
            (opcode << 29) | ((data.len() as u32) << 16) | (sub << 13) | (method >> 2),
        );
        words.extend_from_slice(data);
    }

    fn resource() -> Resource {
        Resource {
            attachment_token: 1,
            range_offset: 0,
            range_size: 4096,
            access: ACCESS_READ | ACCESS_WRITE,
        }
    }

    fn relocation(word_offset: usize, encoding: AddressEncoding) -> Relocation {
        Relocation {
            method_word_offset: word_offset as u32,
            source: RelocationSource::Attachment(0),
            resource_offset: 0,
            required_size: 4096,
            access: ACCESS_READ,
            encoding,
        }
    }

    fn check(methods: &[u32], relocations: &[Relocation]) -> Result<(), &'static str> {
        let resources = [resource()];
        let submit = Submit {
            methods,
            resources: &resources,
            relocations,
        };
        let mut bytes = Vec::new();
        bytes.resize(encoded_len(submit).unwrap(), 0);
        encode(submit, &mut bytes).unwrap();
        let decoded = decode(&bytes).unwrap();
        validate(&decoded)
    }

    #[test]
    fn whitelist_tables_are_sorted_and_pairwise() {
        assert!(METHODS_3D.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(LITERAL_ZERO_METHODS.windows(2).all(|pair| pair[0] < pair[1]));
        for table in [ADDRESS_HALVES_3D, ADDRESS_HALVES_2D] {
            assert!(table.windows(2).all(|pair| pair[0].0 < pair[1].0));
            for (method, half) in table {
                if *half == Half::Hi {
                    assert_eq!(
                        table.iter().find(|entry| entry.0 == method + 4).map(|e| e.1),
                        Some(Half::Lo)
                    );
                }
            }
        }
        assert!(METHODS_2D.windows(2).all(|pair| pair[0].1 < pair[1].0));
        for (method, _) in ADDRESS_HALVES_3D {
            assert!(METHODS_3D.binary_search(method).is_ok());
        }
        for (method, _) in ADDRESS_HALVES_2D {
            assert!(super::method_allowed_2d(*method));
        }
        for method in LITERAL_ZERO_METHODS {
            assert!(METHODS_3D.binary_search(method).is_ok());
        }
    }

    #[test]
    fn native_stream_with_relocated_addresses_is_accepted() {
        let mut methods = Vec::new();
        packet(&mut methods, 1, 0, 0x0000, &[0xb197]);
        packet(&mut methods, 1, 0, 0x0110, &[0]);
        let rt_hi = methods.len() + 1;
        packet(
            &mut methods,
            1,
            0,
            0x0800,
            &[0, 0, 1, 0xcf, 0x50, 1, 0, 0],
        );
        let tic_lo = methods.len() + 2;
        packet(&mut methods, 1, 0, 0x1574, &[0, 0, 1]);
        packet(&mut methods, 5, 0, 0x238c, &[0x20, 1, 2, 3, 4]);
        let dst_hi = methods.len() + 4;
        packet(&mut methods, 1, 3, 0x0214, &[256, 16, 16, 0, 0]);
        let src_hi = methods.len() + 4;
        packet(&mut methods, 1, 3, 0x0244, &[256, 16, 16, 0, 0]);
        let relocations = vec![
            relocation(rt_hi, AddressEncoding::GpuVa64HiLo),
            relocation(tic_lo, AddressEncoding::GpuVa32Lo),
            relocation(dst_hi, AddressEncoding::GpuVa64HiLo),
            relocation(src_hi, AddressEncoding::GpuVa64HiLo),
        ];
        assert_eq!(check(&methods, &relocations), Ok(()));
    }

    #[test]
    fn rejects_unsupported_opcodes_subchannels_and_methods() {
        for (opcode, sub, method) in [
            (2u32, 0u32, 0x0110u32),
            (5, 0, 0x0110),
            (1, 1, 0x0110),
            (1, 0, 0xdead),
        ] {
            let mut methods = Vec::new();
            packet(&mut methods, opcode, sub, method, &[0]);
            assert!(check(&methods, &[]).is_err());
        }
    }

    #[test]
    fn rejects_spans_crossing_out_of_the_whitelist() {
        let mut methods = Vec::new();
        packet(&mut methods, 1, 0, 0x0800, &[0; 10]);
        assert!(check(&methods, &[]).is_err());
    }

    #[test]
    fn rejects_address_halves_without_their_partner() {
        let mut orphan_high = Vec::new();
        packet(&mut orphan_high, 1, 0, 0x0800, &[0]);
        assert!(check(&orphan_high, &[]).is_err());

        let mut orphan_low = Vec::new();
        packet(&mut orphan_low, 1, 0, 0x0804, &[0, 0]);
        assert!(check(&orphan_low, &[]).is_err());
    }

    #[test]
    fn rejects_address_slots_without_matching_relocations() {
        let mut methods = Vec::new();
        let rt_hi = methods.len() + 1;
        packet(
            &mut methods,
            1,
            0,
            0x0800,
            &[0, 0, 1, 0xcf, 0x50, 1, 0, 0],
        );
        assert!(check(&methods, &[]).is_err());

        // A relocation with the wrong encoding still fails the slot match.
        let relocations = vec![relocation(rt_hi, AddressEncoding::GpuVa32Lo)];
        assert!(check(&methods, &relocations).is_err());
    }

    #[test]
    fn rejects_relocations_outside_address_slots() {
        let mut methods = Vec::new();
        packet(&mut methods, 1, 0, 0x0000, &[0xb197]);
        packet(&mut methods, 1, 0, 0x0df8, &[0, 0]);
        packet(&mut methods, 5, 0, 0x238c, &[0x20, 0, 0, 0, 0]);

        // Relocation onto ordinary method data.
        assert!(check(&methods, &[relocation(3, AddressEncoding::GpuVa64HiLo)]).is_err());
        // Stage one rejects descriptor relocations inside CB_POS data.
        assert!(check(&methods, &[relocation(8, AddressEncoding::GpuVa64HiLo)]).is_err());
    }

    #[test]
    fn rejects_nonzero_literal_zero_method_words() {
        let mut methods = Vec::new();
        packet(&mut methods, 1, 0, 0x1574, &[1, 0, 1]);
        assert!(check(&methods, &[]).is_err());
    }

    #[test]
    fn rejects_truncated_packets() {
        let mut methods = vec![(1u32 << 29) | (9 << 16) | (0x0800 >> 2)];
        methods.extend_from_slice(&[0; 3]);
        assert!(check(&methods, &[]).is_err());
    }
}
