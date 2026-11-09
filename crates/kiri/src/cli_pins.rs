//! The docker CLI, Compose and buildx versions Kiri ships (bundled in
//! Kiri.app, downloaded by Kiri Lite), each with the SHA-256 it must match.
//! Shared with xtask, so both always agree.

#![allow(dead_code)]

pub const DOCKER_CLI: (&str, &str) = (
    "29.9.2",
    "ea8a2ce5ac31f9f5f3fb711a7a485e1e46cc7a69798a629a70306f6e6af25b7a",
);
pub const COMPOSE: (&str, &str) = (
    "v5.6.1",
    "fcce41226abd2b7b2936094e688bbd61284673dddafea542e08cb9a3b4423a8b",
);
pub const BUILDX: (&str, &str) = (
    "v0.38.1",
    "6f20ba2437865311ef99015bf5d1039c3c8e4d2177c1c64aa41b134b5ecd3012",
);

/// Grype, Anchore's image scanner (Builds → Security): the Apple Silicon
/// tarball from GitHub, `grype` inside. Downloaded on the first scan.
pub const GRYPE: (&str, &str) = (
    "0.120.1",
    "cf97957fa467d25575ec2cc3228289f391ea51cf88b9cffc5f83dc03d4cbc732",
);

pub fn grype_url(v: &str) -> String {
    format!("https://github.com/anchore/grype/releases/download/v{v}/grype_{v}_darwin_arm64.tar.gz")
}

/// The same Grype for Linux, x86-64 and arm64: the engine scans in WSL for
/// Windows' window, where Kiri's store is.
pub const LINUX_GRYPE: (&str, &str) = (
    "0.120.1",
    "0a9ee97ef5ae2ee953b0a80098105052e846cdbe319a57d808b519c33cd1343d",
);
pub const LINUX_ARM64_GRYPE: (&str, &str) = (
    "0.120.1",
    "29f47391dc283aa79fcc38e65224cd61f64dec0ecfd0db7074128ebf8ff23514",
);

pub fn linux_grype_url(v: &str, arch: &str) -> String {
    format!("https://github.com/anchore/grype/releases/download/v{v}/grype_{v}_linux_{arch}.tar.gz")
}

/// The static docker CLI for Apple Silicon; the binary is `docker/docker` inside.
pub fn docker_url(v: &str) -> String {
    format!("https://download.docker.com/mac/static/stable/aarch64/docker-{v}.tgz")
}

pub fn compose_url(v: &str) -> String {
    format!("https://github.com/docker/compose/releases/download/{v}/docker-compose-darwin-aarch64")
}

pub fn buildx_name(v: &str) -> String {
    format!("buildx-{v}.darwin-arm64")
}

pub fn buildx_url(v: &str) -> String {
    format!(
        "https://github.com/docker/buildx/releases/download/{v}/{}",
        buildx_name(v)
    )
}

/// The same versions for Intel Macs, bundled in the Intel Kiri.app
/// (`cargo xtask app --arch x86_64`). Compose and buildx are GitHub's asset
/// digests; the docker tarball as download.docker.com served it.
pub const X86_DOCKER_CLI: (&str, &str) = (
    "29.9.2",
    "089c3f618df88bdef4f1257d9d604499136a1ec5168b67d194c260acad2b522f",
);
pub const X86_COMPOSE: (&str, &str) = (
    "v5.6.1",
    "245c191b11a6345dd3b32b8f6eadeed1e1dcd5147aa46dfa423462e95f587e93",
);
pub const X86_BUILDX: (&str, &str) = (
    "v0.38.1",
    "76e7a809cc98d34a4c1b7db23feac673def5b8dcdddd9e713ef26598689aa796",
);
pub const X86_GRYPE: (&str, &str) = (
    "0.120.1",
    "5313004ccbc524c8757521dc3913f1edf4309f56bef06a2fb2d0c0eeade7cc62",
);

/// Which Mac a download is for: Apple Silicon or Intel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MacArch {
    Arm64,
    X86_64,
}

