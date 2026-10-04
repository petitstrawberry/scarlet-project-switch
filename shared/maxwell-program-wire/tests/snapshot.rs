// SPDX-License-Identifier: GPL-2.0-only
extern crate alloc;
#[allow(dead_code)]
#[path = "../../../drivers/gpu/nvidia-gm20b/src/program.rs"]
mod program;
use maxwell_program_wire::*;

#[test]
fn executable_is_copied_once_then_materialized_from_private_snapshot() {
    let mut m = Metadata::new(Stage::Vertex);
    m.attr_in[0] = 15;
    m.sysvals_out_ab = 0xf0000000;
    m.store_req_start = 28;
    m.store_req_end = 31;
    let words: [u32; 16] = [
        0x1c400706, 0x003fb401, 0x0807ff00, 0xefd9ff80, 0x0707ff00, 0xeff1ff81, 0x0007000f,
        0xe3000000, 0xfc0007e2, 0x001f8000, 0x00070f00, 0x50b00000, 0x00070f00, 0x50b00000,
        0x00070f00, 0x50b00000,
    ];
    let code: Vec<u8> = words.into_iter().flat_map(u32::to_le_bytes).collect();
    let mut bytes = vec![0; HEADER_SIZE + code.len()];
    Program {
        metadata: m,
        code: &code,
    }
    .encode_into(&mut bytes)
    .unwrap();
    let limits = Limits {
        cb_sizes: [0; 32],
        resource_mask: 0,
    };
    let snapshot = program::Snapshot::copy_from(&bytes, &limits).unwrap();
    bytes.fill(0xff); // Public attachment bytes are modified after preparation.
    let mut arena = vec![0xa5; snapshot.arena_len()];
    snapshot.materialize(&mut arena).unwrap();
    assert_eq!(
        &arena[program::CODE_OFFSET..program::CODE_OFFSET + code.len()],
        code
    );
    assert_eq!(&arena[..program::ENTRY_OFFSET], &[0; program::ENTRY_OFFSET]);
    assert_eq!(
        u32::from_le_bytes(arena[0x30..0x34].try_into().unwrap()),
        0x20461
    );
    assert_eq!(snapshot.stage(), Stage::Vertex);
    assert_eq!(snapshot.attr_in(), &m.attr_in);
    assert_eq!(snapshot.metadata(), &m);
    assert_eq!(
        snapshot.materialize(&mut arena[..100]).unwrap_err(),
        Error::Size
    );
}
