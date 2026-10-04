# SGFX Maxwell NAK backend

This crate vendors the pure Rust portion of Mesa Nouveau NAK at revision
`e881540692daac6532cefec76699f7a025563767`. It optimizes SSA, legalizes
instructions, allocates registers, resolves parallel copies, schedules
instructions and encodes Maxwell SASS. It also emits the NVIDIA SPHv3
graphics shader header and final register, scratch and graphics IO metadata.

The public entrypoint is `compile_graphics_ir(ir::Shader)`; the frontend
constructs the same public NAK IR used by upstream Mesa. The included
`graphics_shader_info` helper creates the stage-specific metadata, including
the mandatory fragment shader input bit. Callers must populate the IO
reflection and the maximum structured control-flow stack depth.

Only SM50 and SM52 vertex and fragment programs are accepted. C NIR lowering,
the Mesa C API, DRM, compute launch descriptors, hardware tests and other
GPU generation backends are omitted. This is a compiler backend, not a claim
that every operation of a frontend language has been lowered correctly or
that a shader has been tested on GM20B hardware.

The crate uses Rust `std`. It has no C/DRM dependencies, no build-time code
generation and no runtime downloads. SPH constants are checked in after
generation from the pinned NVIDIA `cla097sph.h` using Mesa's
`struct_parser.py`. The original header and generator are preserved beside
them. All source copyright notices are preserved; see `NOTICE`,
`LICENSE-MIT` and `UPSTREAM.json` for provenance and source hashes.

Run the compiler tests with:

```sh
cargo test --manifest-path shared/sgfx-nak/Cargo.toml
```