impl MacArch {
    /// The Mac this binary is built for.
    pub const HOST: MacArch = if cfg!(target_arch = "x86_64") {
        MacArch::X86_64
    } else {
        MacArch::Arm64
    };

    /// docker CLI, Compose, buildx: (version, SHA-256).
    pub fn tools(self) -> [(&'static str, &'static str); 3] {
        match self {
            MacArch::Arm64 => [DOCKER_CLI, COMPOSE, BUILDX],
            MacArch::X86_64 => [X86_DOCKER_CLI, X86_COMPOSE, X86_BUILDX],
        }
    }

    pub const fn grype(self) -> (&'static str, &'static str) {
        match self {
            MacArch::Arm64 => GRYPE,
            MacArch::X86_64 => X86_GRYPE,
        }
    }

    /// Compose's name for the architecture (`aarch64`, `x86_64`), which is
    /// also download.docker.com's.
    pub fn compose_arch(self) -> &'static str {
        match self {
            MacArch::Arm64 => "aarch64",
            MacArch::X86_64 => "x86_64",
        }
    }

    /// Go's name (`arm64`, `amd64`): buildx's and Grype's.
    pub fn go_arch(self) -> &'static str {
        match self {
            MacArch::Arm64 => "arm64",
            MacArch::X86_64 => "amd64",
        }
    }

    /// The static docker CLI; the binary is `docker/docker` inside.
    pub fn docker_url(self, v: &str) -> String {
        format!(
            "https://download.docker.com/mac/static/stable/{}/docker-{v}.tgz",
            self.compose_arch()
        )
    }

    pub fn compose_url(self, v: &str) -> String {
        format!(
            "https://github.com/docker/compose/releases/download/{v}/docker-compose-darwin-{}",
            self.compose_arch()
        )
    }

    pub fn buildx_name(self, v: &str) -> String {
        format!("buildx-{v}.darwin-{}", self.go_arch())
    }

    pub fn buildx_url(self, v: &str) -> String {
        format!(
            "https://github.com/docker/buildx/releases/download/{v}/{}",
            self.buildx_name(v)
        )
    }

    pub fn grype_url(self, v: &str) -> String {
        format!(
            "https://github.com/anchore/grype/releases/download/v{v}/grype_{v}_darwin_{}.tar.gz",
            self.go_arch()
        )
    }
}

/// The same versions for Linux x86-64, bundled in the `kiri` .deb and .rpm
/// (packaging/linux; `kiri-lite` ships the engine alone). The docker CLI is
/// `docker/docker` inside download.docker.com's static tarball; Compose and
/// buildx are GitHub's asset digests.
pub const LINUX_DOCKER_CLI: (&str, &str) = (
    "29.9.2",
    "9ac225999fd5e863f7694c728ef23d480616b1e5639328fa309befd4243d69eb",
);
pub const LINUX_COMPOSE: (&str, &str) = (
    "v5.6.1",
    "47737f4236e6d3ccea7291995203f8f018058783b4c3a9469119811a0b5db8c0",
);
pub const LINUX_BUILDX: (&str, &str) = (
    "v0.38.1",
    "e25d5cd675092bb5d1d03632fdc46cdf3fede7b514dc5f023a105e00d3ce5ad4",
);

pub fn linux_docker_url(v: &str) -> String {
    format!("https://download.docker.com/linux/static/stable/x86_64/docker-{v}.tgz")
}

pub fn linux_compose_url(v: &str) -> String {
    format!("https://github.com/docker/compose/releases/download/{v}/docker-compose-linux-x86_64")
}

pub fn linux_buildx_url(v: &str) -> String {
    format!("https://github.com/docker/buildx/releases/download/{v}/buildx-{v}.linux-amd64")
}

/// The same versions for Linux arm64, in the arm64 `kiri` .deb and .rpm,
/// from the same places (packaging/linux/fetch-cli.sh).
pub const LINUX_ARM64_DOCKER_CLI: (&str, &str) = (
    "29.9.2",
    "873b3e0207857c3b2f28392fb922f3ea6efe37620c16d4d4aaf2fef409c90b73",
);
pub const LINUX_ARM64_COMPOSE: (&str, &str) = (
    "v5.6.1",
    "42cfa75f51ad0ccf290bad37899e0038a8c10a0069e85233ecc1d53e3ff1347d",
);
pub const LINUX_ARM64_BUILDX: (&str, &str) = (
    "v0.38.1",
    "3b83b3731916ba433feccaaa689fe5bcdc81c641d92c8e6c2ea865dfa141c97a",
);

