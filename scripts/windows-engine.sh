#!/bin/bash
# The Linux half of the Windows bundle (cargo xtask windows runs this in
# WSL): static kiri and kiri-init (musl), so the same binaries run in Kiri
# Linux (Alpine) and in Ubuntu. x86_64 for x86-64 PCs, aarch64 for Windows
# on Arm, from either kind of WSL. ring's C code is compiled by zig cc,
# from a Python venv in ~/.cache, so the distro needs nothing installed.
#   scripts/windows-engine.sh <repo> <out-dir> [x86_64|aarch64]
set -euo pipefail
repo=$1 out=$2 arch=${3:-x86_64}
case "$arch" in x86_64|aarch64) ;; *) echo "arch is x86_64 or aarch64, not $arch" >&2; exit 2;; esac
target=$arch-unknown-linux-musl
# Windows can leak HOME=C:\Users\... into WSL; rustup then looks in the wrong place.
case "${HOME:-}" in /*) ;; *) export HOME=/root ;; esac
# rustup's usual install; the rust image has cargo on PATH already.
if [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
export PATH="${HOME}/.cargo/bin:${PATH:-/usr/bin:/bin}"
venv=$HOME/.cache/kiri-zig
# A venv whose install failed has python but no zig: made again.
if ! "$venv/bin/python" -c 'import ziglang' 2>/dev/null; then
  rm -rf "$venv"
  python3 -m venv "$venv"
  # The exact wheel, by hash (PyPI's, for x86-64 and arm64 Linux): this
  # compiler builds ring's C for the shipped engine.
  req=$(mktemp)
  echo 'ziglang==0.17.0 --hash=sha256:97079827064a7492cce5541ceb438ae8003ce1e54e399676da2d62fb7a5dc08b --hash=sha256:d2804f94f54f7fcf8169f4b212a619ad688a4e2e6592e976f0cf2e58e2cd8af3' > "$req"
  "$venv/bin/pip" -q install --only-binary :all: --require-hashes -r "$req"
  rm -f "$req"
fi
cc=$venv/zig-musl-cc-$arch
# Written aside and moved in: another build on this machine may be running it.
cat > "$cc.$$" <<CC
#!/bin/bash
args=()
for a in "\$@"; do case "\$a" in --target=*) ;; *) args+=("\$a");; esac; done
exec "$venv/bin/python" -m ziglang cc -target $arch-linux-musl "\${args[@]}"
CC
chmod +x "$cc.$$"
mv "$cc.$$" "$cc"
rustup target add "$target" >/dev/null
cd "$repo"
# The repo's .cargo/config.toml links aarch64 musl with rust-lld (as the
# Mac's guest build does), so it cross-links from x86-64 WSL. x86_64 built
# in arm64 WSL links with rust-lld too: the distro's cc is for arm64.
link=()
if [ "$arch" = x86_64 ] && [ "$(uname -m)" != x86_64 ]; then
  link=(CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld)
fi
# Built in the distro's own disk (the checkout may be on /mnt/c, which is
# slow), one target dir per checkout: CI runners on one machine each have
# their own, and build at the same time.
tdir=$HOME/.cache/kiri-musl-target/$(printf %s "$PWD" | sha256sum | cut -c1-12)
# Builds from before had one shared dir: the first checkout to come along
# takes it, rather than building from nothing.
old=$HOME/.cache/kiri-musl-target
if [ ! -d "$tdir" ] && [ -d "$old/release" -o -d "$old/x86_64-unknown-linux-musl" -o -d "$old/aarch64-unknown-linux-musl" ]; then
  take=$old/.take.$$
  mkdir -p "$take"
  for d in "$old"/*; do case "$(basename "$d")" in ????????????) ;; *) mv "$d" "$take/" ;; esac; done
  mv "$take" "$tdir"
fi
env "CC_${target//-/_}=$cc" "${link[@]}" CARGO_TARGET_DIR="$tdir" \
  cargo build --release -p kiri -p kiri-init --target "$target"
mkdir -p "$out"
install -m 755 "$tdir/$target/release/kiri" "$tdir/$target/release/kiri-init" "$out/"
