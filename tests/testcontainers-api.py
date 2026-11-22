#!/usr/bin/env python3
"""What Testcontainers (docker-java, .NET, Go) needs of the engine API, asked
the way those clients ask: raw HTTP/1.1 on one kept-alive connection.

    tests/testcontainers-api.py SOCKET [version|cmd|keepalive|close|follow|follow-created|timestamps|exec|endpoints|copy-after-start ...]

Prints one line per check, "ok <name>" or "FAIL <name>: why", and exits 1 if
any failed. The containers it makes (tc-api-*) are left running.
"""
import io
import json
import os
import re
import socket
import sys
import tarfile
import time

SOCK = sys.argv[1]
IMAGE = "alpine:latest"


class Conn:
    """One HTTP/1.1 connection, kept for every request, as a client pool keeps it."""

    def __init__(self, timeout=30):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.settimeout(timeout)
        self.s.connect(SOCK)
        self.buf = b""

    def _fill(self):
        d = self.s.recv(65536)
        if not d:
            raise EOFError("the engine closed the connection")
        self.buf += d

    def line(self):
        while b"\r\n" not in self.buf:
            self._fill()
        l, self.buf = self.buf.split(b"\r\n", 1)
        return l

    def take(self, n):
        while len(self.buf) < n:
            self._fill()
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def send(self, method, path, body=None, ctype="application/json", extra=""):
        if isinstance(body, (dict, list)):
            body = json.dumps(body).encode()
        head = f"{method} /v1.32{path} HTTP/1.1\r\nHost: localhost:2375\r\nAccept: application/json\r\n{extra}"
        if body is not None:
            head += f"Content-Type: {ctype}\r\nContent-Length: {len(body)}\r\n"
        self.s.sendall(head.encode() + b"\r\n" + (body or b""))

    def head(self):
        status = int(self.line().split()[1])
        headers = {}
        while True:
            l = self.line()
            if not l:
                return status, headers
            k, v = l.decode().split(":", 1)
            headers[k.strip().lower()] = v.strip()

    def chunks(self):
        """The chunks of a chunked body, as they arrive."""
        while True:
            n = int(self.line().split(b";")[0], 16)
            if n == 0:
                self.line()
                return
            data = self.take(n)
            self.take(2)
            yield data

    def request(self, method, path, body=None, ctype="application/json"):
        self.send(method, path, body, ctype)
        status, headers = self.head()
        if headers.get("transfer-encoding") == "chunked":
            data = b"".join(self.chunks())
        else:
            data = self.take(int(headers.get("content-length", "0"))) if status not in (204, 304) else b""
        return status, headers, data


def frames(chunks):
    """Docker's multiplexed stream: [stream, 0, 0, 0, len u32 BE] data."""
    body = b""
    for c in chunks:
        body += c
        while len(body) >= 8:
            if body[0] not in (0, 1, 2) or body[1:4] != b"\0\0\0":
                raise ValueError(f"bad frame header {body[:8]!r}")
            n = int.from_bytes(body[4:8], "big")
            if len(body) < 8 + n:
                break
            yield body[0], body[8:8 + n]
            body = body[8 + n:]


def fresh(c, name, cmd, **extra):
    c.request("DELETE", f"/containers/{name}?force=1")
    status, _, data = c.request("POST", f"/containers/create?name={name}", {"Image": IMAGE, "Cmd": cmd, **extra})
    assert status == 201, f"create {status} {data[:200]!r}"
    return json.loads(data)["Id"]


def check_version():
    c = Conn()
    _, _, v = c.request("GET", "/version")
    v = json.loads(v)
    major = int(v["Version"].split(".")[0])
    # Testcontainers refuses an engine below 1.6.0; Kiri's own 0.x read as one.
    assert major >= 20, f"Version {v['Version']} is not a Docker Engine version"
    assert v.get("KiriVersion"), "no KiriVersion"
    assert any(x["Name"] == "Kiri" for x in v["Components"]), "no Kiri component"
    _, _, i = c.request("GET", "/info")
    assert json.loads(i)["ServerVersion"] == v["Version"], "info's ServerVersion differs"
    return f"engine {v['Version']}, Kiri {v['KiriVersion']}"


