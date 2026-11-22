"""Writes a one-layer `docker save` archive whose layer has `bin/tool` and a
hard link `bin/tool-1.0` to LINK, for the e2e checks of issue 302.
Usage: hardlink-image.py OUT.tar TAG LINK"""
import hashlib, io, json, platform, sys, tarfile

out, tag, link = sys.argv[1:4]
layer = io.BytesIO()
with tarfile.open(fileobj=layer, mode="w", format=tarfile.GNU_FORMAT) as t:
    d = tarfile.TarInfo("bin"); d.type = tarfile.DIRTYPE; d.mode = 0o755; t.addfile(d)
    data = b"#!/bin/sh\necho hello from tool\n"
    f = tarfile.TarInfo("bin/tool"); f.size = len(data); f.mode = 0o755; t.addfile(f, io.BytesIO(data))
    h = tarfile.TarInfo("bin/tool-1.0"); h.type = tarfile.LNKTYPE; h.linkname = link; h.mode = 0o755; t.addfile(h)
lb = layer.getvalue()
ld = hashlib.sha256(lb).hexdigest()
arch = "arm64" if platform.machine().lower() in ("arm64", "aarch64") else "amd64"
cfg = json.dumps({"architecture": arch, "os": "linux", "config": {"Cmd": ["/bin/tool-1.0"]},
                  "rootfs": {"type": "layers", "diff_ids": ["sha256:" + ld]}}).encode()
cd = hashlib.sha256(cfg).hexdigest()
manifest = json.dumps([{"Config": f"{cd}.json", "RepoTags": [tag], "Layers": [f"{ld}/layer.tar"]}]).encode()
with tarfile.open(out, mode="w") as t:
    for name, b in [(f"{ld}/layer.tar", lb), (f"{cd}.json", cfg), ("manifest.json", manifest)]:
        i = tarfile.TarInfo(name); i.size = len(b); t.addfile(i, io.BytesIO(b))
