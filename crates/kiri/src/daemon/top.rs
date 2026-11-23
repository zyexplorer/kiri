//! `docker top`'s `ps_args`. Docker runs `ps` with them (default `-ef`) and
//! keeps the container's rows. The guest has no `ps`: it sends each process's
//! fields (see `kiri_proto::Msg::Top`), and the columns `ps` would print for
//! the arguments are picked from them here. Then the text is read back the
//! way Docker reads `ps`'s output (`parsePSOutput` in moby's
//! `daemon/top_unix.go`): titles and values split at whitespace, the PID
//! column found by its title.

use std::cmp::Ordering;
use std::io;

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
    /// Start time (`stime`, `start`): the guest's clock time; sorts oldest first.
    Start,
    /// One of ps's priority columns, `a + b * priority` from the kernel's
    /// priority (`/proc/PID/stat` field 18).
    Prio(i64, i64),
    /// A column ps has but the guest does not send: what ps prints when it
    /// has no value (`ADDR` is always `-` on Linux; `WCHAN` is `-` for a
    /// running process).
    Dash,
}

/// How ps cuts a value too wide for its column (never in the last column).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cut {
    /// Numbers and short fields: printed whole.
    No,
    /// Command names and lines: cut at the width.
    Text(usize),
    /// User names (or numbers, without one): cut with a `+` at the end.
    User(usize),
}

#[derive(Clone, Debug)]
struct Column {
    title: String,
    col: Col,
    cut: Cut,
}