def check_cmd():
    # Testcontainers sends "Cmd": [] when no command is set (ryuk, redis…):
    # Docker runs the image's CMD (#121).
    c = Conn()
    fresh(c, "tc-api-cmd", [])
    _, _, d = c.request("GET", "/containers/tc-api-cmd/json")
    cfg = json.loads(d)["Config"]
    assert cfg["Cmd"] == ["/bin/sh"], f"Cmd {cfg['Cmd']}"
    status, _, data = c.request("POST", "/containers/tc-api-cmd/start")
    assert status == 204, f"start {status} {data[:200]!r}"
    return "Cmd [] runs the image's CMD"


def check_keepalive():
    # Testcontainers copies files into a created container, then starts it on
    # the same pooled connection (MySQLContainer's my.cnf).
    c = Conn()
    fresh(c, "tc-api-cp", ["sleep", "600"])
    tar = io.BytesIO()
    with tarfile.open(fileobj=tar, mode="w") as t:
        data = b"[mysqld]\n"
        info = tarfile.TarInfo("my.cnf")
        info.size = len(data)
        t.addfile(info, io.BytesIO(data))
    status, headers, _ = c.request("PUT", "/containers/tc-api-cp/archive?path=/tmp", tar.getvalue(), "application/x-tar")
    assert status == 200, f"archive {status}"
    closing = headers.get("connection", "").lower() == "close"
    try:
        status, _, data = c.request("POST", "/containers/tc-api-cp/start")
    except (EOFError, BrokenPipeError, ConnectionResetError) as e:
        raise AssertionError(f"start on the same connection: {e}" + ("" if closing else " (no Connection: close was sent)"))
    assert status == 204, f"start {status} {data[:200]!r}"
    return "start after a copy on one connection"


def ends_soon(c, within=3):
    """Whether the engine closes the connection within `within` seconds,
    with nothing more sent."""
    c.s.settimeout(within)
    try:
        while True:
            d = c.s.recv(65536)
            if not d:
                return not c.buf
            c.buf += d
    except socket.timeout:
        return False


def check_close():
    # Testcontainers for .NET (Docker.DotNet) sends Connection: close on every
    # request and reads a 204, which has no length, to the end of the stream:
    # the engine must close, or the start hangs until the client's timeout
    # (#520, #479). An HTTP/1.0 request closes the same way unless it asks
    # to keep the connection.
    c = Conn()
    fresh(c, "tc-api-close", ["sleep", "600"])
    t0 = time.time()
    c.send("POST", "/containers/tc-api-close/start", extra="Connection: close\r\n")
    status, headers = c.head()
    assert status == 204, f"start {status}"
    assert headers.get("connection", "").lower() == "close", f"start answered without Connection: close ({headers})"
    assert ends_soon(c), "the connection stayed open after a 204 to a Connection: close request"
    took = time.time() - t0
    # The container is running now: a second start answers 304, also bodiless.
    c = Conn()
    c.s.sendall(b"POST /v1.44/containers/tc-api-close/start HTTP/1.0\r\nContent-Length: 0\r\n\r\n")
    status, _ = c.head()
    assert status == 304, f"second start {status}"
    assert ends_soon(c), "the connection stayed open after a 304 to an HTTP/1.0 request"
    # Without the header the connection stays for the next request.
    c = Conn()
    status, headers, _ = c.request("POST", "/containers/tc-api-close/start")
    assert status == 304 and "connection" not in headers, f"kept-alive start {status} {headers}"
    assert not ends_soon(c, 1), "a kept-alive connection was closed"
    status, _, _ = c.request("GET", "/containers/tc-api-close/json")
    assert status == 200, f"inspect after it {status}"
    return f"closed {took:.2f} s after a Connection: close start; HTTP/1.0 closes, keep-alive stays"


def wait_lines(c, cid, pattern, times, timeout):
    """LogMessageWaitStrategy: follow=true since=0, count frames matching."""
    rx = re.compile(r"(?s).*" + pattern + r".*\s")
    c.s.settimeout(timeout)
    c.send("GET", f"/containers/{cid}/logs?since=0&stdout=true&stderr=true&follow=true")
    status, headers = c.head()
    assert status == 200, f"logs {status}"
    assert headers.get("content-type") == "application/vnd.docker.multiplexed-stream", headers.get("content-type")
    seen = 0
    t0 = time.time()
    try:
        for _, data in frames(c.chunks()):
            seen += bool(rx.fullmatch(data.decode("utf-8", "replace")))
            if seen >= times:
                return time.time() - t0
    except socket.timeout:
        pass
    raise AssertionError(f"{seen} of {times} matching lines")


