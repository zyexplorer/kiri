# Kiri on a Linux host as an RPM: `kiri` (engine + docker CLI, Compose,
# buildx) or `kiri-lite` (engine only), chosen by packaging/linux/build.sh
# through kiri_name and with_cli. Static (musl) binaries: no runtime
# dependencies, no debuginfo. With with_window, `kiri` also carries the
# window (kiri built against GTK 3 and WebKitGTK 4.1), which needs those.
%global debug_package %{nil}
%global __strip /bin/true
%global __brp_mangle_shebangs %{nil}

Name:           %{kiri_name}
Version:        %{kiri_version}
Release:        1%{?dist}
Summary:        Tiny, fast container runtime (Docker API)%{!?with_cli:, engine only}
License:        Apache-2.0
URL:            https://runkiri.com
Source0:        kiri
Source1:        kiri-init
Source2:        kiri.service
Source3:        LICENSE
Source4:        NOTICE
Source18:       dev.kiri.host-access.policy
%if 0%{?with_cli}
Source5:        docker
Source6:        docker-compose
Source7:        docker-buildx
Source8:        docker-wrapper
Conflicts:      kiri-lite
%if 0%{?with_window}
Source9:        kiri-window
Source10:       kiri.desktop
Source11:       kiri-app-icon.svg
Source12:       kiri-app-icon-16.png
Source13:       kiri-app-icon-32.png
Source14:       kiri-app-icon-64.png
Source15:       kiri-app-icon-128.png
Source16:       kiri-app-icon-256.png
Source17:       kiri-app-icon-512.png
Source19:       dev.kiri.Kiri.metainfo.xml
Requires:       gtk3
Requires:       webkit2gtk4.1
# The window's setup checklist asks for the password through pkexec.
Recommends:     polkit
%endif
%else
Conflicts:      kiri
%endif
BuildArch:      %{kiri_arch}
# Static binaries: nothing to find by scanning them.
AutoReq:        no
Requires(pre):  shadow-utils
%if 0%{?with_cli}
Requires(post): %{_sbindir}/update-alternatives
Requires(preun): %{_sbindir}/update-alternatives
Requires(posttrans): %{_sbindir}/update-alternatives
%endif

%description
Kiri runs containers on this machine's kernel and speaks the Docker API on
/run/kiri/docker.sock for users in the kiri group (and at
/var/run/docker.sock while no Docker engine has it). No VM, no daemon other
than kiri itself. Members of the kiri group can run containers as root on
this machine; treat it like the docker group.
%if 0%{?with_cli}
This package brings the docker CLI, Compose and buildx with it, as Kiri.app
does on the Mac: `docker run` works right after installing. A docker CLI from
another package keeps precedence on PATH.
%if 0%{?with_window}
Kiri's window is in the app menu, or: kiri ui
%endif
%else
Engine only: point any docker CLI at it with
  docker context create kiri --docker host=unix:///run/kiri/docker.sock
%endif

%prep

%build

%install
install -D -m 755 %{SOURCE0} %{buildroot}%{_bindir}/kiri
install -D -m 755 %{SOURCE1} %{buildroot}%{_bindir}/kiri-init
install -D -m 644 %{SOURCE2} %{buildroot}%{_unitdir}/kiri.service
install -D -m 644 %{SOURCE3} %{buildroot}%{_licensedir}/%{name}/LICENSE
install -D -m 644 %{SOURCE4} %{buildroot}%{_licensedir}/%{name}/NOTICE
install -D -m 644 %{SOURCE18} %{buildroot}%{_datadir}/polkit-1/actions/dev.kiri.host-access.policy
%if 0%{?with_cli}
install -D -m 755 %{SOURCE5} %{buildroot}/usr/libexec/kiri/bin/docker
install -D -m 755 %{SOURCE8} %{buildroot}/usr/libexec/kiri/docker
install -D -m 755 %{SOURCE6} %{buildroot}/usr/libexec/kiri/cli-plugins/docker-compose
install -D -m 755 %{SOURCE7} %{buildroot}/usr/libexec/kiri/cli-plugins/docker-buildx
%endif
%if 0%{?with_window}
install -D -m 755 %{SOURCE9} %{buildroot}/usr/lib/kiri/kiri-window
install -D -m 644 %{SOURCE10} %{buildroot}%{_datadir}/applications/kiri.desktop
install -D -m 644 %{SOURCE19} %{buildroot}%{_datadir}/metainfo/dev.kiri.Kiri.metainfo.xml
install -D -m 644 %{SOURCE11} %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/kiri.svg
for s in 16 32 64 128 256 512; do
  install -D -m 644 %{_sourcedir}/kiri-app-icon-$s.png %{buildroot}%{_datadir}/icons/hicolor/${s}x${s}/apps/kiri.png