/// What the arguments ask for: the columns and the sort keys (column,
/// descending).
struct Format {
    cols: Vec<Column>,
    sort: Vec<(Col, bool)>,
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
const F: usize = 16;
const PRIO: usize = 17;
const NI: usize = 18;
const SZ: usize = 19;
const CMD: usize = 20;
const FIELDS: usize = 21;

/// A `-o` column name: procps's header for it, its value and how a width
/// cuts it (with procps's default width).
fn named(name: &str) -> Option<(&'static str, Col, Cut)> {
    let n = Cut::No;
    Some(match name {
        "pid" => ("PID", Col::Field(PID), n),
        "tgid" => ("TGID", Col::Field(PID), n),
        "ppid" => ("PPID", Col::Field(PPID), n),
        "uid" => ("UID", Col::Field(UID), n),
        "euid" => ("EUID", Col::Field(UID), n),
        "user" | "uname" => ("USER", Col::Field(USER), Cut::User(8)),
        "euser" => ("EUSER", Col::Field(USER), Cut::User(8)),
        "ruser" => ("RUSER", Col::Field(USER), Cut::User(8)),
        "c" => ("C", Col::Field(C), n),
        "stime" => ("STIME", Col::Start, n),
        "start_time" | "bsdstart" => ("START", Col::Start, n),
        "start" => ("STARTED", Col::Start, n),
        "tty" | "tt" => ("TT", Col::Field(TTY), n),
        "tname" => ("TTY", Col::Field(TTY), n),
        "time" | "cputime" => ("TIME", Col::Time, n),
        "bsdtime" => ("TIME", Col::BsdTime, n),
        "etime" => ("ELAPSED", Col::Etime, n),
        "etimes" => ("ELAPSED", Col::Field(ETIME), n),
        "comm" | "ucomm" => ("COMMAND", Col::Field(COMM), Cut::Text(15)),
        "ucmd" => ("CMD", Col::Field(COMM), Cut::Text(15)),
        "args" | "command" => ("COMMAND", Col::Field(CMD), Cut::Text(27)),
        "cmd" => ("CMD", Col::Field(CMD), Cut::Text(27)),
        "stat" => ("STAT", Col::Field(STAT), n),
        "s" | "state" => ("S", Col::State, n),
        "rss" | "rssize" => ("RSS", Col::Field(RSS), n),
        "rsz" => ("RSZ", Col::Field(RSS), n),
        "vsz" | "vsize" => ("VSZ", Col::Field(VSZ), n),
        "sz" => ("SZ", Col::Field(SZ), n),
        "%cpu" | "pcpu" => ("%CPU", Col::Field(PCPU), n),
        "%mem" | "pmem" => ("%MEM", Col::Field(PMEM), n),
        "f" | "flag" | "flags" => ("F", Col::Field(F), n),
        "ni" | "nice" => ("NI", Col::Field(NI), n),
        "pri" => ("PRI", Col::Prio(39, -1), n),
        "opri" => ("PRI", Col::Prio(60, 1), n),
        "priority" => ("PRI", Col::Prio(0, 1), n),
        "pri_foo" => ("FOO", Col::Prio(-20, 1), n),
        "pri_bar" => ("BAR", Col::Prio(1, 1), n),
        "pri_baz" => ("BAZ", Col::Prio(100, 1), n),
        "addr" => ("ADDR", Col::Dash, n),
        "wchan" => ("WCHAN", Col::Dash, Cut::Text(6)),
        "nwchan" => ("WCHAN", Col::Dash, n),
        _ => return None,
    })
}

const SUPPORTED: &str = "-e, -f, -l, -y, aux, ax, l, w, -o/-eo with ps column names (name:width, name=title), --sort/k with column names";

fn unsupported(what: &str) -> Error {
    Error::BadRequest(format!("ps_args: {what} is not supported (supported: {SUPPORTED})"))
}

/// An error ps itself prints for these arguments: Docker answers with its
/// first line, as a server error.
fn ps_error(what: &str) -> Error {
    Error::Io(io::Error::other(format!("ps: error: {what}")))
}

/// moby's `validatePSArgs`: a title starting with `PID` is refused on
/// anything but `pid`, matched as `\s+([^\s]*)=\s*(PID[^\s]*)` (so the name
/// is the whole word before the `=`, commas included).
fn validate(ps_args: &str) -> Result<(), Error> {
    let b = ps_args.as_bytes();
    let space = |c: u8| c.is_ascii_whitespace();
    let mut i = 0;
    while i < b.len() {
        // A match starts after a run of whitespace.
        if !space(b[i]) {
            i += 1;
            continue;
        }
        while i < b.len() && space(b[i]) {
            i += 1;
        }
        // `[^\s]*` takes `=` too, then backs off to the last one followed
        // by `\s*PID`.
        let k0 = i;
        let mut end = i;
        while end < b.len() && !space(b[end]) {
            end += 1;
        }
        let word = &ps_args[k0..end];
        let mut found = None;
        for (at, _) in word.match_indices('=').collect::<Vec<_>>().into_iter().rev() {
            let after = &ps_args[k0 + at + 1..];
            let v0 = after.len() - after.trim_start_matches(|c: char| c.is_ascii_whitespace()).len();
            let v = &after[v0..];
            if v.starts_with("PID") {
                let vend = v.find(|c: char| c.is_ascii_whitespace()).unwrap_or(v.len());
                found = Some((&word[..at], &v[..vend]));
                break;
            }
        }
        if let Some((k, v)) = found
            && k != "pid"
        {
            return Err(Error::Io(io::Error::other(format!("specifying \"{k}={v}\" is not allowed"))));
        }
        i = end;
    }
    Ok(())
}

/// One `-o` argument: comma-separated `name[:width][=title]` (procps 4 ends
/// a title at the next comma).
fn format_list(list: &str, out: &mut Vec<Column>) -> Result<(), Error> {
    let items: Vec<&str> = list.split(',').collect();
    for (i, item) in items.iter().enumerate() {
        if item.is_empty() {
            // `pid,` is fine; `,pid` and `pid,,comm` are not.
            if i > 0 && i + 1 == items.len() {
                continue;
            }
            return Err(ps_error("improper format list"));
        }
        let (spec, title) = item.split_once('=').map_or((*item, None), |(n, t)| (n, Some(t)));
        let (name, width) = match spec.split_once(':') {
            Some((n, w)) => {
                let ok = w.bytes().all(|c| c.is_ascii_digit()) && !w.starts_with('0') && !w.is_empty();
                (n, Some(w.parse::<usize>().ok().filter(|_| ok).ok_or_else(|| ps_error("column widths must be unsigned decimal numbers"))?))
            }
            None => (spec, None),
        };
        let (default, col, cut) = named(name).ok_or_else(|| unsupported(&format!("column {name:?}")))?;
        let cut = match (cut, width) {
            (Cut::Text(_), Some(w)) => Cut::Text(w),
            (Cut::User(_), Some(w)) => Cut::User(w),
            (c, _) => c,
        };
        let title = title.unwrap_or(default);
        out.push(Column { title: title.to_owned(), col, cut });
    }
    Ok(())
}

/// A sort list: comma-separated column names, each `+` (the default) or `-`.
fn sort_list(list: &str, out: &mut Vec<(Col, bool)>) -> Result<(), Error> {
    let items: Vec<&str> = list.split(',').collect();
    for (i, item) in items.iter().enumerate() {
        if item.is_empty() {
            if i > 0 && i + 1 == items.len() {
                continue;
            }
            return Err(ps_error("improper sort list"));
        }
        let (desc, name) = match item.as_bytes()[0] {
            b'-' => (true, &item[1..]),
            b'+' => (false, &item[1..]),
            _ => (false, *item),
        };
        let (_, col, _) = named(name).ok_or_else(|| unsupported(&format!("sort key {name:?}")))?;
        out.push((col, desc));
    }
    Ok(())
}

fn col(title: &str, col: Col, cut: Cut) -> Column {
    Column { title: title.to_owned(), col, cut }
}

/// The columns `ps` prints for these arguments, and the sort.
fn format(ps_args: &str) -> Result<Format, Error> {
    let args = if ps_args.trim().is_empty() { "-ef" } else { ps_args };
    let (mut bsd, mut user_fmt, mut full, mut long, mut bsd_long, mut y) = (false, false, false, false, false, false);
    let mut picked: Vec<Column> = Vec::new();
    let mut has_list = false;
    let mut sort = Vec::new();
    let mut words = args.split_whitespace();
    while let Some(word) = words.next() {
        if let Some(rest) = word.strip_prefix("--sort") {
            let list = match rest.strip_prefix('=') {
                Some(l) => l,
                None if rest.is_empty() => words.next().unwrap_or(""),
                None => return Err(unsupported(word)),
            };
            if list.is_empty() {
                return Err(ps_error("long sort specification must follow --sort"));
            }
            sort_list(list, &mut sort)?;
            continue;
        }
        // procps reads `-aux` as BSD `aux`.
        let (dash, letters) = match word.strip_prefix('-') {
            Some(_) if word == "-aux" => (false, "aux"),
            Some(rest) if !rest.starts_with('-') && !rest.is_empty() => (true, rest),
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
                'l' if dash => long = true,
                'l' => bsd_long = true,
                'y' if dash => y = true,
                'u' if !dash => user_fmt = true,
                'o' | 'k' => {
                    let rest = &letters[i + 1..];
                    let list = if rest.is_empty() { words.next().unwrap_or("") } else { rest };
                    if ch == 'o' {
                        if list.is_empty() {
                            return Err(unsupported("-o with no columns"));
                        }
                        has_list = true;
                        format_list(list, &mut picked)?;
                    } else if dash {
                        return Err(unsupported("-k"));
                    } else {
                        if list.is_empty() {
                            return Err(ps_error("long sort specification must follow 'k'"));
                        }
                        sort_list(list, &mut sort)?;
                    }
                    break;
                }
                _ => return Err(unsupported(&format!("{}{ch}", if dash { "-" } else { "" }))),
            }
        }
    }
    if has_list {
        if long || bsd_long || user_fmt {
            return Err(ps_error("conflicting format options"));
        }
        if y {
            return Err(ps_error("can not use output modifiers with user-defined output"));
        }
        return Ok(Format { cols: picked, sort });
    }
    if (user_fmt && (full || long || bsd_long)) || (bsd_long && (full || long)) {
        return Err(ps_error("conflicting format options"));
    }
    if y && !long && !bsd_long && !user_fmt {
        return Err(ps_error("modifier -y without format -l makes no sense"));
    }
    let n = Cut::No;
    let cols = if user_fmt {
        vec![
            col("USER", Col::Field(USER), Cut::User(8)),
            col("PID", Col::Field(PID), n),
            col("%CPU", Col::Field(PCPU), n),
            col("%MEM", Col::Field(PMEM), n),
            col("VSZ", Col::Field(VSZ), n),
            col("RSS", Col::Field(RSS), n),
            col("TTY", Col::Field(TTY), n),
            col("STAT", Col::Field(STAT), n),
            col("START", Col::Field(STIME), n),
            col("TIME", Col::BsdTime, n),
            col("COMMAND", Col::Field(CMD), n),
        ]
    } else if bsd_long {
        // BSD `l`; `-y` drops F.
        let mut v = vec![
            col("F", Col::Field(F), n),
            col("UID", Col::Field(UID), n),
            col("PID", Col::Field(PID), n),
            col("PPID", Col::Field(PPID), n),
            col("PRI", Col::Prio(0, 1), n),
            col("NI", Col::Field(NI), n),
            col("VSZ", Col::Field(VSZ), n),
            col("RSS", Col::Field(RSS), n),
            col("WCHAN", Col::Dash, Cut::Text(6)),
            col("STAT", Col::Field(STAT), n),
            col("TTY", Col::Field(TTY), n),
            col("TIME", Col::BsdTime, n),
            col("COMMAND", Col::Field(CMD), n),
        ];
        if y {
            v.remove(0);
        }
        v
    } else if long {
        // `-l`: `-f` names the user, adds STIME and the command line; `-y`
        // drops F and shows RSS for ADDR; a BSD option gives BSD's time and
        // the command line.
        let mut v = Vec::new();
        if !y {
            v.push(col("F", Col::Field(F), n));
        }
        v.push(col("S", Col::State, n));
        v.push(if full { col("UID", Col::Field(USER), Cut::User(8)) } else { col("UID", Col::Field(UID), n) });
        v.extend([
            col("PID", Col::Field(PID), n),
            col("PPID", Col::Field(PPID), n),
            col("C", Col::Field(C), n),
            col("PRI", Col::Prio(60, 1), n),
            col("NI", Col::Field(NI), n),
        ]);
        v.push(if y { col("RSS", Col::Field(RSS), n) } else { col("ADDR", Col::Dash, n) });
        v.push(col("SZ", Col::Field(SZ), n));
        v.push(col("WCHAN", Col::Dash, Cut::Text(6)));
        if full {
            v.push(col("STIME", Col::Field(STIME), n));
        }
        v.push(col("TTY", Col::Field(TTY), n));
        v.push(col("TIME", if bsd { Col::BsdTime } else { Col::Time }, n));
        v.push(col("CMD", Col::Field(if full || bsd { CMD } else { COMM }), n));
        v
    } else if full {
        let mut v = vec![
            col("UID", Col::Field(USER), Cut::User(8)),
            col("PID", Col::Field(PID), n),
            col("PPID", Col::Field(PPID), n),
            col("C", Col::Field(C), n),
            col("STIME", Col::Field(STIME), n),
            col("TTY", Col::Field(TTY), n),
            col("TIME", Col::Field(TIME), n),
            col("CMD", Col::Field(CMD), n),
        ];
        // `-f` with a BSD option: STAT, and BSD's time.
        if bsd {
            v[6] = col("TIME", Col::BsdTime, n);
            v.insert(6, col("STAT", Col::Field(STAT), n));
        }
        v
    } else if bsd {
        vec![col("PID", Col::Field(PID), n), col("TTY", Col::Field(TTY), n), col("STAT", Col::Field(STAT), n), col("TIME", Col::BsdTime, n), col("COMMAND", Col::Field(CMD), n)]
    } else {
        vec![col("PID", Col::Field(PID), n), col("TTY", Col::Field(TTY), n), col("TIME", Col::Time, n), col("CMD", Col::Field(COMM), n)]
    };
    Ok(Format { cols, sort })
}

