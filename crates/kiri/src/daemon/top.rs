//! `docker top`'s `ps_args`. Docker runs `ps` with them (default `-ef`) and
//! keeps the container's rows. The guest has no `ps`: it sends each process's
//! fields (see `kiri_proto::Msg::Top`), and the columns `ps` would print for
//! the arguments are picked from them here.

use serde_json::{Value, json};

use super::engine::Error;

/// Where a column's value comes from in a guest line.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Col {
    /// A field as the guest sent it.
    Field(usize),
    /// The state letter alone (`s`, `state`).
    State,
    /// CPU time as BSD `ps` prints it: `m:ss`.
    BsdTime,
    /// CPU time as `-o time` prints it: `[DD-]HH:MM:SS`.
    Time,
    /// Time since start: `[[DD-]HH:]MM:SS`.
    Etime,
}

// Guest line fields; the command line is always the last.
const USER: usize = 0;
const PID: usize = 1;
const PPID: usize = 2;
const C: usize = 3;
const STIME: usize = 4;
const TTY: usize = 5;
const TIME: usize = 6;
const COMM: usize = 7;
const STAT: usize = 8;
const RSS: usize = 9;
const VSZ: usize = 10;
const PCPU: usize = 11;
const PMEM: usize = 12;
const ETIME: usize = 13;
const CPU_SECS: usize = 14;
const UID: usize = 15;
const CMD: usize = 16;
const FIELDS: usize = 17;

/// A `-o` column name: procps's header for it and its value.
fn named(name: &str) -> Option<(&'static str, Col)> {
    Some(match name {
        "pid" | "tgid" => ("PID", Col::Field(PID)),
        "ppid" => ("PPID", Col::Field(PPID)),
        "uid" | "euid" => ("UID", Col::Field(UID)),
        "user" | "euser" | "uname" | "ruser" => ("USER", Col::Field(USER)),
        "c" => ("C", Col::Field(C)),
        "stime" | "start_time" => ("STIME", Col::Field(STIME)),
        "start" | "bsdstart" => ("START", Col::Field(STIME)),
        "tty" | "tname" | "tt" => ("TT", Col::Field(TTY)),
        "time" | "cputime" => ("TIME", Col::Time),
        "bsdtime" => ("TIME", Col::BsdTime),
        "etime" => ("ELAPSED", Col::Etime),
        "etimes" => ("ELAPSED", Col::Field(ETIME)),
        "comm" | "ucomm" | "ucmd" => ("COMMAND", Col::Field(COMM)),
        "args" | "command" => ("COMMAND", Col::Field(CMD)),
        "cmd" => ("CMD", Col::Field(CMD)),
        "stat" => ("STAT", Col::Field(STAT)),
        "s" | "state" => ("S", Col::State),
        "rss" | "rssize" | "rsz" => ("RSS", Col::Field(RSS)),
        "vsz" | "vsize" => ("VSZ", Col::Field(VSZ)),
        "%cpu" | "pcpu" => ("%CPU", Col::Field(PCPU)),
        "%mem" | "pmem" => ("%MEM", Col::Field(PMEM)),
        _ => return None,
    })
}

fn unsupported(what: &str) -> Error {
    Error::BadRequest(format!("ps_args: {what} is not supported (supported: -ef, -e, -f, aux, ax, -o/-eo with ps column names)"))
}

