# GM20B canonical SGFX shaders

This pack contains actual Maxwell SASS and 80-byte shader headers produced by
Mesa Nouveau's NIR compiler for chipset `0x12b`. The fixed SGFX program semantics,
vertex layouts and pipeline variant identities follow the Chromebook SGFX
backend. Adreno machine code is not reused.

Mesa is pinned to `e881540692daac6532cefec76699f7a025563767` from
<https://gitlab.freedesktop.org/mesa/mesa.git>. The complete host producer and
Meson integration patch are under `artifacts/gm20b`. Derived material retains
the upstream copyright/permission notices in `NOTICE`.

There are seven vertex programs and eight fragment programs, including NV12
sampling with uniform or vertex color. Uniform CB0 is 160 bytes: a column-major
mat4, RGBA color, two plane coordinate transforms, and three YCbCr conversion
rows. Existing RGB programs use only the first 80 bytes. Driver CB15 contains
the Y/RGB texture handle at byte `0x20` and UV handle at `0x24`. The texture
descriptor table permits indices 0 and 1; both handles use sampler 0.
Each program occupies a zeroed 4-KiB slot;
its header begins at `0x30`, and SASS begins at `0x80`, matching Mesa's Maxwell
alignment. Metadata records the actual native input/output slots and GPR counts.
Programs using TLS, shared memory, relocations, global memory access, FP64,
barriers, loops or unsupported system values are rejected by the producer.

## Reproduce

Use a fresh Mesa checkout at the pinned revision. Copy `sgfx_compile.c` into
`src/gallium/drivers/nouveau/codegen/` and apply `meson-target.patch` at the
Mesa root. The host compiler opens no DRM device and can run on macOS.

```sh
meson setup build-maxwell --buildtype=release \
  -Dgallium-drivers=softpipe -Dvulkan-drivers=[] -Dplatforms=[] \
  -Dllvm=disabled -Degl=disabled -Dglx=disabled -Dgbm=disabled \
  -Dgles1=disabled -Dgles2=disabled
ninja -C build-maxwell src/gallium/drivers/nouveau/codegen/sgfx_compile
build-maxwell/src/gallium/drivers/nouveau/codegen/sgfx_compile output-shaders
```

Meson, Ninja, pkg-config, Bison, Flex, Python Mako/PyYAML/packaging, zlib,
Expat and libxml2 are needed. On the development Mac these were supplied by
a pure Nix shell. Generation produced all 15 programs successfully; it does
not establish that they execute correctly on the Switch.

Compare generated `.bin`, `.header.bin` and `mesa-metadata.json` files against
`artifacts/gm20b/SHA256SUMS`. The console build runs
`python3 scripts/verify-maxwell-shaders.py` before packaging. An intentional
pack update must update the checksum manifest, the metadata digest in both
the verifier and Rust pack, and the reviewed kernel method templates.

The kernel keeps the pack in GPU-owned memory. Userspace submits logical
pipeline identities and capability-authorized resource references; it cannot
replace SASS or submit arbitrary PGRAPH methods.

Before exposing the GPU, the kernel additionally reads back 12 NV12 draws:
linear and 2-GOB block-linear storage, both fragment programs, BT.601/BT.709,
limited/full range, and cropped images with poisoned row padding. These use
CPU-produced planes and do not depend on NVDEC. All 12 passed on Switch.