/// A guest line as its fields. An older agent's line (fewer fields, the
/// command last) leaves the newer ones as `-`.
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
        Col::Start => f[STIME].to_owned(),
        Col::Prio(a, b) => f[PRIO].parse::<i64>().map_or_else(|_| "-".to_owned(), |p| (a + b * p).to_string()),
        Col::Dash => "-".to_owned(),
    }
}

/// How ps cuts a value in a column that is not the last.
fn cut(v: String, cut: Cut) -> String {
    match cut {
        Cut::Text(w) if v.len() > w && v.chars().count() > w => v.chars().take(w).collect(),
        Cut::User(w) if v.len() > w && v.chars().count() > w => {
            v.chars().take(w.saturating_sub(1)).chain(std::iter::once('+')).collect()
        }
        _ => v,
    }
}

/// How rows compare on a sort column: numbers as numbers, the rest as text.
fn compare(a: &[&str], b: &[&str], col: Col) -> Ordering {
    let num = |f: &[&str], i: usize| f[i].parse::<f64>().unwrap_or(0.0);
    let by = |i: usize| num(a, i).total_cmp(&num(b, i));
    match col {
        Col::Field(i @ (PID | PPID | UID | C | RSS | VSZ | SZ | PCPU | PMEM | ETIME | CPU_SECS | F | NI)) => by(i),
        Col::Field(i) => a[i].cmp(b[i]),
        Col::State => a[STAT].chars().next().cmp(&b[STAT].chars().next()),
        Col::Time | Col::BsdTime => by(CPU_SECS),
        Col::Etime => by(ETIME),
        // Started earlier: running longer.
        Col::Start => by(ETIME).reverse(),
        Col::Prio(_, b_) if b_ < 0 => by(PRIO).reverse(),
        Col::Prio(..) => by(PRIO),
        Col::Dash => Ordering::Equal,
    }
}

