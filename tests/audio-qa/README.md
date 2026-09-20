# Switch audio check

This is a manually launched SAS client; it never runs during boot. Each pass
plays one second each of left 440 Hz, right 880 Hz, stereo, and silence. The
48 kHz stream is closed before repeating at 44.1 kHz to exercise SAS resampling
and stop/start. Short fades avoid clicks at the tone boundaries.

`--silence` sends two seconds of zero PCM at 48 kHz. `--short` sends the
four segments for 250 ms each at 48 kHz (one second total). These modes help
isolate codec noise without replaying a full movie or a long test signal.

Build with the same userspace sysroot/configuration as the console project:

```sh
nix develop --command sh -c '
  unset CARGO_UNSTABLE_BUILD_STD CARGO_UNSTABLE_BUILD_STD_FEATURES
  CARGO_TARGET_DIR=.cache/audio-qa-target cargo build \
    --config projects/aarch64-switch-console/.cargo/userspace.toml \
    --manifest-path tests/audio-qa/Cargo.toml \
    --target aarch64-unknown-scarlet --release
'
```

Deploy `.cache/audio-qa-target/aarch64-unknown-scarlet/release/audio-qa` as an
executable in a test initramfs or the SD root. From an SD-root boot, a binary
placed in the initramfs at `/bin/audio-qa` is `/old_root/bin/audio-qa`.

Expected results:

- Both passes consume exactly four seconds of input: 192,000 and 176,400
  frames. Printed elapsed times are around 3.8 seconds because SAS buffers
  output; SAS drain acknowledges mixer consumption, not the last DAC sample.
- The user hears the requested channel order, with silence at each pass end.
- `/bin/logctl -u sas -n 30` has no output errors or XRUNs.
- The kernel log has no ADMA stall/overrun or clock/reset timeout.

Frame counts and elapsed times cannot prove analog output. Record the
listener's result separately. For full device release/configure coverage,
restart SAS between runs, then verify `/dev/audio0 ready` again. A second SAS
instance must fail to acquire the device while the first holds it.
