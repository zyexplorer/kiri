//! Registry tag lists: following their pages, and putting them newest
//! first. The engine's registry client uses it, and so does the Windows
//! window (crates/kiri-win/src/hub.rs, by #[path]: it doesn't link this crate).

/// The next page's path from a `Link` header, as the distribution spec
/// sends it: `</v2/o/r/tags/list?last=x&n=1000>; rel="next"`.
pub fn next_link(link: &str) -> Option<&str> {
    link.split(',').find(|l| l.contains("rel=\"next\"")).and_then(|l| l.split_once('<')).and_then(|(_, rest)| rest.split_once('>')).map(|(path, _)| path)
}

/// Tag names compared as versions: runs of digits by their value, so
/// `1.10` comes after `1.9`, and the rest as text.
pub fn version_order(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(s: &str) -> Vec<Result<u64, String>> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(c) = rest.chars().next() {
            let digit = c.is_ascii_digit();
            let end = rest.find(|x: char| x.is_ascii_digit() != digit).unwrap_or(rest.len());
            let (run, tail) = rest.split_at(end);
            out.push(if digit { run.parse::<u64>().map_err(|_| run.to_owned()) } else { Err(run.to_owned()) });
            rest = tail;
        }
        out
    }
    let (pa, pb) = (parts(a), parts(b));
    for (x, y) in pa.iter().zip(&pb) {
        let o = match (x, y) {
            (Ok(m), Ok(n)) => m.cmp(n),
            (Err(m), Err(n)) => m.cmp(n),
            // A number sorts after text: `1.2` is newer than `1.2-rc`'s "-".
            (Ok(_), Err(_)) => std::cmp::Ordering::Greater,
            (Err(_), Ok(_)) => std::cmp::Ordering::Less,
        };
        if o.is_ne() {
            return o;
        }
    }
    // One is the other and more: `1.2-rc1` (a pre-release) is older than
    // `1.2`, and `1.2.1` newer.
    // So is `2026.11.0b6` (PEP 440's a/b/rc, as Home Assistant tags them).
    let pre = |longer: &[Result<u64, String>], at: usize| matches!(longer.get(at), Some(Err(t)) if t.starts_with('-') || ["a", "b", "rc", ".dev", "dev"].contains(&t.as_str()));
    match pa.len().cmp(&pb.len()) {
        std::cmp::Ordering::Greater if pre(&pa, pb.len()) => std::cmp::Ordering::Less,
        std::cmp::Ordering::Less if pre(&pb, pa.len()) => std::cmp::Ordering::Greater,
        o => o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn betas_without_a_dash_come_before_their_release() {
        let mut tags = ["2026.11.0b5", "2026.11.1", "2026.11.0", "2026.11.0b6", "2026.11.0rc1", "2026.10.4"].map(String::from).to_vec();
        tags.sort_by(|a, b| version_order(b, a));
        assert_eq!(tags, ["2026.11.1", "2026.11.0", "2026.11.0rc1", "2026.11.0b6", "2026.11.0b5", "2026.10.4"]);
    }
}
