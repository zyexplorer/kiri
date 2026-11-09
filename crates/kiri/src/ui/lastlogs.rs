//! A stopped container's last log lines, for the page to say why it
//! stopped: the Mac's window and Windows' (crates/kiri-win, by #[path])
//! answer `lastlogs` with them. The page reads the reason from the lines.

/// How many lines the page gets: enough for an image's whole start-up error.
pub const KEEP: usize = 60;

/// How far back the Logs tab starts: the page keeps this many lines
/// (`LOG_MAX` in index.html), and a log on disk can be far longer.
pub const LIVE_TAIL: usize = 5000;

/// The text of a `/containers/{id}/logs` answer, without Docker's 8-byte
/// frame headers unless the container has a TTY, as its last `keep` lines.
pub fn lines(body: &[u8], tty: bool, keep: usize) -> Vec<String> {
    let mut text = Vec::with_capacity(body.len());
    if tty {
        text.extend_from_slice(body);
    } else {
        // [stream, 0, 0, 0, length u32 BE] then the data.
        let mut rest = body;
        while rest.len() >= 8 {
            let len = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
            let end = (8 + len).min(rest.len());
            text.extend_from_slice(&rest[8..end]);
            rest = &rest[end..];
        }
    }
    let text = String::from_utf8_lossy(&text);
    let all: Vec<String> = text
        .split('\n')
        .map(|l| l.trim_end_matches('\r').to_owned())
        .filter(|l| !l.trim().is_empty())
        .collect();
    all[all.len().saturating_sub(keep)..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(stream: u8, s: &str) -> Vec<u8> {
        let mut f = vec![stream, 0, 0, 0];
        f.extend_from_slice(&(s.len() as u32).to_be_bytes());
        f.extend_from_slice(s.as_bytes());
        f
    }

    #[test]
    fn reads_framed_and_tty_logs() {
        let mut body = frame(1, "starting\n");
        body.extend(frame(2, "Error: You must specify POSTGRES_PASSWORD\n\n"));
        assert_eq!(
            lines(&body, false, 60),
            ["starting", "Error: You must specify POSTGRES_PASSWORD"]
        );
        assert_eq!(lines(b"a\r\nb\r\nc\r\n", true, 2), ["b", "c"]);
        assert!(lines(b"", false, 60).is_empty());
    }
}
