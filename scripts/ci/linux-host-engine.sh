#!/bin/sh
# Starts Kiri on this Linux machine the way a Linux host runs it
# (contrib/linux/kiri.service): the static engine in /usr/local/bin, the
# systemd unit and the socket at /run/kiri/docker.sock, and root's docker
# context `kiri`, which tests/e2e.sh and tests/stress.sh use (the Tests
# workflow runs them as root: see there why). For a fresh GitHub runner;
# needs sudo.
#
#   scripts/ci/linux-host-engine.sh <dir with kiri and kiri-init>
set -eu
bin=$1
root=$(cd "$(dirname "$0")/../.." && pwd)

# The kernel features tests/e2e.sh checks run on this machine's kernel. Most
# modules load by themselves when a container asks for them; IPVS and VRF
# don't, and VRF is in Ubuntu's linux-modules-extra, which a runner may not
# have. Missing either fails the kernel-ipvs and kernel-link-vrf checks under
# KIRI_TEST_STRICT=1, so it is loaded here, or this step fails saying why.
mods="ip_vs ip_vs_rr vrf"
for m in $mods; do
  modinfo "$m" >/dev/null 2>&1 && continue
  echo "module $m is not installed: installing linux-modules-extra-$(uname -r)"
  sudo apt-get update -qq
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "linux-modules-extra-$(uname -r)" >/dev/null
  break
done
# shellcheck disable=SC2086 # one word per module
sudo modprobe -a $mods || { echo "could not load $mods on kernel $(uname -r)"; exit 1; }

sudo install -m 755 "$bin/kiri" "$bin/kiri-init" /usr/local/bin/
sudo install -m 644 "$root/contrib/linux/kiri.service" /etc/systemd/system/kiri.service
sudo groupadd -f kiri
sudo systemctl daemon-reload
sudo systemctl start kiri
i=0
until [ -S /run/kiri/docker.sock ]; do
  i=$((i + 1))
  [ $i -gt 120 ] && { sudo journalctl -u kiri --no-pager | tail -n 50; echo "the engine made no socket"; exit 1; }
  sleep 0.5
done
sudo docker context create kiri --docker host=unix:///run/kiri/docker.sock >/dev/null
sudo docker --context kiri version --format 'engine {{.Server.Version}}, kernel {{.Server.KernelVersion}}'
