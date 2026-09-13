# Switch boot video IMG_9082

Source: `/Users/petitstrawberry/Downloads/IMG_9082.mov`, 3.332 seconds,
1920×1080 HEVC, approximately 53.72 fps. SHA256:
`db5a39a1a486b266dac6a4e0246a6721732a132e68fb0ed91e43ab42edbe101f`.

The source is intact. Ten-fps frames and original-cadence frames from
2.35–2.95 seconds are in `.cache/video-9082/`. Metadata, the source hash,
readability crops and ELF disassembly are kept there too. Times and register
readings are visual. Candidate attribution follows the preceding eight-file
SD readback in `gpu-selector-display-verification.json`, with Image SHA256
`c79a7520a10fd5a078aba9892405b7b490d2d72e6c57f8bdd3af4ad3dff3a9f7`.
The recording itself does not display an artifact hash. The symbolized ELF
matches that receipt: SHA256
`b3a69f49feec0a7edb88a28721a13f4f813d8cc108e391225913829dee053306`.

| Frame | Visible evidence | Scope |
| --- | --- | --- |
| `frame-013`, `frame-019` | Four CPUs detected; CPU-frequency provider ready with mask `0xf` | Boot discovery/provider initialization, without AP execution or frequency measurement. |
| `frame-025`, `fault-019` | GPIO6 `0x09`, MC flush complete, `MC_BOOT_0=0x12b000a1 read_us=16` | The corrected power/identity path succeeds again. |
| `fault-019` | BAR1 reads A=`0x53474131`, B=`0x53474232`; write=`0x53475733`, remap=`0x53474232`; `read/write/remap passed` | Private GMMU backing, BAR1 access and the tested TLB remap work on hardware. |
| `fault-019` | `/dev/gpu0` registered; `execution support=0` | The endpoint remains control-only; no GR queue or SGFX execution is implemented. |
| `fault-019` | RTC wall-clock seed and ten-contact touchscreen ready | Initialization occurred, without RTC accuracy, touch interaction or Joy-Con input validation. |
| `fault-019` | Late initialization; CAR/MC/DC mappings; inherited DC address `0x0/0xf5a00000`, options `0x40000000`, kind zero, color 12, size `0x50002d0`, prescale `0x5000b40` | DC probe progressed through its inherited window snapshot; the preceding SMMU/clock guards did not reject it. Transient lines are partially obscured by rolling display clearing. |
| `fault-019`, `frame-030` | Kernel data abort; EC=`0x25`, FSC=`0x5`; FAR=`0xffff80017f244028`, x19 equal to FAR, x30=`0x804e6dd4` | Fatal kernel failure, rather than an unchanged screen with ongoing startup. Full ESR/ELR are not readable and are not reconstructed. |

## Fault interpretation

The exact ELF places x30 at `PageTable::split_leaf + 0x2c4`, immediately
after its `virt_to_phys(child_table)` call. The next instruction loads the
parent descriptor through x19. This is a return-address observation, not an
ELR reading. The visually readable FSC is a level-1 translation fault.

`split_leaf` clears a block, publishes it and broadcasts TLBI before writing
the new child-table descriptor. Here the parent descriptor's HHDM address
lies within the same 1-GiB physical block (`0x140000000`–`0x17fffffff`)
being split. Removing that mapping makes the descriptor pointer inaccessible
before the replacement can be published. DC's first PMM-buffer attribute
change reaches this common VM path. This is the code/disassembly-based
interpretation of the recording; the missing ELR remains a diagnostic limit.

Native scanout activation is not observed. Shell boot, sustained timers,
colors/rotation, GPU-image scanout and input interaction are not validated by
this failed boot. Private GMMU validation does not imply GR firmware loading
or SGFX rendering.

The correction builds the AArch64 HHDM with 4-KiB leaves before activation,
so DMA-buffer retagging does not need to break a live block containing other
PMM allocations or page-table storage. Other mapping regions retain block
selection. Fatal aborts also repeat ESR/ELR/FAR after the register dump so
early-console wrapping leaves the decisive values visible on the next run.
Physical validation of that correction requires a subsequent recording.
