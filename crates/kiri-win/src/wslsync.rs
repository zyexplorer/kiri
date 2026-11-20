//! Putting the engine this app installed into its WSL distro: the binaries,
//! and in Kiri Linux its start script and wsl.conf (whose boot command
//! starts the engine whenever WSL boots the distro). The script is built
//! here and run by crates/kiri-win/src/update.rs; tested on any host.

use std::path::Path;
use std::sync::{Mutex, TryLockError};

/// Kiri Linux's own files, as this version has them.
const KIRI_START: &str = include_str!("../../../contrib/wsl/kiri-linux/kiri-start");
const WSL_CONF: &str = include_str!("../../../contrib/wsl/kiri-linux/wsl.conf");

/// The script, run in WSL as root, that makes the engine in /usr/local/bin
/// (where both distros keep it: kiri-start, install.sh) the one in the
/// Windows folder `from`, and in Kiri Linux (`kiri_linux`) its kiri-start
/// this version's and /etc/wsl.conf's `[boot] command` this version's
/// (the rest of that file is left as it is). Only what differs is copied,
/// each file replaced whole through a temp file of its own (two copies at
/// once never write into one); it prints "copied" when anything was. Any
/// step that fails ends it, exit 1 and why on stderr. `root` is "" but in
/// tests.
pub fn sync_script(from: &Path, root: &str, kiri_linux: bool) -> String {
    let win = |f: &str| sh_quote(&from.join(f).to_string_lossy().replace('\\', "/"));
    let (bin, conf) = (sh_quote(&format!("{root}/usr/local/bin")), sh_quote(&format!("{root}/etc/wsl.conf")));
    let mut s = format!(
        "d=/tmp/kiri-sync.$$; trap 'rm -f $d.*' EXIT; c=0; \
         fail() {{ echo \"$1\" >&2; exit 1; }}; \
         put() {{ cmp -s \"$1\" \"$2\" && return 0; cp -f \"$1\" \"$2.new.$$\" && chmod \"$3\" \"$2.new.$$\" && mv -f \"$2.new.$$\" \"$2\" && c=1 || {{ rm -f \"$2.new.$$\"; fail \"could not put $2 into WSL\"; }}; }}; \
         put \"$(wslpath -a {})\" {bin}/kiri 755; put \"$(wslpath -a {})\" {bin}/kiri-init 755; ",
        win("kiri"),
        win("kiri-init")
    );
    if kiri_linux {
        // As base64, decoded there: the newlines, quotes and `$` of a
        // script passed as they are would rely on how wsl.exe carries an
        // argument's control characters into Linux, which is not documented.
        s += &format!(
            "printf %s {} | base64 -d >$d.start || fail 'could not decode kiri-start in WSL'; put $d.start {bin}/kiri-start 755; \
             if [ -f {conf} ]; then awk -v want={} {} {conf}; else awk -v want={} {} /dev/null; fi >$d.conf || fail 'could not read /etc/wsl.conf in WSL'; put $d.conf {conf} 644; ",
            base64(KIRI_START.as_bytes()),
            sh_quote(boot_command()),
            sh_quote(BOOT_AWK),
            sh_quote(boot_command()),
            sh_quote(BOOT_AWK),
        );
    }
    s + "[ $c = 1 ] && echo copied; true"
}

/// The `command = …` line of this version's wsl.conf.
fn boot_command() -> &'static str {
    WSL_CONF.lines().find(|l| l.starts_with("command =")).unwrap_or_default()
}

/// wsl.conf with `want` as its `[boot] command`: an existing one replaced
/// where it is, else added at the end of `[boot]`, or a `[boot]` added at
/// the end; everything else as it was. A line is matched without its
/// spaces, a `#` comment, or the `\r` of a file saved with CRLF
/// ([[:space:]] has it). On one line, with no backslash, as the rest of
/// the script.
const BOOT_AWK: &str = "function key(l) { sub(/#.*/, \"\", l); gsub(/[[:space:]]/, \"\", l); return l } \
     function flush() { if (boot && !done) { print want; done = 1 } } \
     substr(key($0), 1, 1) == \"[\" { flush(); boot = key($0) == \"[boot]\"; print; next } \
     boot && index(key($0), \"command=\") == 1 { if (!done) { print want; done = 1 } next } \
     { print } \
     END { flush(); if (!done) { if (NR) print \"\"; print \"[boot]\"; print want } }";

