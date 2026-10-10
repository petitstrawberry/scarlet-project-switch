# Immutable canvas mesh validation

`validated-mesh-snapshot.patch` applies to ScarletUI commit
`1b0f08aeb0a7057c9a6d0d7312554372411afff8`. Apply it in an isolated checkout;
do not modify the Cargo dependency cache or a checkout with unrelated changes.

```sh
git apply /path/to/validated-mesh-snapshot.patch
cargo test --locked -p scarlet-ui-renderer-sgfx --lib snapshot
```

The renderer remembers a `Weak<SgfxMesh>` for the exact immutable allocation
that passed validation. An equal handle and revision alone do not bypass the
checks: every distinct allocation still receives structural and finite-value
validation. GPU upload state remains independent, including failed submission
and discard retries. No public mesh API changes.

Six production-path tests accompany the patch. Rendering one unchanged
60,000-vertex mesh eight times traverses its vertices once instead of eight
times, while retaining one 2,400,000-byte upload and eight draws. The tests also
cover invalid same-key snapshots, weak lifetime, revision/capacity changes and
failure retries. The full host suite recorded 79 passes and one pre-existing
text-upload failure; the unmodified base recorded 73 passes and the same
failure. Host validation used a reduced workspace with inactive preview
dependencies removed; those temporary manifest changes are not in this patch.

Boxcraft retains these immutable mesh allocations on camera-only frames, so
this bypass applies to its terrain path. It does not remove GM20B's
per-submission buffer snapshots. The kernel-only USB network candidate does
not contain this userspace patch.

Two physical Switch A/B pairs used Boxcraft revision
`c5cda1ac812354d9ea76ba72c305f61a3f3ac51e`, the same compiler, flags, lock and
dependencies, seed 7, SGFX, and the final compositor extent 1302x710. Each run
lasted 25 seconds; these are medians of its last 30 completed profile lines:

| Metric | Baseline runs | Patched runs |
| --- | --- | --- |
| Encode and submit | 40.37 / 41.03 ms | 26.55 / 21.61 ms |
| Completion wait | 12.66 / 12.77 ms | 13.11 / 12.49 ms |
| Paired encode/submit plus wait | 52.97 / 53.78 ms | 40.33 / 34.22 ms |

This supports a reduction in submission work for this initial scene, not a
general FPS claim. Background apps and clocks were not controlled, profiling
was enabled in both binaries, and completion wait includes CPU/queue work.
Windows resized during startup; the recorded final extents matched. Every
uploaded executable was read back byte-for-byte, and the test processes and
temporary files were removed. The tested binaries were temporary and did not
replace `/bin/boxcraft`.