SCRIPT = "echo starting; sleep 2; echo 'ready for connections'; sleep 2; echo 'ready for connections' >&2; sleep 600"


def check_follow():
    c = Conn()
    cid = fresh(c, "tc-api-logs", ["sh", "-c", SCRIPT])
    status, _, _ = c.request("POST", f"/containers/{cid}/start")
    assert status == 204, f"start {status}"
    took = wait_lines(c, cid, "ready for connections", 2, 30)
    return f"2 lines seen through follow in {took:.1f} s"


def check_follow_created():
    # A follow opened before the container starts sees what it prints after.
    c = Conn()
    cid = fresh(c, "tc-api-logs2", ["sh", "-c", SCRIPT])
    starter = Conn()
    import threading
    threading.Timer(0.5, lambda: starter.request("POST", f"/containers/{cid}/start")).start()
    took = wait_lines(c, cid, "ready for connections", 2, 30)
    return f"2 lines seen through a follow opened while created, in {took:.1f} s"


def check_timestamps():
    c = Conn()
    cid = fresh(c, "tc-api-ts", ["echo", "hi"])
    c.request("POST", f"/containers/{cid}/start")
    c.request("POST", f"/containers/{cid}/wait")
    _, _, data = c.request("GET", f"/containers/{cid}/logs?stdout=1&timestamps=1")
    lines = [d for _, d in frames([data])]
    assert len(lines) == 1, lines
    # One RFC 3339 stamp with nanoseconds, then the line.
    assert re.fullmatch(rb"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{9}Z hi\n", lines[0]), lines[0]
    return "one RFC 3339 stamp"


def exec_start(c, cid, cmd, extra=""):
    """Creates an exec and starts it on the same connection; returns the exec
    id, the reply's status and headers, and everything it streamed to EOF."""
    status, _, reply = c.request("POST", f"/containers/{cid}/exec", {"AttachStdout": True, "AttachStderr": True, "Cmd": cmd})
    assert status == 201, f"exec create {status} {reply[:200]!r}"
    eid = json.loads(reply)["Id"]
    c.send("POST", f"/exec/{eid}/start", {"Detach": False, "Tty": False}, extra=extra)
    status, headers = c.head()
    out = c.buf
    c.buf = b""
    try:
        while True:
            chunk = c.s.recv(65536)
            if not chunk:
                break
            out += chunk
    except socket.timeout:
        raise AssertionError("the exec's stream never closed")
    return eid, status, headers, out


def check_exec():
    # docker-java's execInContainer (MongoDBContainer's rs.initiate) sends no
    # Upgrade header. Docker answers 200 with the stream as the body, closed
    # when the exec ends; a 101 there made the client stop early and read an
    # exec with no ExitCode. The docker CLI asks for the upgrade and gets 101.
    c = Conn()
    cid = fresh(c, "tc-api-exec", ["sleep", "600"])
    status, _, reply = c.request("POST", f"/containers/{cid}/start")
    assert status == 204, f"start {status} {reply[:200]!r}"
    eid, status, headers, out = exec_start(c, cid, ["sh", "-c", "echo out; exit 3"])
    assert status == 200, f"exec start without Upgrade answered {status}"
    assert "upgrade" not in headers, f"Upgrade header in a 200: {headers}"
    assert headers.get("content-type") == "application/vnd.docker.raw-stream", headers.get("content-type")
    assert b"out\n" in out, f"stream {out!r}"
    c2 = Conn()
    _, _, reply = c2.request("GET", f"/exec/{eid}/json")
    info = json.loads(reply)
    assert info["Running"] is False and info["ExitCode"] == 3, f"after the stream closed: {info}"
    # While it runs, Docker reports Running and a null ExitCode (#551).
    status, _, reply = c2.request("POST", f"/containers/{cid}/exec", {"Cmd": ["sleep", "5"]})
    assert status == 201, f"exec create {status} {reply[:200]!r}"
    running = json.loads(reply)["Id"]
    status, _, reply = c2.request("POST", f"/exec/{running}/start", {"Detach": True})
    assert status in (200, 204), f"detached exec start {status} {reply[:200]!r}"
    _, _, reply = c2.request("GET", f"/exec/{running}/json")
    info = json.loads(reply)
    assert info["Running"] is True and info["ExitCode"] is None, f"while running: {info}"
    _, status, headers, _ = exec_start(Conn(), cid, ["true"], extra="Connection: Upgrade\r\nUpgrade: tcp\r\n")
    assert status == 101 and headers.get("upgrade") == "tcp", f"with Upgrade: tcp: {status} {headers}"
    return "200 without Upgrade, ExitCode set at EOF; 101 with it"