/// The same versions for Windows x86-64, bundled with kiri-win
/// (`cargo xtask windows`). Compose and buildx are GitHub's asset digests;
/// the docker CLI's zip as download.docker.com served it.
pub const WIN_DOCKER_CLI: (&str, &str) = (
    "29.9.2",
    "b2be959bfed1281ddaf665c0972cba9a62ce30c542b6bcce82f055bf8886bc7d",
);
pub const WIN_COMPOSE: (&str, &str) = (
    "v5.6.1",
    "91b1492f0a923427651e2b955739d732ac36b8191193bf4799e62f3e3525c4cd",
);
pub const WIN_BUILDX: (&str, &str) = (
    "v0.38.1",
    "fd2e18501910dd535ea3aa7f69010b3cfdfba0f47aa27674124554386dfa26f3",
);

/// The static docker CLI for Windows; the binary is `docker/docker.exe` inside.
pub fn win_docker_url(v: &str) -> String {
    format!("https://download.docker.com/win/static/stable/x86_64/docker-{v}.zip")
}

pub fn win_compose_url(v: &str) -> String {
    format!(
        "https://github.com/docker/compose/releases/download/{v}/docker-compose-windows-x86_64.exe"
    )
}

pub fn win_buildx_url(v: &str) -> String {
    format!("https://github.com/docker/buildx/releases/download/{v}/buildx-{v}.windows-amd64.exe")
}

/// The same versions for Windows on Arm (`cargo xtask windows --arch arm64`).
/// Compose and buildx publish arm64 Windows builds (GitHub's asset digests).
/// Docker publishes no arm64 docker.exe (download.docker.com/win/static has
/// x86_64 only), so it is built from docker/cli's own source at the release
/// tag's commit, with Go's toolchain pinned and cgo off: Go builds that way
/// are reproducible on any host, so the result is pinned like a download.
pub const WIN_ARM64_COMPOSE: (&str, &str) = (
    "v5.6.1",
    "2a8ab89b27a7ba79c3ec5b3eb59e099b39990d2c7ea47fc38433fc24812ad266",
);
pub const WIN_ARM64_BUILDX: (&str, &str) = (
    "v0.38.1",
    "2d4e2e78d7ddf0ea6d63acde7e73599717e94e3f9417edd23d4ffeceef6777fc",
);

/// docker/cli at tag v29.9.2 (WIN_DOCKER_CLI's version): the commit the
/// tag points at, its committer time (the binary's BuildTime, so the
/// build does not depend on the clock), and the Go docker/cli's Dockerfile
/// names for that tag. The SHA-256 is of docker.exe as `docker_cli_arm64`
/// in xtask builds it.
pub const WIN_ARM64_DOCKER_SRC: DockerSource = DockerSource {
    version: "29.9.2",
    commit: "a82e528901260c8df6073311379eac65f92db204",
    time: "2026-10-20T11:24:07Z",
    go: "go1.26.10",
    sha256: "9b33cfdde61ae604705a045e77a45f126b11240342dee4d7706e82dc462329ec",
};

pub struct DockerSource {
    pub version: &'static str,
    pub commit: &'static str,
    pub time: &'static str,
    pub go: &'static str,
    pub sha256: &'static str,
}

pub const DOCKER_CLI_GIT: &str = "https://github.com/docker/cli";

pub fn win_arm64_compose_url(v: &str) -> String {
    format!(
        "https://github.com/docker/compose/releases/download/{v}/docker-compose-windows-aarch64.exe"
    )
}

pub fn win_arm64_buildx_url(v: &str) -> String {
    format!("https://github.com/docker/buildx/releases/download/{v}/buildx-{v}.windows-arm64.exe")
}
