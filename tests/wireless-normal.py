#!/usr/bin/env python3
"""Exercise installed wirelessd/wifictl on the normal SD boot via its console.

Uses an already saved network; no passwords, RAM deployment or SD modifications.
Run while the normal Switchvisor entry is running, with no other UART reader.
"""
import argparse
import fcntl
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import select
import shlex
import socket
import subprocess
import termios
import time
import tty
import threading

ANSI = re.compile(rb"\x1b\[[0-9;]*[A-Za-z]")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--ssid", required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--console", default="/dev/cu.usbmodemSWV00011")
    p.add_argument("--ctl", default="switchvisorctl")
    p.add_argument("--cycles", type=int, default=2, help="Reconnect cycles; 0 keeps the current link for a transfer benchmark")
    p.add_argument("--development-kernel", action="store_true",
                   help="Test an uploaded normal kernel with the persistent SD root")
    p.add_argument("--verify-irq", action="store_true", help="Measure idle CPU and require IRQ 130 progress")
    p.add_argument("--verify-daemon-events", action="store_true", help="Require wirelessd and the kernel radio actor to stay idle")
    p.add_argument("--idle-seconds", type=float, default=30)
    p.add_argument("--tcp-bind", type=ipaddress.IPv4Address,
                   help="Host LAN IPv4 for the TCP download/upload integrity test")
    p.add_argument("--tcp-bytes", type=int, default=262144,
                   help="Bytes per TCP direction, also records transfer time and driver CPU")
    a = p.parse_args()
    if not 0 <= a.cycles <= 5:
        p.error("cycles must be between 0 and 5")
    if not 5 <= a.idle_seconds <= 60:
        p.error("idle-seconds must be between 5 and 60")
    if not 16384 <= a.tcp_bytes <= 8 * 1024 * 1024:
        p.error("tcp-bytes must be between 16 KiB and 8 MiB")
    a.output.mkdir(parents=True, exist_ok=False)
    status = subprocess.check_output([a.ctl, "status"], text=True, timeout=10)
    assert "state=guest-running\n" in status
    if a.development_kernel:
        assert "fallback=disabled\n" in status and "loader=disabled\n" in status, status
    else:
        assert "fallback=enabled\n" in status and "loader=disabled\n" in status, "normal SD boot required"
    (a.output / "monitor-before.txt").write_text(status)
    fd = os.open(a.console, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    old = termios.tcgetattr(fd)
    result = {"pass": False, "normal_sd_boot": not a.development_kernel, "reconnect_cycles": 0}
    try:
        fcntl.ioctl(fd, termios.TIOCEXCL)
        tty.setraw(fd)
        with (a.output / "serial.log").open("wb", buffering=0) as log:
            def read():
                data = os.read(fd, 65536)
                if not data:
                    raise RuntimeError("console disconnected")
                log.write(data)
                return data

            while select.select([fd], [], [], 0.1)[0]:
                read()

            def command(text, timeout=95, interrupt_after_replies=None):
                os.write(fd, (text + "\n").encode())
                raw = bytearray()
                clean = ""
                interrupted = False
                deadline = time.monotonic() + timeout
                while time.monotonic() < deadline:
                    if not select.select([fd], [], [], 0.2)[0]:
                        continue
                    raw.extend(read())
                    clean = ANSI.sub(b"", bytes(raw)).replace(b"\r", b"").decode(errors="replace")
                    if any(marker in clean for marker in ("Panic occurred", "panicked at", "brcmfmac: stopped:")):
                        raise RuntimeError("guest failure: " + clean[-2500:])
                    if interrupt_after_replies and not interrupted and clean.count("[ping] reply ") >= interrupt_after_replies:
                        os.write(fd, b"\x03")
                        interrupted = True
                    # Async kernel logs can redraw the prompt while the shell
                    # is still echoing input. Only accept a prompt after the
                    # complete submitted command line has been entered.
                    entered = clean.find("# " + text + "\n")
                    if entered >= 0 and re.search(r"\n# (?=$|\n)", clean[entered + len(text) + 3:]):
                        return clean
                raise TimeoutError("console command did not finish: " + text + "\n" + clean[-1500:])

            connected = command("wifictl status")
            assert "STATE Connected\n" in connected and "CAN_SCAN 0\n" in connected, connected
            initial_link = re.search(r"^LINK wlan0 (\d+\.\d+\.\d+\.\d+) (\d+\.\d+\.\d+\.\d+)$", connected, re.M)
            assert initial_link and initial_link[1] != "0.0.0.0", connected
            result["address"], result["gateway"] = initial_link.groups()
            def irq_count():
                listing = command("cat /dev/interrupts")
                match = re.search(r"^130\s+\d+\s+(\d+)$", listing, re.M)
                assert match, listing
                return int(match[1])

            if a.verify_irq or a.verify_daemon_events:
                def cpu_sample():
                    started = time.monotonic()
                    listing = command("ps -l")
                    match = re.search(r"\s(\d+):(\d+)\.(\d+)\s+brcmfmac\s", listing)
                    assert match, listing
                    cpu = int(match[1]) * 60 + int(match[2]) + int(match[3]) / 1000
                    actor = 0.0
                    daemon = 0.0
                    for m in re.finditer(r"\s(\d+):(\d+)\.(\d+)\s+(wireless|/bin/wirelessd)\s", listing):
                        elapsed = int(m[1]) * 60 + int(m[2]) + int(m[3]) / 1000
                        if m[4] == "wireless": actor += elapsed
                        else: daemon += elapsed
                    return started, cpu, actor, daemon
                result["irq_before"] = irq_count()
                assert result["irq_before"] > 0, "no hardware IRQ deliveries"
                before = cpu_sample()
                time.sleep(a.idle_seconds)
                after = cpu_sample()
                result["idle_wall_seconds"] = after[0] - before[0]
                result["idle_cpu_seconds"] = after[1] - before[1]
                result["idle_cpu_percent_one_core"] = 100 * result["idle_cpu_seconds"] / result["idle_wall_seconds"]
                assert result["idle_cpu_percent_one_core"] < 1, "idle worker CPU exceeded 1% of one core"
                if a.verify_daemon_events:
                    for index, name in [(2, "wireless_actor"), (3, "wirelessd")]:
                        percent = 100 * (after[index] - before[index]) / result["idle_wall_seconds"]
                        result[name + "_idle_cpu_percent_one_core"] = percent
                        assert percent < 0.5, name + " idle CPU exceeded 0.5% of one core"
            rejected = command("wifictl scan", timeout=5)
            assert "Disconnect before scanning" in rejected, rejected
            assert "STATE Connected\n" in command("wifictl status")
            for cycle in range(a.cycles):
                down = command("wifictl disconnect")
                assert "STATE Disconnected\n" in down, down
                # netcfgd reconciles once per second; poll until retirement.
                for _ in range(20):
                    listing = command("netcfg --list")
                    wlan = listing.split("Interface: wlan0", 1)[1]
                    if "IPv4: (none)" in wlan and "Default route: (none)" in wlan:
                        break
                    time.sleep(0.2)
                else:
                    raise AssertionError("old IPv4 configuration survived disconnect")
                if cycle == 0:
                    scanned = command("wifictl scan")
                    assert "STATE Choose a network\n" in scanned and "AP " + a.ssid.encode().hex() + " " in scanned, scanned
                    result["scan_networks"] = len(re.findall(r"^AP ", scanned, re.M))
                connected = command("wifictl connect " + shlex.quote(a.ssid))
                match = re.search(r"^LINK wlan0 (\d+\.\d+\.\d+\.\d+) (\d+\.\d+\.\d+\.\d+)$", connected, re.M)
                assert "STATE Connected\n" in connected and match and match[1] != "0.0.0.0", connected
                result["address"], result["gateway"] = match.groups()
                result["reconnect_cycles"] += 1
                print("NORMAL_WIFI_RECONNECT_PASS", cycle + 1, flush=True)
            dns = command("resolve example.com")
            assert re.search(r"^\d+\.\d+\.\d+\.\d+$", dns, re.M), dns
            result["dns_lookup"] = True
            ping = command("ping " + result["gateway"], timeout=20, interrupt_after_replies=5)
            assert "5 packets transmitted, 5 packets received, 0% packet loss" in ping, ping
            result["gateway_ping"] = "5/5"
            if a.tcp_bind:
                payload = (bytes(range(256)) * ((a.tcp_bytes + 255) // 256))[:a.tcp_bytes]
                received = bytearray()
                errors = []
                listener = socket.socket()
                listener.settimeout(25)
                listener.bind((str(a.tcp_bind), 0))
                listener.listen(2)
                port = listener.getsockname()[1]

                def serve():
                    try:
                        with listener.accept()[0] as stream:
                            stream.settimeout(25)
                            stream.sendall(payload)
                            stream.shutdown(socket.SHUT_WR)
                        with listener.accept()[0] as stream:
                            stream.settimeout(25)
                            while len(received) < len(payload):
                                data = stream.recv(min(16384, len(payload) - len(received)))
                                if not data:
                                    break
                                received.extend(data)
                            stream.sendall(b"WIFI_TCP_INTEGRITY_OK\n")
                    except Exception as error:
                        errors.append(str(error))
                    finally:
                        listener.close()

                server = threading.Thread(target=serve, daemon=True)
                server.start()
                try:
                    cpu_before = cpu_sample() if a.verify_irq or a.verify_daemon_events else None
                    started = time.monotonic()
                    command(f"nc -n -w 25 {a.tcp_bind} {port} < /dev/null > /tmp/wifi-irq-tcp", timeout=35)
                    result["tcp_download_seconds"] = time.monotonic() - started
                    started = time.monotonic()
                    returned = command(f"nc -n -w 25 {a.tcp_bind} {port} < /tmp/wifi-irq-tcp", timeout=35)
                    result["tcp_upload_seconds"] = time.monotonic() - started
                    if cpu_before:
                        cpu_after = cpu_sample()
                        result["tcp_driver_cpu_seconds"] = cpu_after[1] - cpu_before[1]
                        result["tcp_driver_cpu_ms_per_mib"] = result["tcp_driver_cpu_seconds"] * 1000 / (2 * a.tcp_bytes / 1048576)
                    server.join(timeout=1)
                    assert not server.is_alive() and not errors, errors
                    assert received == payload and "WIFI_TCP_INTEGRITY_OK\n" in returned, "TCP round-trip data mismatch"
                    result["tcp_round_trip_bytes"] = len(received)
                    result["tcp_sha256"] = hashlib.sha256(received).hexdigest()
                finally:
                    listener.close()
                    command("rm /tmp/wifi-irq-tcp", timeout=5)
            if a.verify_irq:
                result["irq_after"] = irq_count()
                assert result["irq_after"] > result["irq_before"], "communication did not deliver new IRQs"
            result["pass"] = True
    except Exception as error:
        result["error"] = str(error)
        raise
    finally:
        (a.output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        try:
            termios.tcsetattr(fd, termios.TCSANOW, old)
            fcntl.ioctl(fd, termios.TIOCNXCL)
        finally:
            os.close(fd)
    (a.output / "monitor-after.txt").write_text(subprocess.check_output([a.ctl, "status"], text=True, timeout=10))
    print("NORMAL_WIFI_PASS", json.dumps(result), flush=True)


if __name__ == "__main__":
    main()
