//! What an image won't start without, read from its own start script before
//! Run, so Run can ask first: the Mac's window and Windows' (crates/kiri-win,
//! by #[path]) answer `needs` with it. The image's entrypoint is copied out
//! of a container that is created and never started, then removed. Only
//! checks that stop the script count: `if [ -z "$VAR" ]` (or `test -z`, or
//! `[ -z … ] && …`) whose block exits or calls a function that does, and
//! `${VAR:?message}`. A check that only applies when another variable is
//! set, or a variable the image sets itself, doesn't.

use std::collections::HashSet;
use std::io;

use serde_json::{Value, json};

use crate::client;

/// One variable the script stops without, or one of `alts` instead, and
/// what the script says when it does.
#[derive(Debug, PartialEq)]
pub struct Need {
    pub name: String,
    pub alts: Vec<String>,
    /// Another way past the check, as `VAR=value` (Postgres's
    /// POSTGRES_HOST_AUTH_METHOD=trust).
    pub instead: Vec<String>,
    pub why: String,
}

/// `{type: "needs", image, id, vars: [{name, alts, instead, why}]}`; no vars
/// when the image needs nothing that Kiri can read, or can't be read.
pub fn event(image: &str) -> String {
    let id = client::get(&format!("/images/{}/json", esc(image)))
        .ok()
        .and_then(|v| v["Id"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let vars: Vec<Value> = probe(image)
        .unwrap_or_default()
        .into_iter()
        .map(|n| json!({"name": n.name, "alts": n.alts, "instead": n.instead, "why": n.why}))
        .collect();
    json!({"type": "needs", "image": image, "id": id, "vars": vars}).to_string()
}

fn probe(image: &str) -> io::Result<Vec<Need>> {
    let info = client::get(&format!("/images/{}/json", esc(image)))?;
    let cfg = &info["Config"];
    let strs = |v: &Value| {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let env = strs(&cfg["Env"]);
    let Some(start) = script_of(&strs(&cfg["Entrypoint"]), &strs(&cfg["Cmd"])) else {
        return Ok(Vec::new());
    };
    let path = match start {
        Start::Inline(text) => return Ok(scan(&text, &env)),
        Start::Path(p) => p,
    };
    let created = client::request(
        "POST",
        "/containers/create",
        Some(
            &json!({"Image": image, "Entrypoint": ["true"], "Cmd": [], "Labels": {"io.kiri.probe": "1"}, "NetworkDisabled": true}),
        ),
    )?;
    if !created.ok() {
        return Err(io::Error::other(created.error()));
    }
    let id = created.json()["Id"].as_str().unwrap_or_default().to_owned();
    let text = read_script(
        &id,
        &path,
        &env,
        cfg["WorkingDir"].as_str().unwrap_or_default(),
    );
    let _ = client::request("DELETE", &format!("/containers/{id}?force=1&v=1"), None);
    Ok(text.map(|t| scan(&t, &env)).unwrap_or_default())
}

enum Start {
    Path(String),
    Inline(String),
}

/// The script the container starts with: past init wrappers (tini,
/// dumb-init, gosu) and a shell running it (`sh -c '…'` is the script).
fn script_of(entrypoint: &[String], cmd: &[String]) -> Option<Start> {
    let argv: Vec<&str> = entrypoint.iter().chain(cmd).map(String::as_str).collect();
    let base = |s: &str| s.rsplit('/').next().unwrap_or(s).to_owned();
    let mut i = 0;
    while i < argv.len()
        && [
            "tini",
            "tini-static",
            "dumb-init",
            "gosu",
            "su-exec",
            "catatonit",
        ]
        .contains(&base(argv[i]).as_str())
    {
        i += 1;
        while i < argv.len() && argv[i].starts_with('-') {
            i += 1;
        }
    }
    if i < argv.len() && ["sh", "bash", "ash", "dash"].contains(&base(argv[i]).as_str()) {
        i += 1;
        while i < argv.len() && argv[i].starts_with('-') {
            if argv[i] == "-c" {
                return argv.get(i + 1).map(|s| Start::Inline((*s).to_owned()));
            }
            i += 1;
        }
    }
    argv.get(i).map(|s| Start::Path((*s).to_owned()))
}

/// The script's text, found as the shell would (PATH, or the working
/// directory), following symlinks; None for a binary or nothing there.
fn read_script(id: &str, path: &str, env: &[String], workdir: &str) -> Option<String> {
    let candidates: Vec<String> = if path.starts_with('/') {
        vec![path.to_owned()]
    } else if path.contains('/') {
        vec![format!(
            "{}/{path}",
            if workdir.is_empty() {
                ""
            } else {
                workdir.trim_end_matches('/')
            }
        )]
    } else {
        let dirs = env
            .iter()
            .find_map(|e| e.strip_prefix("PATH="))
            .unwrap_or("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        dirs.split(':')
            .filter(|d| !d.is_empty())
            .map(|d| format!("{}/{path}", d.trim_end_matches('/')))
            .collect()
    };
    for mut p in candidates {
        for _ in 0..4 {
            let Ok(r) = client::request(
                "GET",
                &format!("/containers/{id}/archive?path={}", esc(&p)),
                None,
            ) else {
                break;
            };
            if !r.ok() {
                break;
            }
            match first_entry(&r.body) {
                Some(Entry::Link(to)) => {
                    p = if to.starts_with('/') {
                        to
                    } else {
                        format!("{}/{to}", p.rsplit_once('/').map_or("", |(d, _)| d))
                    }
                }
                Some(Entry::File(data)) => {
                    let text = String::from_utf8_lossy(&data);
                    return text
                        .lines()
                        .next()
                        .is_some_and(|l| l.starts_with("#!") && l.contains("sh"))
                        .then(|| text.into_owned());
                }
                None => break,
            }
        }
    }
    None
}

enum Entry {
    File(Vec<u8>),
    Link(String),
}

/// The first file or symlink in a tar stream, past pax and GNU long-name headers.
fn first_entry(tar: &[u8]) -> Option<Entry> {
    let mut at = 0;
    while at + 512 <= tar.len() {
        let h = &tar[at..at + 512];
        if h.iter().all(|&b| b == 0) {
            return None;
        }
        let field = |a: usize, b: usize| {
            String::from_utf8_lossy(&h[a..b])
                .trim_end_matches('\0')
                .trim()
                .to_owned()
        };
        let size = usize::from_str_radix(&field(124, 136), 8).ok()?;
        let data = at + 512;
        match h[156] {
            b'0' | 0 => return Some(Entry::File(tar.get(data..data + size)?.to_vec())),
            b'1' | b'2' => return Some(Entry::Link(field(157, 257))),
            _ => at = data + size.div_ceil(512) * 512,
        }
    }
    None
}

/// For a URL path or query; `:` and `@` stay as they are, since Kiri's engine
/// matches an image name without decoding them.
fn esc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"/-._:@".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// One line of the script as the shell reads it: continuations joined, and
/// the heredoc it opens, if any, beside it.
struct Line {
    code: String,
    here: Vec<String>,
}

fn lines(script: &str) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut raw = script.lines();
    while let Some(first) = raw.next() {
        let mut code = first.to_owned();
        while code.ends_with('\\') {
            code.pop();
            match raw.next() {
                Some(next) => code.push_str(next),
                None => break,
            }
        }
        let mut here = Vec::new();
        if let Some(term) = heredoc_term(&code) {
            for l in raw.by_ref() {
                if l.trim() == term {
                    break;
                }
                here.push(l.trim().to_owned());
            }
        }
        if !code.trim_start().starts_with('#') {
            out.push(Line { code, here });
        }
    }
    out
}

/// `EOF` in `cat <<-'EOF'`; not here-strings (`<<<`).
fn heredoc_term(code: &str) -> Option<String> {
    let at = code.find("<<")?;
    let rest = &code[at + 2..];
    if rest.starts_with('<') {
        return None;
    }
    let rest = rest
        .trim_start_matches('-')
        .trim_start()
        .trim_start_matches(['\'', '"']);
    let term: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!term.is_empty()).then_some(term)
}

fn is_env_name(s: &str) -> bool {
    s.len() >= 2
        && s.starts_with(|c: char| c.is_ascii_uppercase())
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// The variable at the start of `s`, just past a `$`: `VAR`, `{VAR}` or `{VAR:-}`.
fn var_at(s: &str) -> String {
    let s = s.strip_prefix('{').unwrap_or(s);
    s.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

/// The statements of a line, split at `;`, `&&`, `||` and braces.
fn statements(code: &str) -> Vec<String> {
    code.replace("&&", ";")
        .replace("||", ";")
        .split([';', '{', '}'])
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

fn first_word(stmt: &str) -> &str {
    stmt.split_whitespace().next().unwrap_or_default()
}

fn exits(stmt: &str) -> bool {
    let mut w = stmt.split_whitespace();
    w.next() == Some("exit")
        && w.next().is_some_and(|n| {
            n.trim_matches(['"', '\''])
                .parse::<i32>()
                .is_ok_and(|n| n != 0)
        })
}

/// Functions that end the script: they exit, or call one that does.
fn fatal_functions(ls: &[Line]) -> HashSet<String> {
    let mut bodies: Vec<(String, Vec<String>)> = Vec::new();
    let mut cur: Option<(String, Vec<String>)> = None;
    for l in ls {
        let t = l.code.trim();
        if let Some(name) = t
            .strip_suffix('{')
            .map(str::trim)
            .and_then(|s| s.strip_suffix("()"))
            .map(|s| s.trim().trim_start_matches("function ").trim())
        {
            cur = Some((name.to_owned(), Vec::new()));
        } else if l.code.starts_with('}') {
            bodies.extend(cur.take());
        } else if let Some((_, b)) = cur.as_mut() {
            b.extend(statements(t));
        }
    }
    let mut fatal = HashSet::new();
    for _ in 0..3 {
        for (name, body) in &bodies {
            if body
                .iter()
                .any(|s| exits(s) || fatal.contains(first_word(s)))
            {
                fatal.insert(name.clone());
            }
        }
    }
    fatal
}

/// Variables the script declares as its own (`declare -g VAR`, `local VAR`)
/// or defaults (`: "${VAR:=x}"`). A plain `VAR=` doesn't count: MySQL's sets
/// MYSQL_ROOT_PASSWORD itself only after checking it.
fn assigned(ls: &[Line]) -> HashSet<String> {
    let mut set = HashSet::new();
    for l in ls {
        for s in statements(&l.code) {
            let mut w = s.split_whitespace();
            if w.next()
                .is_some_and(|f| ["local", "declare", "typeset", "readonly"].contains(&f))
            {
                set.extend(
                    w.filter(|f| !f.starts_with('-'))
                        .map(|f| f.split('=').next().unwrap_or_default().to_owned()),
                );
            }
        }
        let mut rest = l.code.as_str();
        while let Some(at) = rest.find("${") {
            let name = var_at(&rest[at + 1..]);
            if rest[at + 2 + name.len()..].starts_with(":=") {
                set.insert(name);
            }
            rest = &rest[at + 2..];
        }
    }
    set
}

/// The `-z` variables of a condition that fails only when they're empty, and
/// whether it fails when any one is (`||`) rather than all (`&&`, `-a`); None
/// when it also depends on something being set or equal to something.
fn zero_tests(cond: &str) -> Option<(Vec<String>, bool, Vec<String>)> {
    let toks: Vec<&str> = cond.split_whitespace().collect();
    let (mut vars, mut instead) = (Vec::new(), Vec::new());
    let unq = |t: &str| t.trim_matches(['"', '\'']).to_owned();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if t == "-z" {
            if i > 0 && toks[i - 1] == "!" {
                return None;
            }
            let v = toks.get(i + 1).map(|v| unq(v)).unwrap_or_default();
            let name = v.strip_prefix('$').map(var_at)?;
            vars.push(name);
            i += 2;
            continue;
        }
        if t == "-n" || t == "=" || t == "==" {
            return None;
        }
        if t == "!=" {
            // `[ 'trust' != "$VAR" ]`: setting it is another way past.
            let (a, b) = (
                toks.get(i.wrapping_sub(1))
                    .map(|v| unq(v))
                    .unwrap_or_default(),
                toks.get(i + 1).map(|v| unq(v)).unwrap_or_default(),
            );
            let (var, lit) = if a.starts_with('$') { (a, b) } else { (b, a) };
            let name = var
                .strip_prefix('$')
                .map(var_at)
                .filter(|n| is_env_name(n))?;
            if lit.starts_with('$') || lit.is_empty() {
                return None;
            }
            instead.push(format!("{name}={lit}"));
            i += 2;
            continue;
        }
        // `[ "$VAR" ]` alone: only when it's set.
        if t.trim_matches('"').starts_with('$')
            && toks.get(i + 1).is_some_and(|n| n.starts_with(']'))
        {
            return None;
        }
        i += 1;
    }
    let any = cond.contains("||") || toks.contains(&"-o");
    (!vars.is_empty()).then_some((vars, any, instead))
}

/// What the script says when it stops: the heredoc it prints, or the first
/// quoted text in the block.
fn message(block: &[&Line]) -> String {
    let text = block
        .iter()
        .flat_map(|l| l.here.iter())
        .find(|l| !l.trim().is_empty())
        .cloned()
        .or_else(|| {
            block.iter().find_map(|l| {
                let c = &l.code;
                let q = c.find(['"', '\''])?;
                let quote = c[q..].chars().next()?;
                let end = c[q + 1..].find(quote)?;
                Some(c[q + 1..q + 1 + end].to_owned())
                    .filter(|s| s.chars().any(char::is_alphabetic) && !s.starts_with('$'))
            })
        });
    let t = text.unwrap_or_default();
    let t = t
        .trim()
        .trim_start_matches("Error:")
        .trim_start_matches("error:")
        .trim_start_matches("ERROR:")
        .trim();
    t.chars().take(240).collect()
}

/// Every variable `script` stops without. `image_env` is the image's own
/// `ENV`: a variable it gives a value isn't missing.
pub fn scan(script: &str, image_env: &[String]) -> Vec<Need> {
    let ls = lines(script);
    let fatal = fatal_functions(&ls);
    let own = assigned(&ls);
    let has_default = |n: &str| {
        image_env.iter().any(|e| {
            e.split_once('=')
                .is_some_and(|(k, v)| k == n && !v.is_empty())
        })
    };
    let ends = |stmts: &[String]| {
        stmts
            .iter()
            .any(|s| exits(s) || fatal.contains(first_word(s)))
    };
    let mut found: Vec<Need> = Vec::new();
    let add =
        |vars: Vec<String>, any: bool, instead: Vec<String>, why: String, found: &mut Vec<Need>| {
            let vars: Vec<String> = vars
                .into_iter()
                .filter(|v| is_env_name(v) && !own.contains(v))
                .collect();
            if vars.is_empty() || vars.iter().any(|v| has_default(v)) && !any {
                return;
            }
            let groups: Vec<Vec<String>> = if any {
                vars.into_iter()
                    .filter(|v| !has_default(v))
                    .map(|v| vec![v])
                    .collect()
            } else {
                vec![vars]
            };
            for g in groups {
                if found.iter().any(|n| n.name == g[0]) {
                    continue;
                }
                found.push(Need {
                    name: g[0].clone(),
                    alts: g[1..].to_vec(),
                    instead: instead.clone(),
                    why: why.clone(),
                });
            }
        };
    let mut i = 0;
    while i < ls.len() {
        let t = ls[i].code.trim();
        // if [ -z "$VAR" ]; then … exit 1 … fi
        if let Some(rest) = t.strip_prefix("if ") {
            let mut cond = rest.to_owned();
            let mut j = i;
            while !cond.contains("then") && j + 1 < ls.len() {
                j += 1;
                cond.push(' ');
                cond.push_str(ls[j].code.trim());
            }
            let (cond, inline) = cond.split_once("then").unwrap_or((&cond, ""));
            let cond = cond.trim().trim_end_matches(';').to_owned();
            // if …; then exit 1; fi, on one line.
            if let Some(body) = inline
                .trim()
                .strip_suffix("fi")
                .filter(|_| inline.trim() != "")
            {
                if statements(body)
                    .iter()
                    .any(|s| exits(s) || fatal.contains(first_word(s)))
                    && let Some((vars, any, instead)) = zero_tests(&cond)
                {
                    add(
                        vars,
                        any,
                        instead,
                        message(&[&Line {
                            code: body.to_owned(),
                            here: Vec::new(),
                        }]),
                        &mut found,
                    );
                }
                i = j + 1;
                continue;
            }
            let (mut depth, mut k, mut block) = (0, j + 1, Vec::new());
            while k < ls.len() {
                let w = first_word(ls[k].code.trim());
                if w == "if"
                    && !ls[k].code.contains("; fi")
                    && !ls[k].code.trim_end().ends_with(" fi")
                {
                    depth += 1;
                } else if w == "fi" || (depth == 0 && (w == "else" || w == "elif")) {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                } else if depth == 0 {
                    block.push(&ls[k]);
                }
                k += 1;
            }
            let stmts: Vec<String> = block.iter().flat_map(|l| statements(&l.code)).collect();
            if ends(&stmts)
                && let Some((vars, any, instead)) = zero_tests(&cond)
            {
                add(vars, any, instead, message(&block), &mut found);
            }
        }
        // [ -z "$VAR" ] && { echo …; exit 1; }
        else if (t.starts_with('[') || t.starts_with("test ")) && t.contains("&&") {
            let (cond, rest) = t.split_once("&&").unwrap_or_default();
            let mut block = vec![&ls[i]];
            if rest.trim() == "{" {
                let mut k = i + 1;
                while k < ls.len() && ls[k].code.trim() != "}" {
                    block.push(&ls[k]);
                    k += 1;
                }
            }
            let stmts: Vec<String> = statements(rest)
                .into_iter()
                .chain(block[1..].iter().flat_map(|l| statements(&l.code)))
                .collect();
            if ends(&stmts)
                && let Some((vars, any, instead)) = zero_tests(cond)
            {
                add(vars, any, instead, message(&block), &mut found);
            }
        }
        // : "${VAR:?message}"
        let mut rest = ls[i].code.as_str();
        while let Some(at) = rest.find("${") {
            let name = var_at(&rest[at + 1..]);
            let after = &rest[at + 2 + name.len()..];
            if let Some(msg) = after.strip_prefix(":?").or_else(|| after.strip_prefix('?')) {
                let why = msg
                    .split('}')
                    .next()
                    .unwrap_or_default()
                    .trim_matches(['"', '\''])
                    .to_owned();
                add(vec![name], false, Vec::new(), why, &mut found);
            }
            rest = &rest[at + 2..];
        }
        i += 1;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[Need]) -> Vec<&str> {
        n.iter().map(|n| n.name.as_str()).collect()
    }

    #[test]
    fn finds_a_password_the_script_exits_without() {
        let s = "#!/bin/sh\nset -e\nif [ -z \"$APP_PASSWORD\" ]; then\n\tcat >&2 <<-'EOE'\n\t\tError: APP_PASSWORD is not specified.\n\t\tYou must specify it.\n\tEOE\n\texit 1\nfi\necho ok\n";
        let n = scan(s, &[]);
        assert_eq!(names(&n), ["APP_PASSWORD"]);
        assert_eq!(n[0].why, "APP_PASSWORD is not specified.");
        assert!(
            scan(s, &["APP_PASSWORD=x".into()]).is_empty(),
            "the image sets it"
        );
    }

    #[test]
    fn reads_postgres_and_mysql() {
        let pg = "docker_verify_minimum_env() {\n\tif [ -z \"$POSTGRES_PASSWORD\" ] && [ 'trust' != \"$POSTGRES_HOST_AUTH_METHOD\" ]; then\n\t\tcat >&2 <<-'EOE'\n\t\t\tError: Database is uninitialized and superuser password is not specified.\n\t\tEOE\n\t\texit 1\n\tfi\n}\nif [ -z \"$DATABASE_ALREADY_EXISTS\" ]; then\n\texit 2\nfi\ndeclare -g DATABASE_ALREADY_EXISTS\n";
        let n = scan(pg, &[]);
        assert_eq!(names(&n), ["POSTGRES_PASSWORD"]);
        assert_eq!(n[0].instead, ["POSTGRES_HOST_AUTH_METHOD=trust"]);
        let my = "mysql_error() {\n\tmysql_log ERROR \"$@\" >&2\n\texit 1\n}\nf() {\n\tif [ -z \"$MYSQL_ROOT_PASSWORD\" -a -z \"$MYSQL_ALLOW_EMPTY_PASSWORD\" -a -z \"$MYSQL_RANDOM_ROOT_PASSWORD\" ]; then\n\t\tmysql_error <<-'EOF'\n\t\t\tDatabase is uninitialized and password option is not specified\n\t\tEOF\n\tfi\n\tif [ -n \"$MYSQL_USER\" ] && [ -z \"$MYSQL_PASSWORD\" ]; then\n\t\tmysql_error 'no'\n\tfi\n}\n";
        let n = scan(my, &[]);
        assert_eq!(names(&n), ["MYSQL_ROOT_PASSWORD"]);
        assert_eq!(
            n[0].alts,
            ["MYSQL_ALLOW_EMPTY_PASSWORD", "MYSQL_RANDOM_ROOT_PASSWORD"]
        );
    }

    #[test]
    fn other_forms_and_what_isnt_a_need() {
        assert_eq!(
            names(&scan(": \"${API_KEY:?set API_KEY}\"\n", &[])),
            ["API_KEY"]
        );
        assert_eq!(
            names(&scan(
                "[ -z \"$TOKEN\" ] && { echo 'TOKEN is required' >&2; exit 1; }\n",
                &[]
            )),
            ["TOKEN"]
        );
        assert_eq!(
            names(&scan(
                "if [ -z \"$A_KEY\" ] || [ -z \"$B_KEY\" ]; then exit 1; fi\n",
                &[]
            )),
            ["A_KEY", "B_KEY"]
        );
        assert!(
            scan("if [ -z \"$QUIET_LOGS\" ]; then\n\techo hi\nfi\n", &[]).is_empty(),
            "doesn't stop"
        );
        assert!(scan("if [ ! -z \"$GF_X\" ]; then exit 1; fi\n", &[]).is_empty());
        assert!(
            scan(
                "if [ \"$IS_SERVER\" ] && [ -z \"$SKIP_X\" ]; then exit 1; fi\n",
                &[]
            )
            .is_empty()
        );
        assert!(
            scan("if [ -z \"$dbPath\" ]; then exit 1; fi\n", &[]).is_empty(),
            "a script variable"
        );
        assert!(scan(": \"${PGDATA:=/var/lib}\"\n", &[]).is_empty());
    }

    #[test]
    fn finds_the_script_and_reads_tar() {
        assert!(
            matches!(script_of(&["tini".into(), "--".into(), "docker-entrypoint.sh".into()], &["redis-server".into()]), Some(Start::Path(p)) if p == "docker-entrypoint.sh")
        );
        assert!(
            matches!(script_of(&["/bin/sh".into(), "-c".into(), "exit 1".into()], &[]), Some(Start::Inline(s)) if s == "exit 1")
        );
        let mut tar = vec![0u8; 512];
        tar[..4].copy_from_slice(b"x.sh");
        tar[124..135].copy_from_slice(b"00000000003");
        tar[156] = b'0';
        tar.extend_from_slice(b"abc");
        tar.resize(2048, 0);
        assert!(matches!(first_entry(&tar), Some(Entry::File(d)) if d == b"abc"));
    }
}
