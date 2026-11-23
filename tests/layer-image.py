"""Writes a `docker load` archive to stdout: the layers of a base image (a
`docker save` read from stdin) and crafted layers on top, for the e2e checks
of how layers unpack. Each run's layers are new (a file in each holds the
time it was made), so the engine under test unpacks them itself: an engine
keeps layers it unpacked before, maybe with older code.

    docker save alpine | python3 tests/layer-image.py SCENARIO TAG [N] | docker load
    python3 tests/layer-image.py nolayers TAG --no-base --arch arm64 | docker load
    docker save alpine | python3 tests/layer-image.py --times PATH...

Scenarios:
  fresh      the base's own layers, unpacked anew (#676: the reporter's alpine)
  dirtimes   directories with their own times (#676): nested, in two layers,
             under a whiteout or an opaque marker, 0, after 2038, behind a symlink
  layers N   N layers in all, the base's included; layer i holds /layer-i (#253)
  hardlinks  hard links to symlinks: dangling, in another directory, chained (#409)
  special    a character and a block device, a fifo, and times of 0 (#679, #684)
  big        one layer of 4 MiB that does not compress (#661)
  nolayers   no layers at all (#685)
  retype     one layer names a path twice, as another kind the second time
  retype2    paths named again as another kind in a later layer, deep in a tree,
             then removed by a whiteout, and hard links to a retyped path

--times PATH...: prints "MTIME PATH" for each path as the base's layers give
it (the topmost layer that names it), what a container's stat must show."""
import hashlib, io, json, os, sys, tarfile, time

T0 = 1_000_000_000
NONCE = f"{time.time_ns()}-{os.getpid()}".encode()


def ent(name, kind=tarfile.REGTYPE, mtime=T0, data=None, **kw):
    return dict(name=name, type=kind, mtime=mtime, data=data, **kw)


def d(name, mtime=T0):
    return ent(name, tarfile.DIRTYPE, mtime, mode=0o755)


def f(name, mtime=T0, data=b"x", mode=0o644):
    return ent(name, mtime=mtime, data=data, mode=mode)


def s(name, target, mtime=T0):
    return ent(name, tarfile.SYMTYPE, mtime, linkname=target, mode=0o777)


def h(name, target):
    return ent(name, tarfile.LNKTYPE, T0, linkname=target, mode=0o777)


def nonce(i):
    return f(f".kiri-test-{hashlib.sha1(NONCE + bytes([i % 256])).hexdigest()[:12]}", data=NONCE + str(i).encode())


def layer(entries):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.GNU_FORMAT) as t:
        for e in entries:
            e = dict(e)
            data = e.pop("data")
            ti = tarfile.TarInfo(e.pop("name"))
            for k, v in e.items():
                setattr(ti, k, v)
            if data is not None:
                ti.size = len(data)
            t.addfile(ti, io.BytesIO(data) if data is not None else None)
    return buf.getvalue()


def renewed(raw, i):
    """The base layer `raw` with one more file, so its digest is new."""
    src = tarfile.open(fileobj=io.BytesIO(raw))
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as t:
        for m in src:
            t.addfile(m, src.extractfile(m) if m.isreg() else None)
        e = nonce(i)
        ti = tarfile.TarInfo(e["name"]); ti.size = len(e["data"]); ti.mtime = T0
        t.addfile(ti, io.BytesIO(e["data"]))
    return buf.getvalue()