done
%endif

%pre
getent group kiri >/dev/null || groupadd --system kiri
exit 0

%post
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload >/dev/null 2>&1 || :
  # Not started or enabled: it starts when someone sets Kiri up (the window's
  # Allow access, or `kiri host-access`). An upgrade ($1 >= 2) leaves a
  # running engine alone: it keeps the old binary until it restarts, which
  # stops its containers.
  if [ "$1" -ge 2 ] && systemctl is-active -q kiri.service; then
    echo "Kiri is upgraded. The engine keeps running the old version until it restarts:"
    echo "  sudo systemctl restart kiri   (running containers stop; those with a restart policy start again)"
    echo "or open Kiri and choose Restart engine. The next boot starts the new one either way."
  fi
fi
# A `kiri` before /usr/bin on PATH runs instead of this package's.
for d in /usr/local/sbin /usr/local/bin; do
  if [ -e "$d/kiri" ] && [ "$(readlink -f "$d/kiri")" != /usr/bin/kiri ]; then
    echo "Warning: $d/kiri comes before /usr/bin/kiri on PATH, so it runs instead of this Kiri."
    echo "  If it's an older Kiri, remove it:  sudo rm $d/kiri"
  fi
done
%if 0%{?with_cli}
# Only a /usr/bin/docker that is absent or already an alternatives link; a
# real file, or a symlink another package or the administrator put there, is
# left alone: Kiri's CLI must not take over someone else's docker.
free=1
if [ -e /usr/bin/docker ] || [ -L /usr/bin/docker ]; then
  case "$(readlink /usr/bin/docker 2>/dev/null)" in /etc/alternatives/*) ;; *) free=0 ;; esac
fi
if [ "$free" = 1 ]; then
  %{_sbindir}/update-alternatives --install /usr/bin/docker docker /usr/libexec/kiri/docker 10 || :
  echo "Kiri is installed with its own docker CLI (docker, docker compose, docker buildx)."
else
  echo "Kiri is installed; /usr/bin/docker belongs to something else and stays. Kiri's CLI: /usr/libexec/kiri/docker"
fi
%else
echo "Kiri is installed (engine only). Point a docker CLI at it:"
echo "  docker context create kiri --docker host=unix:///run/kiri/docker.sock && docker context use kiri"
%endif
echo "To use it without sudo:  sudo kiri host-access \$USER   (or open Kiri, which sets it up)"
echo "Members of the kiri group can run containers as root on this machine; treat it like the docker group."
echo "While no Docker engine has /var/run/docker.sock, it leads to Kiri too (for Testcontainers, docker-py and other tools that ignore docker contexts)."
exit 0

%preun
if [ "$1" -eq 0 ]; then
  if [ -d /run/systemd/system ]; then
    systemctl disable --now kiri.service >/dev/null 2>&1 || :
  fi
  # Everything else it runs: a window or kiri command still open.
  for sig in TERM KILL; do
    pkill -$sig -f '^/usr/lib/kiri/kiri-window( |$)' >/dev/null 2>&1 || :
    pkill -$sig -f '^(/usr)?/bin/kiri( |$)' >/dev/null 2>&1 || :
    [ "$sig" = KILL ] || sleep 1
  done
%if 0%{?with_cli}
  %{_sbindir}/update-alternatives --remove docker /usr/libexec/kiri/docker || :
  for p in docker-compose docker-buildx; do
    %{_sbindir}/update-alternatives --remove $p /usr/libexec/kiri/cli-plugins/$p || :
  done
%endif
fi
exit 0

%if 0%{?with_cli}
%posttrans
# Compose and buildx into /usr/lib/docker/cli-plugins where the slot is free
# and Docker's own copy isn't in another of the CLI's plugin folders; else
# Kiri's link goes and Docker's copy is used. In %posttrans because an
# upgrade from a Kiri that put real files there erases them only after %post;
# again whenever another package changes Docker's plugin folders.
mkdir -p /usr/lib/docker/cli-plugins
for p in docker-compose docker-buildx; do
  l=/usr/lib/docker/cli-plugins/$p
  theirs=
  for d in /usr/local/lib/docker/cli-plugins /usr/local/libexec/docker/cli-plugins /usr/libexec/docker/cli-plugins; do
    [ -e $d/$p ] && theirs=1
  done
  if [ -z "$theirs" ] && { [ ! -e $l ] && [ ! -L $l ] || readlink $l 2>/dev/null | grep -q '^/etc/alternatives/'; }; then
    %{_sbindir}/update-alternatives --install $l $p /usr/libexec/kiri/cli-plugins/$p 10 || :
  elif %{_sbindir}/update-alternatives --display $p 2>/dev/null | grep -q "^/usr/libexec/kiri/cli-plugins/$p "; then
    %{_sbindir}/update-alternatives --remove $p /usr/libexec/kiri/cli-plugins/$p || :
  fi
done
exit 0

%filetriggerin -- /usr/lib/docker/cli-plugins /usr/libexec/docker/cli-plugins /usr/local/lib/docker/cli-plugins /usr/local/libexec/docker/cli-plugins
mkdir -p /usr/lib/docker/cli-plugins
for p in docker-compose docker-buildx; do
  l=/usr/lib/docker/cli-plugins/$p
  theirs=
  for d in /usr/local/lib/docker/cli-plugins /usr/local/libexec/docker/cli-plugins /usr/libexec/docker/cli-plugins; do
    [ -e $d/$p ] && theirs=1
  done
  if [ -z "$theirs" ] && { [ ! -e $l ] && [ ! -L $l ] || readlink $l 2>/dev/null | grep -q '^/etc/alternatives/'; }; then
    %{_sbindir}/update-alternatives --install $l $p /usr/libexec/kiri/cli-plugins/$p 10 || :
  elif %{_sbindir}/update-alternatives --display $p 2>/dev/null | grep -q "^/usr/libexec/kiri/cli-plugins/$p "; then
    %{_sbindir}/update-alternatives --remove $p /usr/libexec/kiri/cli-plugins/$p || :
  fi
done
exit 0

%filetriggerpostun -- /usr/lib/docker/cli-plugins /usr/libexec/docker/cli-plugins /usr/local/lib/docker/cli-plugins /usr/local/libexec/docker/cli-plugins
mkdir -p /usr/lib/docker/cli-plugins
for p in docker-compose docker-buildx; do
  l=/usr/lib/docker/cli-plugins/$p
  theirs=
  for d in /usr/local/lib/docker/cli-plugins /usr/local/libexec/docker/cli-plugins /usr/libexec/docker/cli-plugins; do
    [ -e $d/$p ] && theirs=1
  done
  if [ -z "$theirs" ] && { [ ! -e $l ] && [ ! -L $l ] || readlink $l 2>/dev/null | grep -q '^/etc/alternatives/'; }; then
    %{_sbindir}/update-alternatives --install $l $p /usr/libexec/kiri/cli-plugins/$p 10 || :
  elif %{_sbindir}/update-alternatives --display $p 2>/dev/null | grep -q "^/usr/libexec/kiri/cli-plugins/$p "; then
    %{_sbindir}/update-alternatives --remove $p /usr/libexec/kiri/cli-plugins/$p || :
  fi
done
exit 0
%endif

%postun
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload >/dev/null 2>&1 || :
fi
if [ "$1" -eq 0 ]; then
  echo "Kiri is removed; its images, containers and volumes stay in /var/lib/kiri"
fi
exit 0

%files
%license %{_licensedir}/%{name}/LICENSE
%license %{_licensedir}/%{name}/NOTICE
%{_bindir}/kiri
%{_bindir}/kiri-init
%{_unitdir}/kiri.service
%{_datadir}/polkit-1/actions/dev.kiri.host-access.policy
%if 0%{?with_cli}
%dir /usr/libexec/kiri
%dir /usr/libexec/kiri/bin
/usr/libexec/kiri/bin/docker
/usr/libexec/kiri/docker
%dir /usr/libexec/kiri/cli-plugins
/usr/libexec/kiri/cli-plugins/docker-compose
/usr/libexec/kiri/cli-plugins/docker-buildx
%endif
%if 0%{?with_window}
%dir /usr/lib/kiri
/usr/lib/kiri/kiri-window
%{_datadir}/applications/kiri.desktop
%{_datadir}/icons/hicolor/*/apps/kiri.*
%{_datadir}/metainfo/dev.kiri.Kiri.metainfo.xml
%endif

%changelog
* Sun Oct 25 2026 Kiri <kiri@canestack.com> - %{kiri_version}-1
- Packaged from the static Linux build
