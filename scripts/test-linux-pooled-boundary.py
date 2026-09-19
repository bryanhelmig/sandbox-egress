#!/usr/bin/env python3
"""Disposable Linux proof: fence -> close -> host reset -> same-slot reuse."""
import hashlib
import json
import os
from pathlib import Path
import selectors
import shutil
import socket
import subprocess
import sys
import time

HOST_IP, SLOT_IP, GUEST_IP = "198.19.0.1", "198.19.0.2", "10.0.0.2"
PORT, CAPACITY = 40123, 4


def run(*args):
    result = subprocess.run(args, text=True, capture_output=True, timeout=10)
    if result.returncode:
        raise RuntimeError(f"{args!r}: {result.stderr.strip()}")
    return result.stdout.strip()


def client():
    """A guest process; its namespace disappears with the simulated VM."""
    connections = {}
    for line in sys.stdin:
        command, *args = json.loads(line)
        if command == "send":
            connections[int(args[0])].sendall(b"blackholed-download-proof")
            result = "sent"
        else:
            port, upstream, expected = map(int, args)
            stream = socket.socket()
            stream.settimeout(3)
            stream.bind((GUEST_IP, port))
            stream.connect((HOST_IP, int(os.environ["PROXY_PORT"])))
            response = b""
            try:
                stream.sendall(f"CONNECT 127.0.0.1:{upstream} HTTP/1.1\r\nHost: 127.0.0.1:{upstream}\r\n\r\n".encode())
                while not response.endswith(b"\r\n\r\n") and len(response) < 4096:
                    chunk = stream.recv(1)
                    if not chunk:
                        break
                    response += chunk
            except (ConnectionResetError, BrokenPipeError):
                pass
            if expected == 200:
                assert response.startswith(b"HTTP/1.1 200"), response
                stream.sendall(b"generation-proof")
                echo = b""
                while len(echo) < len(b"generation-proof"):
                    chunk = stream.recv(len(b"generation-proof") - len(echo))
                    assert chunk, "echo ended early"
                    echo += chunk
                assert echo == b"generation-proof", echo
                connections[port] = stream
            else:
                assert not response.startswith(b"HTTP/1.1 200"), response
                if expected == 403:
                    assert response.startswith(b"HTTP/1.1 403"), response
                stream.close()
            result = "ok"
        print(json.dumps(result), flush=True)


class Process:
    def __init__(self, *args, environment=None):
        self.process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=None, env=environment, bufsize=0)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.buffer = b""

    def line(self, prefix=""):
        deadline = time.monotonic() + 5
        while True:
            if b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                line = line.decode()
                if line.startswith(prefix):
                    return line
                print(line, flush=True)
                continue
            remaining = deadline - time.monotonic()
            assert remaining > 0 and self.selector.select(remaining), f"missing {prefix!r}"
            data = os.read(self.process.stdout.fileno(), 4096)
            assert data, f"process exited awaiting {prefix!r}: {self.process.poll()}"
            self.buffer += data

    def command(self, command):
        self.process.stdin.write((command + "\n").encode())
        self.process.stdin.flush()

    def stop(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=5)
        self.selector.close()
        self.process.stdin.close()
        self.process.stdout.close()