def check_endpoints():
    # Testcontainers for .NET sends NetworkMode "" and names the network only in
    # NetworkingConfig.EndpointsConfig. Docker joins that network and not
    # bridge, so its embedded DNS answers the aliases (#407).
    c = Conn()
    for name in ("tc-api-net-web", "tc-api-net-client"):
        c.request("DELETE", f"/containers/{name}?force=1")
    c.request("DELETE", "/networks/tc-api-net")
    status, _, data = c.request("POST", "/networks/create", {"Name": "tc-api-net"})
    assert status == 201, f"network create {status} {data[:200]!r}"
    ids = {}
    for name, alias in (("tc-api-net-web", "tc-web"), ("tc-api-net-client", "tc-client")):
        endpoints = {"tc-api-net": {"Aliases": [alias]}}
        ids[name] = fresh(c, name, ["sleep", "600"], HostConfig={"NetworkMode": ""}, NetworkingConfig={"EndpointsConfig": endpoints})
        status, _, data = c.request("POST", f"/containers/{ids[name]}/start")
        assert status == 204, f"start {status} {data[:200]!r}"
    _, _, d = c.request("GET", f"/containers/{ids['tc-api-net-client']}/json")
    networks = sorted(json.loads(d)["NetworkSettings"]["Networks"])
    assert networks == ["tc-api-net"], f"networks {networks}, want only tc-api-net"
    _, status, _, out = exec_start(Conn(), ids["tc-api-net-client"], ["getent", "hosts", "tc-web"])
    assert status == 200 and re.search(rb"\d+\.\d+\.\d+\.\d+\s+tc-web", out), f"the alias did not resolve: {out!r}"
    # Two networks, the internal one sorted first: the default route must go
    # by the one that reaches outside, as Docker picks its gateway.
    c.request("DELETE", "/containers/tc-api-net-two?force=1")
    for n in ("tc-api-net-a-back", "tc-api-net-z-front"):
        c.request("DELETE", f"/networks/{n}")
    status, _, data = c.request("POST", "/networks/create", {"Name": "tc-api-net-a-back", "Internal": True})
    assert status == 201, f"internal network create {status} {data[:200]!r}"
    status, _, data = c.request("POST", "/networks/create", {"Name": "tc-api-net-z-front"})
    assert status == 201, f"network create {status} {data[:200]!r}"
    two = fresh(c, "tc-api-net-two", ["sleep", "600"], HostConfig={"NetworkMode": ""},
                NetworkingConfig={"EndpointsConfig": {"tc-api-net-a-back": {}, "tc-api-net-z-front": {}}})
    status, _, data = c.request("POST", f"/containers/{two}/start")
    assert status == 204, f"start {status} {data[:200]!r}"
    _, _, d = c.request("GET", f"/containers/{two}/json")
    networks = json.loads(d)["NetworkSettings"]["Networks"]
    assert sorted(networks) == ["tc-api-net-a-back", "tc-api-net-z-front"], f"networks {sorted(networks)}"
    gateway = networks["tc-api-net-z-front"]["Gateway"]
    _, status, _, out = exec_start(Conn(), two, ["ip", "route"])
    assert status == 200 and re.search(rb"default via " + re.escape(gateway.encode()) + rb"\b", out), f"no default route by {gateway} (the outside network): {out!r}"
    return "joins only the networks named in EndpointsConfig, its alias resolves, and an internal one doesn't take the default route"