/// The columns `ps` prints for these arguments, as (title, value).
fn columns(ps_args: &str) -> Result<Vec<(String, Col)>, Error> {
    let args = if ps_args.trim().is_empty() { "-ef" } else { ps_args };
    let (mut bsd, mut user_fmt, mut full) = (false, false, false);
    let mut picked: Vec<(String, Col)> = Vec::new();
    let mut words = args.split_whitespace();
    while let Some(word) = words.next() {
        // procps reads `-aux` as BSD `aux`.
        let (dash, letters) = match word.strip_prefix('-') {
            Some(_) if word == "-aux" => (false, "aux"),
            Some(rest) if !rest.starts_with('-') => (true, rest),
            Some(_) => return Err(unsupported(word)),
            None => (false, word),
        };
        bsd |= !dash;
        for (i, ch) in letters.char_indices() {
            match ch {
                // Which processes: Docker keeps the container's anyway.
                'e' | 'A' if dash => {}
                'a' | 'x' if !dash => {}
                'w' => {}
                'f' if dash => full = true,
                'u' if !dash => user_fmt = true,
                'o' => {
                    let rest = &letters[i + 1..];
                    let list = if rest.is_empty() { words.next().ok_or_else(|| unsupported("-o with no columns"))? } else { rest };
                    for item in list.split(',').filter(|s| !s.is_empty()) {
                        let (name, title) = item.split_once('=').map_or((item, None), |(n, t)| (n, Some(t)));
                        let (default, col) = named(name).ok_or_else(|| unsupported(&format!("column {name:?}")))?;
                        picked.push((title.unwrap_or(default).to_owned(), col));
                    }
                    break;
                }
                _ => return Err(unsupported(&format!("{}{ch}", if dash { "-" } else { "" }))),
            }
        }
    }
    if !picked.is_empty() {
        return Ok(picked);
    }
    let set = |cols: &[(&str, Col)]| cols.iter().map(|(t, c)| ((*t).to_owned(), *c)).collect();
    Ok(match (bsd, user_fmt, full) {
        (true, true, _) => set(&[
            ("USER", Col::Field(USER)),
            ("PID", Col::Field(PID)),
            ("%CPU", Col::Field(PCPU)),
            ("%MEM", Col::Field(PMEM)),
            ("VSZ", Col::Field(VSZ)),
            ("RSS", Col::Field(RSS)),
            ("TTY", Col::Field(TTY)),
            ("STAT", Col::Field(STAT)),
            ("START", Col::Field(STIME)),
            ("TIME", Col::BsdTime),
            ("COMMAND", Col::Field(CMD)),
        ]),
        (true, false, _) => set(&[("PID", Col::Field(PID)), ("TTY", Col::Field(TTY)), ("STAT", Col::Field(STAT)), ("TIME", Col::BsdTime), ("COMMAND", Col::Field(CMD))]),
        (false, _, true) => set(&[
            ("UID", Col::Field(USER)),
            ("PID", Col::Field(PID)),
            ("PPID", Col::Field(PPID)),
            ("C", Col::Field(C)),
            ("STIME", Col::Field(STIME)),
            ("TTY", Col::Field(TTY)),
            ("TIME", Col::Field(TIME)),
            ("CMD", Col::Field(CMD)),
        ]),
        (false, _, false) => set(&[("PID", Col::Field(PID)), ("TTY", Col::Field(TTY)), ("TIME", Col::Time), ("CMD", Col::Field(COMM))]),
    })
}

/// A guest line as its fields. An older agent's line (seven fields and the
/// command) leaves the newer ones as `-`.
fn fields(line: &str) -> Vec<&str> {
    let mut f: Vec<&str> = line.splitn(FIELDS, '\t').collect();
    if f.len() < FIELDS {
        let cmd = f.pop().unwrap_or("");
        f.resize(FIELDS - 1, "-");
        f.push(cmd);
    }
    f
}

