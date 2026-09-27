#!/usr/bin/env python3
"""UDP load generator for net-qa (not compatible with iperf's wire protocol)."""
import argparse
import json
import math
from pathlib import Path
import secrets
import socket
import struct
import time


def groups(packets, duration_ns, burst_ms):
    count = min(packets, math.ceil(duration_ns / (burst_ms * 1_000_000))) if burst_ms else packets
    for group in range(count):
        yield group * duration_ns // count, (group + 1) * packets // count - group * packets // count


def run(args):
    for value in (args.mbps, args.seconds, args.burst_ms):
        if not math.isfinite(value):
            raise ValueError("rate, duration and burst interval must be finite")
    if not (0 < args.mbps <= 1000 and 1 <= args.seconds <= 600
            and 64 <= args.size <= 1472 and 0 <= args.burst_ms <= 1000
            and 0 <= args.spin_us <= 1000):
        raise ValueError("unsupported rate/duration/packet size/burst interval/spin time")
    packets = math.floor(args.mbps * 1_000_000 * args.seconds / (8 * args.size))
    if not 1 <= packets <= 2_000_000:
        raise ValueError("run must contain 1..2000000 packets")
    duration_ns = round(args.seconds * 1_000_000_000)
    run_id = secrets.randbits(64)
    target_ip = socket.gethostbyname(args.host)
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp, socket.create_connection(
            (target_ip, args.port), timeout=5) as control:
        # Route-selected local address also works for loopback tests.
        udp.bind((control.getsockname()[0], 0))
        control.settimeout(args.seconds + 15)
        reader = control.makefile("rb")
        config = f"NETQA1 {run_id} {udp.getsockname()[1]} {args.size} {packets} {math.ceil(args.seconds * 1000)}\n"
        control.sendall(config.encode())
        ready = reader.readline(4096).decode().strip().split()
        if len(ready) != 2 or ready[0] != "READY":
            raise RuntimeError(f"receiver did not become ready: {ready}")
        udp.connect((target_ip, int(ready[1])))
        udp.settimeout(1)
        payload = bytearray(b"\x5a" * args.size)
        seq = 0
        max_lateness = 0
        late_over_1ms = 0
        send_errors = 0
        successful = 0
        started = time.monotonic_ns()
        spin_ns = args.spin_us * 1000
        for offset, count in groups(packets, duration_ns, args.burst_ms):
            target = started + offset
            remaining = target - time.monotonic_ns()
            # Long sleeps can be coalesced by macOS by several milliseconds.
            # Short sleeps plus a bounded final spin keep burst timing observable.
            while remaining > spin_ns:
                time.sleep(min((remaining - spin_ns) / 1_000_000_000, .001))
                remaining = target - time.monotonic_ns()
            while time.monotonic_ns() < target:
                pass
            lateness = max(0, time.monotonic_ns() - target)
            max_lateness = max(max_lateness, lateness)
            late_over_1ms += lateness > 1_000_000
            for _ in range(count):
                struct.pack_into("!8sQQQ", payload, 0, b"SNETQA01", run_id, seq,
                                 time.monotonic_ns() - started)
                try:
                    if udp.send(payload) != len(payload):
                        send_errors += 1
                    else:
                        successful += 1
                except OSError:
                    send_errors += 1
                seq += 1
            # Do not turn a stalled sender into an unbounded test. Actual attempted
            # count, not the plan, is sent over TCP so an unsent tail is not loss.
            if time.monotonic_ns() - started > duration_ns + 2_000_000_000:
                break
        # Include the final pacing interval in rate calculations.
        remaining = started + duration_ns - time.monotonic_ns()
        if remaining > 0:
            time.sleep(remaining / 1_000_000_000)
        elapsed = time.monotonic_ns() - started
        control.sendall(f"DONE {seq} {elapsed}\n".encode())
        response = reader.readline(32768).decode().strip()
        if response.startswith("ERROR"):
            raise RuntimeError(response)
        report = json.loads(response)
        if report.get("run_id") != run_id or report.get("sent_packets") != seq:
            raise RuntimeError("receiver returned mismatched run/totals")
        report.update(
            target=args.host, requested_mbps=args.mbps, requested_seconds=args.seconds,
            burst_ms=args.burst_ms, mode="burst" if args.burst_ms else "paced",
            sender_planned_packets=packets, sender_successful_packets=successful,
            sender_send_errors=send_errors, sender_max_lateness_us=max_lateness / 1000,
            sender_groups_late_over_1ms=late_over_1ms,
            sender_max_packets_per_burst=max(n for _, n in groups(packets, duration_ns, args.burst_ms)),
            # Receiver missing packets include local send failures: do not label
            # that result as measured path loss.
            valid_path_loss=(send_errors == 0),
        )
        return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("host")
    parser.add_argument("--port", type=int, default=18780)
    parser.add_argument("--mbps", type=float, default=10)
    parser.add_argument("--seconds", type=float, default=10)
    parser.add_argument("--size", type=int, default=1024, help="UDP payload bytes, including 32-byte probe header")
    parser.add_argument("--burst-ms", type=float, default=0, help="0: pace each packet; 16.666667: 60 frame/s-like bursts")
    parser.add_argument("--spin-us", type=int, default=100, help="busy-wait final portion of pacing interval on sender")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        report = run(args)
    except (OSError, ValueError, RuntimeError) as error:
        parser.exit(1, f"net-qa: {error}\n")
    text = json.dumps(report, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(text)
    print(text, end="")
    if not report["valid_path_loss"]:
        parser.exit(1, "net-qa: local send errors; loss cannot be attributed to the network\n")


if __name__ == "__main__":
    main()
