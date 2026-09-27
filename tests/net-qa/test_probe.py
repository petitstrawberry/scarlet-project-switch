#!/usr/bin/env python3
"""Real socket regression checks, plus sender pacing schedule bounds."""
import importlib.util
import json
from pathlib import Path
import socket
import struct
import subprocess
import time
import unittest

ROOT = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("sender", ROOT / "send.py")
sender = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sender)


class ProbeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        subprocess.run(["cargo", "build", "--quiet", "--manifest-path", str(ROOT / "Cargo.toml")], check=True)

    def start(self):
        with socket.socket() as reserve, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
            reserve.bind(("127.0.0.1", 0))
            udp.bind(("127.0.0.1", 0))
            port, udp_port = reserve.getsockname()[1], udp.getsockname()[1]
        self.process = subprocess.Popen(
            [str(ROOT / "target/debug/net-qa"), f"127.0.0.1:{port}", str(udp_port)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.cleanup)
        self.assertTrue(self.process.stdout.readline().startswith("NETQA listening"))
        return port, udp_port

    def cleanup(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.communicate(timeout=5)

    def test_loss_duplicates_reordering_and_late_tail_over_real_sockets(self):
        port, udp_port = self.start()
        with socket.create_connection(("127.0.0.1", port), timeout=3) as tcp, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
            udp.bind(("127.0.0.1", 0))
            tcp.sendall(f"NETQA1 7 {udp.getsockname()[1]} 64 8 1000\n".encode())
            with tcp.makefile("rb") as reader:
                self.assertEqual(reader.readline(), f"READY {udp_port}\n".encode())
                def send(seq):
                    udp.sendto(struct.pack("!8sQQQ", b"SNETQA01", 7, seq, seq * 1000) + bytes(32), ("127.0.0.1", udp_port))
                for seq in [0, 2, 2, 1, 5]:
                    send(seq)
                tcp.sendall(b"DONE 8 1000000000\n")
                time.sleep(.05)
                send(7)
                report = json.loads(reader.readline())
                self.assertEqual(report["received_packets"], 5)
                self.assertEqual(report["lost_packets"], 3)
                self.assertEqual(report["duplicates"], 1)
                self.assertEqual(report["reordered"], 1)
                self.assertEqual(report["longest_loss_run"], 2)
                self.assertTrue(report["complete"])
        self.assertEqual(self.process.wait(timeout=3), 0)

    def test_abandoned_control_connection_exits_without_fake_report(self):
        port, udp_port = self.start()
        with socket.create_connection(("127.0.0.1", port), timeout=3) as tcp:
            tcp.sendall(b"NETQA1 7 12345 64 8 1000\n")
            self.assertIn(b"READY", tcp.recv(256))
            tcp.shutdown(socket.SHUT_RDWR)
        self.assertNotEqual(self.process.wait(timeout=3), 0)

    def test_sender_paced_and_burst_runs(self):
        for burst in [0, 16.666667]:
            port, _ = self.start()
            result = subprocess.run([
                "python3", str(ROOT / "send.py"), "127.0.0.1", "--port", str(port),
                "--mbps", "1", "--seconds", "1", "--burst-ms", str(burst),
            ], capture_output=True, text=True, timeout=10, check=True)
            report = json.loads(result.stdout)
            self.assertTrue(report["complete"])
            self.assertTrue(report["valid_path_loss"])
            self.assertEqual(report["sent_packets"], report["sender_planned_packets"])
            self.assertEqual(report["received_packets"] + report["lost_packets"], report["sent_packets"])
            self.assertEqual(report["lost_packets"], 0)
            self.assertEqual(self.process.wait(timeout=3), 0)
            self.process.communicate(timeout=3)

    def test_schedules_preserve_totals_and_bound_burst_sizes(self):
        for packets in [1, 61, 12207]:
            for burst in [0, 16.666667, 100]:
                plan = list(sender.groups(packets, 1_000_000_000, burst))
                self.assertEqual(sum(n for _, n in plan), packets)
                self.assertEqual(plan[0][0], 0)
                self.assertLess(plan[-1][0], 1_000_000_000)
                self.assertTrue(all(n > 0 for _, n in plan))
                self.assertTrue(all(a[0] < b[0] for a, b in zip(plan, plan[1:])))


if __name__ == "__main__":
    unittest.main()
