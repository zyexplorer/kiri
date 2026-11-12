#!/bin/sh
# Kiri on Windows: run inside a WSL2 distro, as root, from the directory that
# holds the Linux build (cargo xtask windows; x86-64, or arm64 with --arch arm64):
#   wsl -d Ubuntu -u root sh ./install.sh
# Containers run on WSL2's Linux kernel. The daemon serves its unix socket
# only; kiri-win.exe carries the Windows docker CLI's connections to it over
# `kiri bridge` (see ../windows/kiri.ps1). It is not served on TCP: WSL's
# localhost relay drops half-closes, which `docker run` needs.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
install -m 755 "$here/kiri" "$here/kiri-init" /usr/local/bin/
# Hub mirrors for regions where docker.io is slow (mainland China). Skip if
# the person already has a daemon.json from Settings → Network.
mkdir -p /var/lib/kiri-daemon/.kiri
if [ ! -f /var/lib/kiri-daemon/.kiri/daemon.json ]; then
  printf '%s\n' '{"registry-mirrors":["https://docker.m.daocloud.io","https://docker.1ms.run","https://docker.xuanyuan.me"],"max-concurrent-downloads":4,"max-concurrent-uploads":5}' \
    >/var/lib/kiri-daemon/.kiri/daemon.json
fi
if [ -d /run/systemd/system ]; then
  cat > /etc/systemd/system/kiri.service <<UNIT
[Unit]
Description=Kiri container engine
After=network-online.target
[Service]
ExecStart=/usr/local/bin/kiri daemon
Environment=HOME=/var/lib/kiri-daemon
Delegate=yes
KillMode=mixed
Restart=on-failure
UMask=0027
[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  systemctl enable kiri
  systemctl restart kiri
else
  # No systemd in this distro: start Kiri when WSL boots it.
  if ! grep -q 'kiri daemon' /etc/wsl.conf 2>/dev/null; then
    printf '\n[boot]\ncommand = HOME=/var/lib/kiri-daemon nohup /usr/local/bin/kiri daemon >/var/log/kiri.log 2>&1 &\n' >> /etc/wsl.conf
  fi
  pkill -f '/usr/local/bin/kiri daemon' 2>/dev/null || true
  HOME=/var/lib/kiri-daemon nohup /usr/local/bin/kiri daemon >/var/log/kiri.log 2>&1 &
fi
echo "Kiri runs in WSL on /var/lib/kiri-daemon/.kiri/run/docker.sock"