class Boundary:
    def __init__(self):
        suffix = str(os.getpid())
        self.host, self.slot, self.guest = [p + suffix for p in ("sph", "sps", "spg")]
        self.processes = []
        self.namespaces = []

    def ns(self, namespace):
        run("ip", "netns", "add", namespace)
        self.namespaces.append(namespace)
        run("ip", "-n", namespace, "link", "set", "lo", "up")

    def execute(self, namespace, *args):
        return run("ip", "netns", "exec", namespace, *args)

    def nft(self, namespace, expression):
        return self.execute(namespace, "nft", expression)

    def link(self, first, first_name, first_ip, second, second_name, second_ip):
        run("ip", "-n", first, "link", "add", first_name, "type", "veth", "peer", "name", second_name)
        run("ip", "-n", first, "link", "set", second_name, "netns", second)
        for ns, name, ip in [(first, first_name, first_ip), (second, second_name, second_ip)]:
            run("ip", "-n", ns, "addr", "add", ip + "/30", "dev", name)
            run("ip", "-n", ns, "link", "set", name, "up")

    def make_guest(self):
        self.ns(self.guest)
        self.link(self.slot, "guest", "10.0.0.1", self.guest, "eth0", GUEST_IP)
        run("ip", "-n", self.guest, "route", "add", "default", "via", "10.0.0.1")

    def start(self, namespace, *args, environment=None):
        process = Process("ip", "netns", "exec", namespace, *args, environment=environment)
        self.processes.append(process)
        return process

    def sockets(self):
        return self.execute(self.host, "ss", "-Htan", "dst", SLOT_IP)

    def conntrack(self):
        return self.execute(self.slot, "conntrack", "-L")

    def reset(self, omit):
        # This reference reset is exclusively scoped to these disposable namespaces.
        if omit != "sockets":
            self.execute(self.host, "ss", "-Ktan", "dst", SLOT_IP)
        if omit != "conntrack":
            self.execute(self.slot, "conntrack", "-F")
        if self.sockets():
            raise AssertionError("reset left host TCP sockets")
        if self.conntrack():
            raise AssertionError("reset left slot conntrack entries")

    def cleanup(self):
        for process in reversed(self.processes):
            process.stop()
        for namespace in reversed(self.namespaces):
            subprocess.run(["ip", "netns", "del", namespace], capture_output=True, timeout=5)
        remaining = {line.split()[0] for line in run("ip", "netns", "list").splitlines()}
        assert not remaining.intersection(self.namespaces), "cleanup left a named namespace"


