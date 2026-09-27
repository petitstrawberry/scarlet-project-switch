# UDP network measurement

`net-qa` receives controlled UDP traffic without Moonlight, NVDEC or SWS. The
Python sender runs on the Mac. This measures the host → USB/NCM → Switchvisor →
Scarlet → userspace path; it does not isolate Switchvisor by itself and is not
wire-compatible with iperf. No automatic boot service or package installation.

Build the receiver using the project's Cortex-A57-compatible Scarlet SDK:

```sh
cargo build --manifest-path tests/net-qa/Cargo.toml --release --target aarch64-unknown-scarlet
```

Audit the ELF using `tests/check-isa.py` before transferring it. On Scarlet,
launch the executable manually (example path after transfer):

```sh
/tmp/net-qa 192.168.77.3:18780
```

From the Mac, run one of the following. Restart the receiver for each run:

```sh
python3 tests/net-qa/send.py 192.168.77.3 --mbps 10 --seconds 10 --output .cache/net-qa/paced-10.json
python3 tests/net-qa/send.py 192.168.77.3 --mbps 10 --seconds 10 --burst-ms 16.666667 --output .cache/net-qa/burst-10.json
```

Stop streaming first for a baseline, then separately compare with streaming
active. Start with 5/10/20 Mbps. Larger `--burst-ms` values deliberately stress
buffer headroom at the same average rate. Use `--seconds 300` to look for stalls
that only appear after several minutes. `--size` defaults to 1024 UDP payload
bytes including the 32-byte diagnostic header; reported Mbps excludes IP,
Ethernet and USB framing. The receiver accepts one run and exits.

TCP negotiates a random run ID, packet size, count bound and sender UDP source
port before the receiver acknowledges readiness. It also carries actual sender
totals and the final JSON result, so losing the last UDP packets cannot hide tail
loss. The receiver drains for 500 ms after sender completion. Duplicates do not
increase throughput; late/reordered packets fill previously missing sequence
numbers. Buffers and total run duration are bounded (2 million packets, 600 s).
Control failure is an error, not fabricated packet loss.

Results include actual offered/received Mbps, final loss, duplicates, reordering,
longest missing sequence run, receiver interarrival p95/p99/max, smoothed
interarrival jitter and per-second unique reception counts. Arrival intervals
include deliberate burst gaps and guest scheduling delays; they are not network
latency. Jitter uses differences of sender/receiver monotonic deltas and does not
require synchronized clocks. Per-second bins start at receiver READY, so the
first and last may be partial. Timestamp recording occurs when userspace reads
a packet, not at the USB controller.

The sender reports pacing lateness and local send errors. Late scheduling may
compress subsequent sends into bursts; inspect those fields when comparing
profiles. Any send error makes `valid_path_loss=false` and exits nonzero because
missing packets then include local failures. Neither the sender nor receiver
changes socket buffer sizes, routing, NAT, or system settings.

Host verification:

```sh
cargo test --manifest-path tests/net-qa/Cargo.toml
python3 tests/net-qa/test_probe.py
```