/// `fields` the way Go's `strings.FieldsFunc` splits ps's output.
fn split_ascii(s: &str) -> impl Iterator<Item = &str> {
    s.split(['\t', '\n', '\x0c', '\r', ' ']).filter(|w| !w.is_empty())
}

/// Docker's answer for `GET /containers/{id}/top`.
pub fn answer(ps_args: &str, lines: &[String]) -> Result<Value, Error> {
    validate(ps_args)?;
    let fmt = format(ps_args)?;
    // ps's header line, as Docker splits it; with every title empty ps
    // prints none, and Docker finds no PID title.
    let titles: Vec<&str> = fmt.cols.iter().flat_map(|c| split_ascii(&c.title)).collect();
    let Some(pid_at) = titles.iter().position(|t| *t == "PID") else {
        return Err(Error::Io(io::Error::other("Couldn't find PID field in ps output")));
    };
    let mut rows: Vec<Vec<&str>> = lines.iter().map(|l| fields(l)).collect();
    if !fmt.sort.is_empty() {
        rows.sort_by(|a, b| fmt.sort.iter().map(|&(c, desc)| if desc { compare(a, b, c).reverse() } else { compare(a, b, c) }).find(|o| o.is_ne()).unwrap_or(Ordering::Equal));
    }
    let last = fmt.cols.len() - 1;
    let mut procs: Vec<Vec<String>> = Vec::with_capacity(rows.len());
    for f in &rows {
        // A row as ps prints it, split as Docker splits it. The last column
        // is kept whole (ps never cuts it; Docker joins its words again).
        let mut words: Vec<String> = Vec::with_capacity(fmt.cols.len());
        for (i, c) in fmt.cols.iter().enumerate() {
            let v = value(f, c.col);
            if i == last {
                words.push(v);
            } else {
                let v = cut(v, c.cut);
                if v.is_empty() || v.bytes().any(|b| matches!(b, b'\t' | b'\n' | 0x0c | b'\r' | b' ')) {
                    words.extend(split_ascii(&v).map(str::to_owned));
                } else {
                    words.push(v);
                }
            }
        }
        let Some(pid) = words.get(pid_at) else {
            return Err(Error::Io(io::Error::other("ps output has fewer fields than titles")));
        };
        if pid != "-" && pid.parse::<i64>().is_err() {
            return Err(Error::Io(io::Error::other(format!("Unexpected pid '{pid}': strconv.Atoi: parsing \"{pid}\": invalid syntax"))));
        }
        // Words past the titles join the last column.
        let n = titles.len();
        if words.len() + 1 < n {
            return Err(Error::Io(io::Error::other("ps output has fewer fields than titles")));
        }
        if words.len() != n {
            let tail = words.split_off((n - 1).min(words.len())).join(" ");
            words.push(tail);
        }
        procs.push(words);
    }
    Ok(json!({"Titles": titles, "Processes": procs}))
}

