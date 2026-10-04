# Maxwell programmable wire format

`maxwell-program-wire` is a `no_std` crate using `alloc` only for verifier CFG
state. Packages contain one vertex or fragment shader. The 512-byte versioned
little-endian metadata block precedes SM50/52 SASS bundles, each consisting of
one scheduler word and three instruction words. `Program::encode_into` and
`Program::parse` are the canonical producer and consumer. The maximum code size
is 64 KiB. Metadata declares GPR usage, immediate constant-buffer extents,
resource slots, and the pinned NAK graphics IO maps. It contains no SPH words.

`validate` requires separate kernel-supplied `Limits`: declared metadata alone
cannot authorize a constant-buffer bank or descriptor slot. Immediate bound
texture handles are `8 + resource_slot`, matching the kernel-owned CB15 handle
table. Direct CB reads must fit both declared and actual kernel-uploaded bank
extents. Resource table construction and attachment permissions are enforced by
the driver before calling this crate.

The opcode allowlist follows the Mesa revision recorded in `NOTICE` and
`MESA_SHA`. Each computational encoding has explicit fixed bits, operand bits,
reserved bits, sparse enum choices, GPR spans, and CB width. External instruction
decoders separately check bound texture provenance and immediate attributes
against IO reflection. Dynamic attribute addressing, dynamic LDC, bindless
textures, general loads/stores, atomics, surface operations, calls, indirect
branches, and unknown system registers fail closed. Scheduler words admit only
documented scoreboard barrier indices and reject the reserved high bit.

The control pass starts at the first instruction and propagates typed SSY, PBK,
and PCNT target stacks through every predicate outcome and direct branch. SYNC,
BRK, and CONT must find the matching stack token in the order used by pinned NAK.
Any stack underflow, conflicting stack at a CFG join, or depth above the 16
resident entries is rejected. Loops must return to the same proven stack state;
re-entering a push without popping fails. Every instruction is decoded, including
unreachable padding. Branch targets must be aligned executable words inside the
same stage, and reachable execution cannot fall beyond the package.

Only successful validation returns a generated 80-byte SPHv3 header. Local
memory, global stores, CRS spill, interlock, tessellation, and geometry are
disabled. Vertex ISBE space sharing stays conservatively disabled; fragment
headers match pinned NAK. Executable bytes are materialized by the driver's
private `Snapshot`, with SPH at offset 0x30 and SASS at offset 0x80 of a separate
page-aligned arena. That arena must remain absent from public attachments and
GPU write/copy destinations. Validation does not assert hardware readiness;
startup draw/readback probes remain the driver's execution gate.

Run `cargo test --manifest-path shared/maxwell-program-wire/Cargo.toml` from the
repository root. Tests cover actual NAK passthrough binaries, fourteen actual
WGSL frontend packages, instruction/reserved-bit rejection, signed branch
bounds, control-stack loops and overflow, CB and descriptor authority, random
wire/instruction mutations, and immutable snapshot materialization. Checked-in
frontend artifact hashes and their generator are in `tests/fixtures/manifest.json`.
