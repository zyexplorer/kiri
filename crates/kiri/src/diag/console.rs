//! Guest serial output, kept locally with the same permissions and
//! redaction as the daemon log. No container environment or I/O is added.

use std::io::{self, BufRead, BufReader, Read};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

const MAX_FILE: u64 = 1 << 20;
const MAX_LINE: u64 = 16 << 10;

pub fn path() -> io::Result<PathBuf> {
    super::logs_dir()
        .map(|d| d.join("guest-console.log"))
        .ok_or_else(|| io::Error::other("HOME is not set"))
}

pub fn capture(path: &Path) -> io::Result<OwnedFd> {
    let (fd, _reader) = capture_to(path, MAX_FILE)?;
    Ok(fd)
}

fn capture_to(
    path: &Path,
    max: u64,
) -> io::Result<(OwnedFd, std::thread::JoinHandle<io::Result<()>>)> {
    super::append_to(path, "=== guest console ===", max)?;
    let (reader, writer) = UnixStream::pair()?;
    let path = path.to_owned();
    let thread = std::thread::Builder::new()
        .name("kiri-console".into())
        .spawn(move || {
            let result = record(BufReader::new(reader), &path, max);
            if let Err(e) = &result {
                crate::diag::error!(
                    "guest console could not be saved",
                    file = path.display(),
                    error = e
                );
            }
            result
        })?;
    Ok((writer.into(), thread))
}

fn record(mut reader: impl BufRead, path: &Path, max: u64) -> io::Result<()> {
    let redactor = super::redact::Redactor::home_only();
    let mut line = Vec::new();
    let mut private_key = false;
    loop {
        line.clear();
        if reader
            .by_ref()
            .take(MAX_LINE + 1)
            .read_until(b'\n', &mut line)?
            == 0
        {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&line);
        if text.contains("-----BEGIN ") && text.contains("PRIVATE KEY-----") {
            private_key = true;
        }
        let omitted = line.len() as u64 > MAX_LINE;
        if omitted && !line.ends_with(b"\n") {
            reader.skip_until(b'\n')?;
        }
        let output = if private_key {
            if text.contains("-----END ") && text.contains("PRIVATE KEY-----") {
                private_key = false;
            }
            "[redacted private key]".to_owned()
        } else if omitted {
            "[guest console line omitted: exceeds 16 KiB]".to_owned()
        } else {
            redactor.apply(text.trim_end_matches(['\r', '\n']))
        };
        if let Err(e) = super::append_to(path, &output, max) {
            // A full disk must not block (or close) the VM's serial port.
            let _ = io::copy(&mut reader, &mut io::sink());
            return Err(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kiri-console-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn serial_eof_keeps_oops_and_redacts_split_secrets() {
        let dir = dir("eof");
        let path = dir.join("logs/guest-console.log");
        let (fd, reader) = capture_to(&path, MAX_FILE).unwrap();
        let mut serial = std::fs::File::from(fd);
        serial.write_all(b"password=hun").unwrap();
        // A key block, put together here so the file itself holds none.
        let key = format!("ter2\n-----BEGIN {k}-----\nprivate-data\n-----END {k}-----\n", k = ["PRIVATE", "KEY"].join(" "));
        serial.write_all(key.as_bytes()).unwrap();
        serial
            .write_all(b"kernel BUG at net/core/skbuff.c:4854!\nlast frame")
            .unwrap();
        drop(serial);
        reader.join().unwrap().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("kernel BUG at net/core/skbuff.c:4854!\nlast frame\n"));
        assert!(!text.contains("hunter2"));
        assert!(!text.contains("private-data"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rotation_failure_does_not_grow_the_current_file() {
        let dir = dir("rotation-error");
        let path = dir.join("guest-console.log");
        record(io::Cursor::new(format!("{}\n", "x".repeat(99))), &path, 128).unwrap();
        // A nonempty directory blocks each rename in the rotation chain.
        for n in 1..=2 {
            let rotated = super::super::rotated(&path, n);
            std::fs::create_dir(&rotated).unwrap();
            std::fs::write(rotated.join("blocker"), b"occupied").unwrap();
        }
        let result = record(io::Cursor::new(format!("{}\n", "y".repeat(99))), &path, 128);
        assert!(result.is_err(), "failed rotation must stop writing");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 100);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unbounded_lines_are_omitted_and_files_stay_capped_across_boots() {
        let dir = dir("bounded");
        let path = dir.join("guest-console.log");
        let input = format!("{}\nkernel BUG\n", "x".repeat(MAX_LINE as usize * 4));
        record(io::Cursor::new(input), &path, 128).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("kernel BUG"));
        assert!(text.contains("omitted"));
        assert!(!text.contains(&"x".repeat(32)));
        for _ in 0..20 {
            let (fd, reader) = capture_to(&path, 128).unwrap();
            let mut serial = std::fs::File::from(fd);
            serial.write_all(b"next boot\n").unwrap();
            drop(serial);
            reader.join().unwrap().unwrap();
        }
        for p in [
            &path,
            &super::super::rotated(&path, 1),
            &super::super::rotated(&path, 2),
        ] {
            assert!(
                std::fs::metadata(p).unwrap().len() <= 128,
                "{}",
                p.display()
            );
        }
        assert!(!super::super::rotated(&path, 3).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