/// Checks the arguments before the guest is asked.
pub fn check(ps_args: &str) -> Result<(), Error> {
    answer(ps_args, &[]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    // user pid ppid c stime tty time comm stat rss vsz %cpu %mem etime cpu-secs uid f prio ni sz cmd
    const LINE: &str = "root\t7\t1\t0\t05:43\t?\t00:01:05\tcat\tSs\t1024\t2048\t0.5\t0.1\t3725\t65\t0\t4\t20\t0\t512\tcat -n";
    const LINE2: &str = "1000\t9\t7\t2\t05:44\t?\t00:00:01\tsleep\tS\t512\t4096\t1.5\t0.0\t60\t1\t1000\t0\t20\t5\t1024\tsleep 1d";
    const LINE3: &str = "root\t8\t7\t0\t05:43\t?\t00:00:00\tnginx\tS\t2048\t1024\t0.0\t0.2\t3700\t0\t0\t1\t20\t-\t256\tnginx: worker process";

    fn ok(args: &str) -> Value {
        answer(args, &[LINE.to_owned()]).unwrap()
    }

    fn three(args: &str) -> Value {
        answer(args, &[LINE.to_owned(), LINE2.to_owned(), LINE3.to_owned()]).unwrap()
    }

    fn col_of(v: &Value, i: usize) -> Vec<String> {
        v["Processes"].as_array().unwrap().iter().map(|r| r[i].as_str().unwrap().to_owned()).collect()
    }

    fn err(args: &str) -> String {
        match answer(args, &[LINE.to_owned()]) {
            Err(Error::BadRequest(m)) => format!("400 {m}"),
            Err(Error::Io(e)) => format!("500 {e}"),
            other => panic!("{args}: {other:?}"),
        }
    }

    #[test]
    fn the_default_is_ps_ef() {
        let want = json!({"Titles": ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"], "Processes": [["root", "7", "1", "0", "05:43", "?", "00:01:05", "cat -n"]]});
        assert_eq!(ok(""), want);
        assert_eq!(ok("-ef"), want);
        assert_eq!(ok("-e -f"), want);
        // The command line is kept as the guest sent it.
        let spaced = "root\t7\t1\t0\t05:43\t?\t00:01:05\tsh\tS\t1\t1\t0\t0\t1\t1\t0\t0\t20\t0\t1\tsh -c  'a  b'".to_owned();
        assert_eq!(answer("", &[spaced]).unwrap()["Processes"][0][7], "sh -c  'a  b'");
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
    fn f_with_a_bsd_option_adds_stat() {
        // procps: `ps -ef x`, `ps -ef w`.
        let want = json!({"Titles": ["UID", "PID", "PPID", "C", "STIME", "TTY", "STAT", "TIME", "CMD"], "Processes": [["root", "7", "1", "0", "05:43", "?", "Ss", "1:05", "cat -n"]]});
        assert_eq!(ok("-ef x"), want);
        assert_eq!(ok("-ef w"), want);
        assert_eq!(ok("-e w")["Titles"], json!(["PID", "TTY", "STAT", "TIME", "COMMAND"]));
    }

    #[test]
    fn o_picks_the_columns_named() {
        assert_eq!(ok("-eo pid,rss,args"), json!({"Titles": ["PID", "RSS", "COMMAND"], "Processes": [["7", "1024", "cat -n"]]}));
        assert_eq!(ok("-o pid,s,etime,comm"), json!({"Titles": ["PID", "S", "ELAPSED", "COMMAND"], "Processes": [["7", "S", "01:02:05", "cat"]]}));
        assert_eq!(ok("o pid=PID -o vsz,%mem")["Titles"], json!(["PID", "VSZ", "%MEM"]), "a title given with =, and -o twice");
        // procps 4: `ps -o pid=X,comm=Z,ppid` → `X Z PPID`.
        assert_eq!(ok("-o pid=PID,vsz=Size,ppid")["Titles"], json!(["PID", "Size", "PPID"]), "a title ends at a comma");
        assert_eq!(ok("-o comm=Name -o pid,vsz")["Titles"], json!(["Name", "PID", "VSZ"]));
        assert_eq!(ok("-o pid,"), json!({"Titles": ["PID"], "Processes": [["7"]]}));
    }

    #[test]
    fn long_format() {
        // procps: `ps -l`, F S UID PID PPID C PRI NI ADDR SZ WCHAN TTY TIME CMD.
        let want = json!({
            "Titles": ["F", "S", "UID", "PID", "PPID", "C", "PRI", "NI", "ADDR", "SZ", "WCHAN", "TTY", "TIME", "CMD"],
            "Processes": [["4", "S", "0", "7", "1", "0", "80", "0", "-", "512", "-", "?", "00:01:05", "cat"]],
        });
        assert_eq!(ok("-l"), want);
        assert_eq!(ok("-el"), want);
        assert_eq!(ok("-e -l"), want);
        assert_eq!(
            ok("-efl"),
            json!({
                "Titles": ["F", "S", "UID", "PID", "PPID", "C", "PRI", "NI", "ADDR", "SZ", "WCHAN", "STIME", "TTY", "TIME", "CMD"],
                "Processes": [["4", "S", "root", "7", "1", "0", "80", "0", "-", "512", "-", "05:43", "?", "00:01:05", "cat -n"]],
            })
        );
        assert_eq!(ok("-lf")["Titles"], ok("-efl")["Titles"]);
        assert_eq!(
            ok("-ely"),
            json!({
                "Titles": ["S", "UID", "PID", "PPID", "C", "PRI", "NI", "RSS", "SZ", "WCHAN", "TTY", "TIME", "CMD"],
                "Processes": [["S", "0", "7", "1", "0", "80", "0", "1024", "512", "-", "?", "00:01:05", "cat"]],
            })
        );
        // A BSD option: BSD's time and the command line.
        assert_eq!(ok("-l x")["Processes"][0].as_array().unwrap()[12..], [json!("1:05"), json!("cat -n")]);
        // BSD `l`.
        assert_eq!(
            ok("l"),
            json!({
                "Titles": ["F", "UID", "PID", "PPID", "PRI", "NI", "VSZ", "RSS", "WCHAN", "STAT", "TTY", "TIME", "COMMAND"],
                "Processes": [["4", "0", "7", "1", "20", "0", "2048", "1024", "-", "Ss", "?", "1:05", "cat -n"]],
            })
        );
        assert_eq!(ok("lax -y")["Titles"][0], "UID");
        // A real-time process has no nice value.
        assert_eq!(col_of(&three("-l --sort pid"), 7), ["0", "-", "5"]);
    }

    #[test]
    fn o_titles_are_procps_headers() {
        let v = ok("-o pid,euid,euser,ruser,uname,tname,tt,ucmd,rsz,start,start_time,bsdstart");
        assert_eq!(v["Titles"], json!(["PID", "EUID", "EUSER", "RUSER", "USER", "TTY", "TT", "CMD", "RSZ", "STARTED", "START", "START"]));
        // `tgid` is titled TGID, so Docker finds no PID column.
        assert_eq!(err("-o tgid,comm"), "500 Couldn't find PID field in ps output");
    }

    #[test]
    fn o_has_the_long_columns() {
        assert_eq!(ok("-o pid,f,pri,opri,priority,ni,sz,addr,wchan,nwchan"), json!({"Titles": ["PID", "F", "PRI", "PRI", "PRI", "NI", "SZ", "ADDR", "WCHAN", "WCHAN"], "Processes": [["7", "4", "19", "80", "20", "0", "512", "-", "-", "-"]]}));
    }

    #[test]
    fn widths_cut_text_columns_but_not_the_last() {
        // procps: `ps -o pid:2,comm:3,user:2,args:4` → `1780 sle l+ sleep 300`.
        let long_user = "libracoder\t7\t1\t0\t05:43\t?\t00:01:05\tsleep\tS\t1\t1\t0\t0\t1\t1\t501\t0\t20\t0\t1\tsleep 300".to_owned();
        let v = answer("-o pid:2,comm:3,user:2,args:4", std::slice::from_ref(&long_user)).unwrap();
        assert_eq!(v, json!({"Titles": ["PID", "COMMAND", "USER", "COMMAND"], "Processes": [["7", "sle", "l+", "sleep 300"]]}));
        // A user with no name is its number, cut the same way (procps: `1+`).
        assert_eq!(three("-o user:2,pid --sort pid")["Processes"][2][0], "1+");
        assert_eq!(ok("-o pid:8,comm:20"),json!({"Titles": ["PID", "COMMAND"], "Processes": [["7", "cat"]]}));
        // The default widths: user 8, comm 15, args 27.
        assert_eq!(answer("-o user,pid", std::slice::from_ref(&long_user)).unwrap()["Processes"], json!([["libraco+", "7"]]));
        assert_eq!(answer("-ef", &[long_user]).unwrap()["Processes"][0][0], "libraco+");
        // A width and a title.
        assert_eq!(err("-o pid:8=PID"), "500 specifying \"pid:8=PID\" is not allowed", "moby: the name is `pid:8`");
        assert_eq!(err("-o pid:8=Id"), "500 Couldn't find PID field in ps output");
        assert_eq!(ok("-o comm:8=Name -o pid")["Titles"], json!(["Name", "PID"]));
        for bad in ["-o pid:0", "-o pid:08", "-o pid:+8", "-o pid:x", "-o pid:8:9", "-o pid:"] {
            assert_eq!(err(bad), "500 ps: error: column widths must be unsigned decimal numbers", "{bad}");
        }
    }

    #[test]
    fn values_split_as_docker_reads_ps() {
        // A command line that is not last is cut to 27 and split at spaces,
        // so Docker reads a word where the PID is.
        assert_eq!(err("-o args,pid"), "500 Unexpected pid '-n': strconv.Atoi: parsing \"-n\": invalid syntax");
        // An empty title leaves the value to join the last column.
        assert_eq!(ok("-o pid,comm="), json!({"Titles": ["PID"], "Processes": [["7 cat"]]}));
        assert_eq!(ok("-o comm,pid"), json!({"Titles": ["COMMAND", "PID"], "Processes": [["cat", "7"]]}));
    }

    #[test]
    fn sort_orders_the_rows() {
        assert_eq!(col_of(&three("-o pid,comm --sort -pid"), 0), ["9", "8", "7"]);
        assert_eq!(col_of(&three("-eo pid --sort=pid"), 0), ["7", "8", "9"]);
        assert_eq!(col_of(&three("-ef --sort +rss"), 1), ["9", "7", "8"]);
        assert_eq!(col_of(&three("-ef --sort -%cpu"), 1), ["9", "7", "8"]);
        assert_eq!(col_of(&three("-ef --sort comm"), 1), ["7", "8", "9"]);
        assert_eq!(col_of(&three("-ef --sort ppid,-pid"), 1), ["7", "9", "8"]);
        assert_eq!(col_of(&three("-ef --sort start_time"), 1), ["7", "8", "9"], "oldest first");
        assert_eq!(col_of(&three("-ef --sort -vsz"), 1), ["9", "7", "8"]);
        assert_eq!(col_of(&three("-o pid k -pid"), 0), ["9", "8", "7"]);
        assert_eq!(col_of(&three("ax k-pid"), 0), ["9", "8", "7"]);
        assert_eq!(col_of(&three("aux --sort -rss"), 1), ["8", "7", "9"]);
        // `k` is a BSD option: `-ef k` is `-ef x`'s format; `--sort` is not.
        assert_eq!(three("-ef k pid")["Titles"], ok("-ef x")["Titles"]);
        assert_eq!(three("-ef --sort pid")["Titles"], ok("-ef")["Titles"]);
        // Without a sort the guest's order stays.
        assert_eq!(col_of(&three("-ef"), 1), ["7", "9", "8"]);
    }

    #[test]
    fn a_pid_title_on_another_column_is_refused_as_moby_does() {
        assert_eq!(err("-o ppid=PID"), "500 specifying \"ppid=PID\" is not allowed");
        assert_eq!(err("-eo comm=PIDX,pid"), "500 specifying \"comm=PIDX,pid\" is not allowed");
        assert_eq!(err("-o pid,ppid=PID"), "500 specifying \"pid,ppid=PID\" is not allowed", "the whole word before =");
        assert_eq!(err("-o ppid= PID"), "500 specifying \"ppid=PID\" is not allowed");
        // `pid` may be titled PID; any other title leaves no PID column.
        assert_eq!(ok("-o pid=PID,comm")["Titles"], json!(["PID", "COMMAND"]));
        assert_eq!(ok("-o pid=PID -o comm")["Titles"], json!(["PID", "COMMAND"]));
        assert_eq!(err("-o pid=ID"), "500 Couldn't find PID field in ps output");
        assert_eq!(err("-o pid=PIDS"), "500 Couldn't find PID field in ps output");
        assert_eq!(err("-o pid= -o comm="), "500 Couldn't find PID field in ps output");
        // Titles not starting with PID are anyone's.
        assert_eq!(ok("-o pid,ppid=Parent")["Titles"], json!(["PID", "Parent"]));
        // moby needs whitespace before the name.
        assert!(validate("-oppid=PID").is_ok());
    }

    #[test]
    fn what_ps_refuses_is_refused_with_its_words() {
        for (args, want) in [
            ("-l -o pid", "conflicting format options"),
            ("aux -l", "conflicting format options"),
            ("-ef u", "conflicting format options"),
            ("-f l", "conflicting format options"),
            ("-o pid u", "conflicting format options"),
            ("-y", "modifier -y without format -l makes no sense"),
            ("-o pid -y", "can not use output modifiers with user-defined output"),
            ("-o ,pid", "improper format list"),
            ("-o pid,,comm", "improper format list"),
            ("-ef --sort", "long sort specification must follow --sort"),
            ("-ef --sort=", "long sort specification must follow --sort"),
            ("ax k", "long sort specification must follow 'k'"),
            ("-ef --sort ,pid", "improper sort list"),
        ] {
            assert_eq!(err(args), format!("500 ps: error: {want}"), "{args}");
        }
    }

    #[test]
    fn what_kiri_cannot_answer_is_refused() {
        for args in ["--forest", "-eo pid,nosuch", "-o", "-F", "-j", "j", "-ef --sort nosuch", "-ef -k pid", "-ef --sort pid:3"] {
            assert!(err(args).starts_with("400 ps_args: "), "{args}: {}", err(args));
        }
    }

    #[test]
    fn an_older_agents_line_still_gives_ps_ef() {
        let old = "root\t7\t1\t0\t05:43\t?\t00:00:00\tsh -c x".to_owned();
        assert_eq!(answer("-ef", std::slice::from_ref(&old)).unwrap()["Processes"], json!([["root", "7", "1", "0", "05:43", "?", "00:00:00", "sh -c x"]]));
        assert_eq!(answer("-eo pid,rss", std::slice::from_ref(&old)).unwrap()["Processes"], json!([["7", "-"]]));
        // An agent from before the long columns.
        let mid = "root\t7\t1\t0\t05:43\t?\t00:01:05\tcat\tSs\t1024\t2048\t0.5\t0.1\t3725\t65\t0\tcat -n".to_owned();
        assert_eq!(answer("-l", &[mid]).unwrap()["Processes"][0], json!(["-", "S", "0", "7", "1", "0", "-", "-", "-", "-", "-", "?", "00:01:05", "cat"]));
    }
}