fn clock(secs: u64, always_hours: bool) -> String {
    let (d, h, m, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (d, h) {
        (0, 0) if !always_hours => format!("{m:02}:{s:02}"),
        (0, _) => format!("{h:02}:{m:02}:{s:02}"),
        _ => format!("{d}-{h:02}:{m:02}:{s:02}"),
    }
}

fn value(f: &[&str], col: Col) -> String {
    let secs = |i: usize| f[i].parse::<u64>().ok();
    match col {
        Col::Field(i) => f[i].to_owned(),
        Col::State => f[STAT].chars().next().map_or_else(|| "-".to_owned(), String::from),
        Col::BsdTime => secs(CPU_SECS).map_or_else(|| "-".to_owned(), |s| format!("{}:{:02}", s / 60, s % 60)),
        Col::Time => secs(CPU_SECS).map_or_else(|| f[TIME].to_owned(), |s| clock(s, true)),
        Col::Etime => secs(ETIME).map_or_else(|| "-".to_owned(), |s| clock(s, false)),
    }
}

/// Docker's answer for `GET /containers/{id}/top`. Docker finds each row's
/// process by the PID column, so the columns must include it.
pub fn answer(ps_args: &str, lines: &[String]) -> Result<Value, Error> {
    let cols = columns(ps_args)?;
    if !cols.iter().any(|(_, c)| *c == Col::Field(PID)) {
        return Err(Error::BadRequest("Couldn't find PID field in ps output".into()));
    }
    let procs: Vec<Vec<String>> = lines
        .iter()
        .map(|l| {
            let f = fields(l);
            cols.iter().map(|(_, c)| value(&f, *c)).collect()
        })
        .collect();
    let titles: Vec<&str> = cols.iter().map(|(t, _)| t.as_str()).collect();
    Ok(json!({"Titles": titles, "Processes": procs}))
}

/// Checks the arguments before the guest is asked.
pub fn check(ps_args: &str) -> Result<(), Error> {
    answer(ps_args, &[]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "root\t7\t1\t0\t05:43\t?\t00:01:05\tcat\tSs\t1024\t2048\t0.5\t0.1\t3725\t65\t0\tcat -n";

    fn ok(args: &str) -> Value {
        answer(args, &[LINE.to_owned()]).unwrap()
    }

    #[test]
    fn the_default_is_ps_ef() {
        let want = json!({"Titles": ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"], "Processes": [["root", "7", "1", "0", "05:43", "?", "00:01:05", "cat -n"]]});
        assert_eq!(ok(""), want);
        assert_eq!(ok("-ef"), want);
        assert_eq!(ok("-e -f"), want);
    }

    #[test]
    fn aux_has_bsd_user_columns() {
        let want = json!({
            "Titles": ["USER", "PID", "%CPU", "%MEM", "VSZ", "RSS", "TTY", "STAT", "START", "TIME", "COMMAND"],
            "Processes": [["root", "7", "0.5", "0.1", "2048", "1024", "?", "Ss", "05:43", "1:05", "cat -n"]],
        });
        assert_eq!(ok("aux"), want);
        assert_eq!(ok("-aux"), want);
        assert_eq!(ok("ax")["Titles"], json!(["PID", "TTY", "STAT", "TIME", "COMMAND"]));
        assert_eq!(ok("-e")["Processes"], json!([["7", "?", "00:01:05", "cat"]]));
    }

    #[test]
    fn o_picks_the_columns_named() {
        assert_eq!(ok("-eo pid,rss,args"), json!({"Titles": ["PID", "RSS", "COMMAND"], "Processes": [["7", "1024", "cat -n"]]}));
        assert_eq!(ok("-o pid,s,etime,comm"), json!({"Titles": ["PID", "S", "ELAPSED", "COMMAND"], "Processes": [["7", "S", "01:02:05", "cat"]]}));
        assert_eq!(ok("o pid=ID,vsz -o %mem")["Titles"], json!(["ID", "VSZ", "%MEM"]), "a title given with =, and -o twice");
    }

    #[test]
    fn what_ps_cannot_do_here_is_refused() {
        for args in ["-eo rss,args", "-l", "--forest", "-eo pid,nosuch", "-o"] {
            assert!(matches!(answer(args, &[]), Err(Error::BadRequest(_))), "{args}");
        }
    }

    #[test]
    fn an_older_agents_line_still_gives_ps_ef() {
        let old = "root\t7\t1\t0\t05:43\t?\t00:00:00\tsh -c x".to_owned();
        assert_eq!(answer("-ef", std::slice::from_ref(&old)).unwrap()["Processes"], json!([["root", "7", "1", "0", "05:43", "?", "00:00:00", "sh -c x"]]));
        assert_eq!(answer("-eo pid,rss", &[old]).unwrap()["Processes"], json!([["7", "-"]]));
    }
}