def crafted(name, n):
    T1, T2, T3 = T0 + 100_000, T0 + 200_000, T0 + 300_000
    if name == "dirtimes":
        one = [d("deep")]
        p = "deep"
        for i in range(1, 25):
            p += f"/n{i}"
            one.append(d(p, T0 + i))
        one += [f(p + "/leaf", T0 + 99),
                d("two", T1), f("two/f1", T1),
                d("gone", T1), f("gone/f", T1),
                d("op", T1), f("op/old", T1),
                d("rep", T1), f("rep/f", T1),
                d("p", T1), f("p/x", T1),
                d("future", 4102444800), f("future/f", T1),
                d("zero", 0), f("zero/f", T1),
                d("target", T1), s("lnk", "target", T2),
                d("empty", T1),
                f("after/f", T1), d("after", T2)]
        two = [d("two", T3), f("two/f2", T3),
               f(".wh.gone", T3),
               d("op", T3), f("op/.wh..wh..opq", T3), f("op/new", T3),
               f(".wh.rep", T3), s("rep", "/mnt", T3),
               d("p", T3), f("p/.wh.x", T3),
               d("late", T3)] + [f(f"late/f{i}", T1) for i in range(50)]
        return [one, two]
    if name == "layers":
        return [[f(f"layer-{i}", T0 + i, data=f"{i}\n".encode())] for i in range(1, n + 1)]
    if name == "hardlinks":
        return [[d("bin"), f("bin/real", data=b"#!/bin/sh\necho ok\n", mode=0o755),
                 s("bin/sym", "/bin/real"), h("bin/hl", "bin/sym"),
                 s("bin/dangling", "/no/such/file"), h("bin/hl-dangling", "bin/dangling"),
                 d("other"), h("other/hl-sym", "bin/sym"),
                 s("bin/rel", "real"), h("other/hl-rel", "bin/rel"),
                 h("bin/chain1", "bin/hl"), h("other/chain2", "bin/chain1"),
                 s("bin/symsym", "sym"), h("bin/hl-symsym", "bin/symsym"),
                 d("dir"), s("dirlink", "dir"), h("hl-dirlink", "dirlink")]]
    if name == "special":
        return [[d("spec"),
                 ent("spec/null", tarfile.CHRTYPE, devmajor=1, devminor=3, mode=0o666),
                 ent("spec/blk", tarfile.BLKTYPE, devmajor=7, devminor=0, mode=0o660),
                 ent("spec/fifo", tarfile.FIFOTYPE, mode=0o644),
                 f("spec/zero", 0), d("zdir", 0), s("zlink", "spec/zero", 0)]]
    if name == "big":
        return [[f("big", data=os.urandom(4 << 20))]]
    if name == "retype":
        return [[d("swap1"), f("swap1/inner"), f("swap1", T1),
                 s("swap2", "/srv"), d("swap2", T1),
                 f("swap3"), d("swap3", T1),
                 d("swap4"), f("swap4/inner"), s("swap4", "swap1", T1),
                 d("swap5"), f("swap5/inner"), h("swap5", "swap1")]]
    if name == "retype2":
        one = [d("ac1"), f("ac1/inner"),
               f("ac2"),
               s("ac3", "/srv"),
               d("ac4"), f("ac4/inner"),
               d("wo1"), f("wo1/inner"), f("wo1", T1),
               f("wo2"),
               d("deep"), d("deep/a"), d("deep/a/b"), f("deep/a/b/c"),
               d("deep/a/b/d"), f("deep/a/b/d/e"), f("deep/a/b/d", T1),
               d("deep2"), d("deep2/a"), d("deep2/a/b"), f("deep2/a/b/x"),
               d("hl1"), f("hl1/inner"), f("hl1", T1, data=b"one"), h("hl1link", "hl1")]
        two = [f("ac1", T2, data=b"was-dir"),
               d("ac2", T2), f("ac2/inner", T2),
               d("ac3", T2), f("ac3/inner", T2),
               s("ac4", "ac2", T2),
               f(".wh.wo1", T2),
               f(".wh.wo2", T2), d("wo2", T2), f("wo2/new", T2),
               d("deep/a/b/c", T2), f("deep/a/b/c/x", T2),
               f("deep2/a", T2),
               h("hl2link", "ac1")]
        return [one, two]
    if name in ("fresh", "nolayers"):
        return []
    raise SystemExit(f"layer-image.py: no scenario {name}")


def plain(b):
    """A layer blob uncompressed: a containerd image store saves them as pulled."""
    if b[:2] == b"\x1f\x8b":
        import gzip
        return gzip.decompress(b)
    if b[:4] == b"\x28\xb5\x2f\xfd":
        try:
            from compression import zstd
            return zstd.decompress(b)
        except ImportError:
            import subprocess
            return subprocess.run(["zstd", "-dc"], input=b, capture_output=True, check=True).stdout
    return b


def base(raw):
    t = tarfile.open(fileobj=io.BytesIO(raw))
    m = json.load(t.extractfile("manifest.json"))[0]
    return [plain(t.extractfile(p).read()) for p in m["Layers"]], json.load(t.extractfile(m["Config"]))


def times(paths):
    layers, _ = base(sys.stdin.buffer.read())
    seen = {}
    for raw in layers:
        for m in tarfile.open(fileobj=io.BytesIO(raw)):
            seen[m.name.rstrip("/").removeprefix("./")] = int(m.mtime)
    for p in paths:
        print(seen.get(p.strip("/"), "missing"), p)


def main():
    args = sys.argv[1:]
    if args[0] == "--times":
        return times(args[1:])
    name, tag = args[0], args[1]
    n = int(args[2]) if len(args) > 2 and args[2].isdigit() else 0
    arch = args[args.index("--arch") + 1] if "--arch" in args else "amd64"
    blayers, bcfg = ([], {"architecture": arch}) if "--no-base" in args else base(sys.stdin.buffer.read())
    if name == "fresh":
        blayers = [renewed(raw, i) for i, raw in enumerate(blayers)]
    top = crafted(name, max(0, n - len(blayers)))
    layers = blayers + [layer(e + [nonce(i)]) for i, e in enumerate(top)]
    diff_ids = ["sha256:" + hashlib.sha256(raw).hexdigest() for raw in layers]
    conf = dict(bcfg.get("config") or {})
    conf["Cmd"] = ["/nothing"] if name == "nolayers" else ["/bin/sh"]
    cfg = json.dumps({"architecture": bcfg.get("architecture", arch), "os": "linux", "config": conf,
                      "rootfs": {"type": "layers", "diff_ids": diff_ids},
                      "history": [{"created_by": f"layer {i}"} for i in range(len(layers))]}).encode()
    cid = hashlib.sha256(cfg).hexdigest()
    out = tarfile.open(fileobj=sys.stdout.buffer, mode="w|")

    def add(path, b):
        i = tarfile.TarInfo(path); i.size = len(b); out.addfile(i, io.BytesIO(b))

    paths = []
    for raw, did in zip(layers, diff_ids):
        p = f"{did[7:]}/layer.tar"
        if p not in paths:
            add(p, raw)
        paths.append(p)
    add(f"{cid}.json", cfg)
    add("manifest.json", json.dumps([{"Config": f"{cid}.json", "RepoTags": [tag], "Layers": paths}]).encode())
    out.close()


main()