/// Moving an engine older than this app over to this app's, at most once
/// per run of kiri-win. After an update, Kiri Linux's boot command (or the
/// bridge) starts the engine in WSL before kiri-win can copy the new one in,
/// so finding it answering says nothing about its version: that is asked
/// once it answers. A restart that fails is not tried again in this run
/// (the window offers it again). One asked while another runs returns at
/// once, so two never overlap and none pile up behind a slow one.
pub struct CatchUp(Mutex<bool>);

impl CatchUp {
    pub const fn new() -> CatchUp {
        CatchUp(Mutex::new(false))
    }

    /// Asks `version` for the engine's version (None: it did not say) and,
    /// the first time it says one that is `behind` (older than this app's:
    /// not a newer one, nor a development build's), runs `restart`. Once
    /// any version has been read, nothing is asked again. True if it ran
    /// `restart`.
    pub fn run(&self, version: impl FnOnce() -> Option<String>, behind: impl FnOnce(&str) -> bool, restart: impl FnOnce(&str)) -> bool {
        let mut done = match self.0.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        if *done {
            return false;
        }
        let Some(v) = version() else { return false };
        *done = true;
        if !behind(&v) {
            return false;
        }
        restart(&v);
        true
    }
}

/// `s` as one sh word.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = c.iter().enumerate().fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= c.len() { char::from(ABC[(n >> (18 - 6 * i) & 63) as usize]) } else { '=' });
        }
    }
    out
}