def scenario(omit):
    fixture = Path(os.environ["SANDBOX_EGRESS_HOST_FIXTURE"])
    expected = os.environ["SANDBOX_EGRESS_HOST_FIXTURE_SHA256"]
    assert hashlib.sha256(fixture.read_bytes()).hexdigest() == expected, "stale fixture hash"
    boundary = Boundary()
    try:
        boundary.ns(boundary.host)
        boundary.ns(boundary.slot)
        boundary.link(boundary.host, "slot", HOST_IP, boundary.slot, "uplink", SLOT_IP)
        boundary.execute(boundary.slot, "sysctl", "-w", "net.ipv4.ip_forward=1")
        boundary.nft(boundary.slot, "add table ip pool")
        boundary.nft(boundary.slot, "add chain ip pool postrouting { type nat hook postrouting priority srcnat; policy accept; }")
        boundary.nft(boundary.slot, f"add rule ip pool postrouting ip saddr {GUEST_IP} snat to {SLOT_IP}")
        boundary.nft(boundary.slot, "add chain ip pool forward { type filter hook forward priority filter; policy drop; }")
        boundary.nft(boundary.slot, "add rule ip pool forward ct state established,related accept")
        boundary.make_guest()
        slot_inode = Path("/run/netns", boundary.slot).stat().st_ino
        uplink = json.loads(run("ip", "-n", boundary.slot, "-j", "link", "show", "uplink"))[0]["ifindex"]
        proxy = boundary.start(boundary.host, str(fixture), HOST_IP + ":0", SLOT_IP, "pooled")
        endpoint = proxy.line("PROXY_ADDR=").split("=", 1)[1]
        upstream = int(proxy.line("UPSTREAM_ADDR=").rsplit(":", 1)[1])
        replacement = int(proxy.line("REPLACEMENT_ADDR=").rsplit(":", 1)[1])
        proxy_port = endpoint.rsplit(":", 1)[1]
        boundary.nft(boundary.slot, f"add rule ip pool forward ip daddr {HOST_IP} tcp dport {proxy_port} accept")
        boundary.nft(boundary.host, "add table inet pool")
        boundary.nft(boundary.host, "add chain inet pool output { type filter hook output priority filter; policy accept; }")
        environment = dict(os.environ, PROXY_PORT=proxy_port)

        def new_client():
            return boundary.start(boundary.guest, sys.executable, __file__, "--client", environment=environment)

        def request(worker, *args):
            worker.command(json.dumps(args))
            assert json.loads(worker.line()) in ("ok", "sent")

        guest = new_client()
        for port in range(PORT, PORT + CAPACITY):
            request(guest, "open", port, upstream, 200)
        proxy.command("occupied")
        print(proxy.line("OCCUPIED"), flush=True)
        assert boundary.conntrack(), "fixture must populate slot conntrack"
        boundary.nft(boundary.host, f"add rule inet pool output ip daddr {SLOT_IP} tcp sport {proxy_port} tcp dport {PORT} drop")
        request(guest, "send", PORT)
        deadline = time.monotonic() + 3
        while True:
            rows = [row.split() for row in boundary.sockets().splitlines()]
            if any(row[0] == "ESTAB" and int(row[2]) > 0 and row[-1].endswith(f":{PORT}") for row in rows):
                break
            assert time.monotonic() < deadline, "no unacknowledged download observed"
            time.sleep(0.01)
        # Fence before destroying the simulated VM's own kernel network state.
        run("ip", "-n", boundary.slot, "link", "set", "guest", "down")
        guest.stop()
        run("ip", "-n", boundary.slot, "link", "del", "guest")
        run("ip", "netns", "del", boundary.guest)
        boundary.namespaces.remove(boundary.guest)
        proxy.command("close")
        print(proxy.line("FINAL generation=1 "), flush=True)
        sockets = boundary.sockets()
        assert f"{SLOT_IP}:{PORT}" in sockets and "FIN-WAIT-1" in sockets, sockets
        assert boundary.conntrack(), "conntrack vanished before reset control"
        print("BASELINE: certified close leaves old kernel state", flush=True)
        boundary.reset(omit)
        print("RESET: zero host TCP sockets and zero slot conntrack entries", flush=True)
        assert Path("/run/netns", boundary.slot).stat().st_ino == slot_inode
        assert json.loads(run("ip", "-n", boundary.slot, "-j", "link", "show", "uplink"))[0]["ifindex"] == uplink
        assert SLOT_IP in run("ip", "-n", boundary.slot, "addr", "show", "uplink")
        proxy.command("attach")
        assert proxy.line("ATTACHED generation=2 ").endswith("endpoint=" + endpoint)
        boundary.nft(boundary.host, "flush chain inet pool output")
        boundary.make_guest()
        guest = new_client()
        request(guest, "open", PORT + 10, upstream, 403)
        for port in range(PORT, PORT + CAPACITY):
            request(guest, "open", port, replacement, 200)
        request(guest, "open", PORT + CAPACITY, replacement, 0)
        proxy.command("capacity")
        print(proxy.line("CAPACITY"), flush=True)
        run("ip", "-n", boundary.slot, "link", "set", "guest", "down")
        guest.stop()
        run("ip", "-n", boundary.slot, "link", "del", "guest")
        run("ip", "netns", "del", boundary.guest)
        boundary.namespaces.remove(boundary.guest)
        proxy.command("finish")
        print(proxy.line("FINAL generation=2 "), flush=True)
        print(proxy.line("BYSTANDER"), flush=True)
        assert proxy.process.wait(timeout=5) == 0
        boundary.reset(None)
        print("POOLED: same namespace/IP/ports, new policy, full budget, unrelated tunnel preserved", flush=True)
    finally:
        boundary.cleanup()


def main():
    if not __debug__:
        raise RuntimeError("run this certificate without Python optimization")
    if sys.argv[1:] == ["--client"]:
        client()
        return
    assert sys.platform == "linux" and os.geteuid() == 0, "requires disposable privileged Linux"
    for binary in ("ip", "nft", "ss", "conntrack", "sysctl"):
        assert shutil.which(binary), f"missing {binary}"
    if sys.argv[1:]:
        assert sys.argv[1:] in (["--omit", "sockets"], ["--omit", "conntrack"])
        scenario(sys.argv[2])
        return
    for omit, expected in [("sockets", "reset left host TCP sockets"),
                           ("conntrack", "reset left slot conntrack entries")]:
        try:
            scenario(omit)
        except AssertionError as error:
            assert str(error) == expected, f"wrong negative-control failure: {error}"
            print(f"NEGATIVE CONTROL: omitting {omit} prevented reuse", flush=True)
        else:
            raise AssertionError(f"omitting {omit} incorrectly permitted reuse")
    scenario(None)


if __name__ == "__main__":
    main()
