// SPDX-License-Identifier: GPL-2.0-only
//! Scalar volatile copies for the private Normal NonCacheable DMA arena.
//! MMIO registers do not use these routines. DMA ownership and memory barriers
//! are checked by the caller; only the arena side needs natural alignment.
use core::ptr::{read_volatile, write_volatile};

/// # Safety
/// `source` covers output.len() readable DMA bytes, exclusively CPU-owned and
/// disjoint from output. The mapping permits naturally aligned u64 accesses.
pub unsafe fn read(source: *const u8, output: &mut [u8]) {
    let head = ((8 - (source as usize & 7)) & 7).min(output.len());
    let mut offset = 0;
    while offset < head {
        output[offset] = unsafe { read_volatile(source.add(offset)) };
        offset += 1;
    }
    while output.len() - offset >= 8 {
        let word = unsafe { read_volatile(source.add(offset).cast::<u64>()) };
        output[offset..offset + 8].copy_from_slice(&word.to_ne_bytes());
        offset += 8;
    }
    while offset < output.len() {
        output[offset] = unsafe { read_volatile(source.add(offset)) };
        offset += 1;
    }
}

/// # Safety
/// `destination` covers input.len() writable DMA bytes, exclusively CPU-owned
/// and disjoint from input. The mapping permits naturally aligned u64 accesses.
pub unsafe fn write(destination: *mut u8, input: &[u8]) {
    let head = ((8 - (destination as usize & 7)) & 7).min(input.len());
    let mut offset = 0;
    while offset < head {
        unsafe {
            write_volatile(destination.add(offset), input[offset]);
        }
        offset += 1;
    }
    while input.len() - offset >= 8 {
        let word = u64::from_ne_bytes(input[offset..offset + 8].try_into().unwrap());
        unsafe {
            write_volatile(destination.add(offset).cast::<u64>(), word);
        }
        offset += 8;
    }
    while offset < input.len() {
        unsafe {
            write_volatile(destination.add(offset), input[offset]);
        }
        offset += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_alignment_and_tail_preserves_payload_and_neighboring_storage() {
        for alignment in 0..8 {
            for length in (0..66).chain([113, 1514, 2048]) {
                let mut storage = [0xa5a5_a5a5_a5a5_a5a5u64; 260];
                let bytes = unsafe {
                    core::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<u8>(), 2080)
                };
                let mut input = std::vec![0; length + 9];
                for (i, byte) in input.iter_mut().enumerate() {
                    *byte = (i * 71) as u8;
                }
                let offset = 8 + alignment;
                unsafe {
                    write(bytes.as_mut_ptr().add(offset), &input[3..3 + length]);
                }
                assert_eq!(&bytes[offset..offset + length], &input[3..3 + length]);
                assert!(
                    bytes[..offset]
                        .iter()
                        .chain(&bytes[offset + length..])
                        .all(|b| *b == 0xa5)
                );
                let mut output = std::vec![0xee; length + 9];
                unsafe {
                    read(bytes.as_ptr().add(offset), &mut output[5..5 + length]);
                }
                assert_eq!(&output[5..5 + length], &input[3..3 + length]);
                assert!(
                    output[..5]
                        .iter()
                        .chain(&output[5 + length..])
                        .all(|b| *b == 0xee)
                );
            }
        }
    }
}