/// wsl.exe's arguments to run `script` with sh as root in `distro`. With
/// `-e` (`--exec`), not `--`: after `--` wsl.exe hands the rest of its
/// command line to the distro's login shell, which expands the script's
/// `$1`, `$(...)` and quotes before `sh -c` sees it; `-e` runs sh itself,
/// with the script as one argument, as given.
pub fn wsl_sh<'a>(distro: &'a str, script: &'a str) -> [&'a str; 8] {
    ["-d", distro, "-u", "root", "-e", "sh", "-c", script]
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    /// The script, in sh, against a stand-in root and a `wslpath` that
    /// gives the path as is: it copies what differs, puts this version's
    /// kiri-start and wsl.conf in, and the second run copies nothing.
    #[test]
    fn sync_puts_the_engine_and_kiri_linux_files_in() {
        let dir = std::env::temp_dir().join(format!("kiri-wslsync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (from, root, tools) = (dir.join("win"), dir.join("root"), dir.join("tools"));
        for d in [&from, &root.join("usr/local/bin"), &root.join("etc"), &tools] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(from.join("kiri"), "new kiri").unwrap();
        std::fs::write(from.join("kiri-init"), "init").unwrap();
        std::fs::write(root.join("usr/local/bin/kiri"), "old kiri").unwrap();
        std::fs::write(root.join("usr/local/bin/kiri-init"), "init").unwrap();
        std::fs::write(root.join("usr/local/bin/kiri-start"), "old start").unwrap();
        // An older Kiri Linux's, and a user's own line.
        std::fs::write(root.join("etc/wsl.conf"), "[boot]\nsystemd = false\n\n[user]\ndefault = root\n[automount]\noptions = metadata\n").unwrap();
        std::fs::write(tools.join("wslpath"), "#!/bin/sh\n[ \"$1\" = -a ] && shift\necho \"$1\"\n").unwrap();
        std::fs::set_permissions(tools.join("wslpath"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", tools.display(), std::env::var("PATH").unwrap_or_default());
        let script = sync_script(&from, &root.to_string_lossy(), true);
        let run = || Command::new("sh").args(["-c", &script]).env("PATH", &path).output().unwrap();
        let out = run();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "copied");
        let read = |p: &str| std::fs::read_to_string(root.join(p)).unwrap();
        assert_eq!(read("usr/local/bin/kiri"), "new kiri");
        assert_eq!(read("usr/local/bin/kiri-start"), KIRI_START);
        let want = boot_command();
        assert!(want.starts_with("command = /usr/local/bin/kiri-start"), "{want}");
        assert_eq!(read("etc/wsl.conf"), format!("[boot]\nsystemd = false\n\n{want}\n[user]\ndefault = root\n[automount]\noptions = metadata\n"));
        let mode = |p: &str| std::fs::metadata(root.join(p)).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode("usr/local/bin/kiri"), mode("usr/local/bin/kiri-start"), mode("etc/wsl.conf")), (0o755, 0o755, 0o644));
        // No temp file left behind.
        let left: Vec<_> = std::fs::read_dir(root.join("usr/local/bin")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left.len(), 3, "{left:?}");
        // Everything is this version's now: nothing copied.
        let out = run();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "");
        // Ubuntu's: the binaries only.
        std::fs::write(from.join("kiri"), "newer kiri").unwrap();
        std::fs::write(root.join("usr/local/bin/kiri-start"), "keep").unwrap();
        let out = Command::new("sh").args(["-c", &sync_script(&from, &root.to_string_lossy(), false)]).env("PATH", &path).output().unwrap();
        assert!(out.status.success());
        assert_eq!((read("usr/local/bin/kiri").as_str(), read("usr/local/bin/kiri-start").as_str()), ("newer kiri", "keep"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stand-in root and tools for the script: (Windows folder, root, PATH).
    fn stand_in(name: &str) -> (std::path::PathBuf, std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("kiri-wslsync-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (from, root, tools) = (dir.join("win"), dir.join("root"), dir.join("tools"));
        for d in [&from, &root.join("usr/local/bin"), &root.join("etc"), &tools] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(from.join("kiri"), "kiri").unwrap();
        std::fs::write(from.join("kiri-init"), "init").unwrap();
        std::fs::write(tools.join("wslpath"), "#!/bin/sh\n[ \"$1\" = -a ] && shift\necho \"$1\"\n").unwrap();
        std::fs::set_permissions(tools.join("wslpath"), std::fs::Permissions::from_mode(0o755)).unwrap();
        (from, root, format!("{}:{}", tools.display(), std::env::var("PATH").unwrap_or_default()))
    }

    /// wsl.conf keeps what a user put in it: only its `[boot] command` is
    /// made this version's, replaced where it is or added; this version's
    /// own wsl.conf is left exactly as it is.
    #[test]
    fn wsl_conf_keeps_the_rest() {
        let (from, root, path) = stand_in("conf");
        let want = boot_command();
        let run = |conf: Option<&str>| {
            let f = root.join("etc/wsl.conf");
            match conf {
                Some(c) => std::fs::write(&f, c).unwrap(),
                None => {
                    let _ = std::fs::remove_file(&f);
                }
            }
            let out = Command::new("sh").args(["-c", &sync_script(&from, &root.to_string_lossy(), true)]).env("PATH", &path).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            std::fs::read_to_string(&f).unwrap()
        };
        assert_eq!(run(Some(WSL_CONF)), WSL_CONF);
        assert_eq!(run(Some("[network]\nhostname = box\n[boot]\ncommand = /old/start\nsystemd = false\n")), format!("[network]\nhostname = box\n[boot]\n{want}\nsystemd = false\n"));
        assert_eq!(run(Some("[ boot ]\n  command=/a\ncommand = /b\n")), format!("[ boot ]\n{want}\n"));
        assert_eq!(run(Some("[user]\ndefault = root\n")), format!("[user]\ndefault = root\n\n[boot]\n{want}\n"));
        // Saved with CRLF, and a header with a comment: still the one [boot].
        assert_eq!(run(Some("[boot]\r\nsystemd = false\r\ncommand = /old\r\n[user]\r\ndefault = root\r\n")), format!("[boot]\r\nsystemd = false\r\n{want}\n[user]\r\ndefault = root\r\n"));
        assert_eq!(run(Some("[boot] # Kiri's\ncommand=/old # mine\n")), format!("[boot] # Kiri's\n{want}\n"));
        // A commented-out command is left, and one added.
        assert_eq!(run(Some("[boot]\n# command = /off\n")), format!("[boot]\n# command = /off\n{want}\n"));
        assert_eq!(run(None), format!("[boot]\n{want}\n"));
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// A step that fails ends the script with why: here base64 -d.
    #[test]
    fn a_failed_step_fails_the_script() {
        let (from, root, path) = stand_in("fail");
        let tools = path.split(':').next().unwrap();
        std::fs::write(format!("{tools}/base64"), "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(format!("{tools}/base64"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = Command::new("sh").args(["-c", &sync_script(&from, &root.to_string_lossy(), true)]).env("PATH", &path).output().unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "could not decode kiri-start in WSL");
        assert!(!root.join("usr/local/bin/kiri-start").exists());
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// No backslash in the script: how wsl.exe reads one in an argument
    /// is Windows' quoting, not sh's.
    #[test]
    fn the_script_has_no_backslash() {
        assert!(!sync_script(Path::new("C:/Users/x/AppData/Local/Kiri"), "", true).contains('\\'));
    }

    /// An engine older than this app is restarted once: not again after
    /// that (a restart that failed does not loop), not while it does not
    /// answer, not when it is not behind (current, newer, a development
    /// build), and one asked while a restart runs returns at once.
    #[test]
    fn a_stale_engine_is_caught_up_once() {
        let behind = |v: &str| v == "0.1.18";
        let c = CatchUp::new();
        let restarts = std::sync::atomic::AtomicUsize::new(0);
        let restart = |v: &str| {
            assert_eq!(v, "0.1.18");
            restarts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        };
        // Not answering: nothing done, asked again next time.
        assert!(!c.run(|| None, behind, restart));
        assert!(c.run(|| Some("0.1.18".into()), behind, restart));
        // Still old (that restart failed): not again.
        assert!(!c.run(|| Some("0.1.18".into()), behind, restart));
        assert_eq!(restarts.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Not behind: never restarted, and not asked again.
        let c = CatchUp::new();
        assert!(!c.run(|| Some("0.1.20".into()), behind, |_| panic!("restarted an engine that is not behind")));
        assert!(!c.run(|| panic!("asked again"), behind, |_| {}));
        // One asked while a restart runs: back at once, nothing run.
        let c = std::sync::Arc::new(CatchUp::new());
        let (began_tx, began) = std::sync::mpsc::channel();
        let (end, end_rx) = std::sync::mpsc::channel::<()>();
        let first = {
            let c = c.clone();
            std::thread::spawn(move || {
                c.run(
                    || Some("0.1.18".into()),
                    behind,
                    |_| {
                        began_tx.send(()).unwrap();
                        end_rx.recv().unwrap();
                    },
                )
            })
        };
        began.recv().unwrap();
        let t = std::time::Instant::now();
        assert!(!c.run(|| panic!("asked during a restart"), behind, |_| panic!("two restarts")));
        assert!(t.elapsed() < std::time::Duration::from_secs(1));
        end.send(()).unwrap();
        assert!(first.join().unwrap());
    }

    #[test]
    fn base64_as_base64_d_reads_it() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }

    /// sh runs the script itself (-e), not the distro's login shell (--),
    /// which would expand its `$` first.
    #[test]
    fn runs_sh_itself() {
        assert_eq!(wsl_sh("kiri", "echo $$"), ["-d", "kiri", "-u", "root", "-e", "sh", "-c", "echo $$"]);
    }

    #[test]
    fn quotes_one_word() {
        assert_eq!(sh_quote("C:/Users/O'Neil/x"), r"'C:/Users/O'\''Neil/x'");
    }
}