def check_copy_after_start():
    # Testcontainers' LocalStack (Java) starts a container that waits for a
    # script, then uploads it with PUT /archive straight after /start. An
    # image VOLUME with content is copied into the new volume while the
    # container sets up its root; an upload in that window answered 200 and
    # went into the wrong root, so the file never appeared (#527, #311).
    c = Conn(timeout=300)
    image = "tc-api-bigvol:latest"
    status, _, _ = c.request("GET", f"/images/{image}/json")
    if status != 200:
        src = fresh(c, "tc-api-bigvol-src", ["sh", "-c", "mkdir -p /data && i=0; while [ $i -lt 4000 ]; do head -c 50000 /dev/urandom > /data/f$i; i=$((i+1)); done"])
        status, _, data = c.request("POST", f"/containers/{src}/start")
        assert status == 204, f"start {status} {data[:200]!r}"
        status, _, data = c.request("POST", f"/containers/{src}/wait")
        assert status == 200 and json.loads(data)["StatusCode"] == 0, f"filling /data: {status} {data[:200]!r}"
        status, _, data = c.request("POST", "/commit?container=tc-api-bigvol-src&repo=tc-api-bigvol&tag=latest", {"Volumes": {"/data": {}}})
        assert status == 201, f"commit {status} {data[:200]!r}"
        c.request("DELETE", "/containers/tc-api-bigvol-src?force=1")
    script = b"#!/bin/sh\necho Ready.\nsleep 600\n"
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tar:
        entry = tarfile.TarInfo("tc_start.sh")
        entry.size, entry.mode = len(script), 0o755
        tar.addfile(entry, io.BytesIO(script))
    lost = []
    for run in range(3):
        name = f"tc-api-copy-{run}"
        c.request("DELETE", f"/containers/{name}?force=1&v=1")
        status, _, data = c.request("POST", f"/containers/create?name={name}", {
            "Image": image, "Cmd": [],
            "Entrypoint": ["sh", "-c", "while [ ! -f /tc_start.sh ]; do sleep 0.1; done; /tc_start.sh"]})
        assert status == 201, f"create {status} {data[:200]!r}"
        cid = json.loads(data)["Id"]
        status, _, data = c.request("POST", f"/containers/{cid}/start")
        assert status == 204, f"start {status} {data[:200]!r}"
        status, _, data = c.request("PUT", f"/containers/{cid}/archive?path=/", buf.getvalue(), "application/x-tar")
        assert status == 200, f"archive {status} {data[:200]!r}"
        deadline = time.time() + 10
        while time.time() < deadline:
            _, _, logs = c.request("GET", f"/containers/{cid}/logs?stdout=1&stderr=1")
            if b"Ready." in logs:
                break
            time.sleep(0.25)
        else:
            lost.append(run + 1)
        c.request("DELETE", f"/containers/{cid}?force=1&v=1")
    assert not lost, f"PUT /archive answered 200 but the file never appeared in run(s) {lost} of 3"
    return "a file uploaded straight after /start is there, 3 of 3"


CHECKS = {
    "version": check_version, "cmd": check_cmd, "keepalive": check_keepalive, "close": check_close,
    "follow": check_follow, "follow-created": check_follow_created, "timestamps": check_timestamps,
    "exec": check_exec, "endpoints": check_endpoints, "copy-after-start": check_copy_after_start,
}

if __name__ == "__main__":
    c = Conn(timeout=300)
    # Pulled only when missing, as Testcontainers' default pull policy does,
    # and then from KIRI_TEST_REGISTRY (tests/lib/images.sh), not Docker Hub:
    # this script runs once a check, and every pull of plain alpine counted
    # against Docker Hub's hourly limit.
    status, _, _ = c.request("GET", f"/images/{IMAGE}/json")
    if status != 200:
        reg = os.environ.get("KIRI_TEST_REGISTRY") or "public.ecr.aws/docker/library"
        repo, tag = IMAGE.split(":")
        status, _, _ = c.request("POST", f"/images/create?fromImage={reg}/{repo}&tag={tag}")
        if status == 200:
            c.request("POST", f"/images/{reg}/{IMAGE}/tag?repo={repo}&tag={tag}")
            c.request("DELETE", f"/images/{reg}/{IMAGE}")
    failed = 0
    for name in sys.argv[2:] or CHECKS:
        try:
            print(f"ok {name}: {CHECKS[name]()}", flush=True)
        except Exception as e:  # noqa: BLE001 — every failure is a line
            failed += 1
            print(f"FAIL {name}: {type(e).__name__}: {e}", flush=True)
    sys.exit(1 if failed else 0)
