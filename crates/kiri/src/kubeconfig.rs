//! The cluster's entry in kubectl's own file, written by Kiri itself with
//! no kubectl: `kiri k8s` on the Mac and Linux (crates/kiri/src/k8s.rs) and
//! the window on Windows (crates/kiri-win/src/k8s.rs, which includes this
//! file) use it.
//!
//! The file is the first of `$KUBECONFIG`, else `.kube/config` in the home
//! folder (`%USERPROFILE%` on Windows), where kubectl looks. Only the
//! cluster, user and context named after the engine (`kiri`, `kiri-dev`,
//! …) are added, replaced or removed; every other line stays as it was, in
//! its place, comments too. `current-context` changes only when asked
//! (`kiri k8s start --use`), or a new file has ours. It is written to a
//! temporary file beside it and renamed over it, 0600.
//!
//! kubeconfigs are YAML as kubectl writes them, and as other tools (aws,
//! gcloud, k3d) write them too: top-level keys at the left edge and block
//! lists under `clusters`, `contexts` and `users`, flush or indented. That
//! is what is read here, line by line; a file in another shape (JSON, flow
//! lists) is left alone with an error, never rewritten.

use std::io;
use std::path::{Path, PathBuf};

/// The lists a context lives in.
const LISTS: [&str; 3] = ["clusters", "contexts", "users"];

/// kubectl's file: the first of `$KUBECONFIG`, else `.kube/config` at home.
pub fn path() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    pick(std::env::var_os("KUBECONFIG"), home.map(PathBuf::from))
}

fn pick(kubeconfig: Option<std::ffi::OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(first) = kubeconfig
        .as_deref()
        .and_then(|l| std::env::split_paths(l).find(|p| !p.as_os_str().is_empty()))
    {
        return Some(first);
    }
    home.filter(|h| !h.as_os_str().is_empty())
        .map(|h| h.join(".kube").join("config"))
}

/// k3s' own kubeconfig (`/etc/rancher/k3s/k3s.yaml`), its `default`
/// cluster, user and context named `name`, and its server the host's `port`.
pub fn from_k3s(raw: &str, name: &str, port: u16) -> String {
    raw.lines()
        .map(|l| {
            let t = l.trim_start();
            let indent = &l[..l.len() - t.len()];
            match t {
                "name: default" | "- name: default" => l.replace("default", name),
                "cluster: default" | "user: default" => {
                    format!("{indent}{}", t.replace("default", name))
                }
                "current-context: default" => format!("current-context: {name}"),
                // The node's own port, published on the host's `port`.
                t if t.starts_with("server: https://127.0.0.1:") => {
                    format!("{indent}server: https://127.0.0.1:{port}")
                }
                _ => l.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// `ours`' cluster, user and context called `name` put into the file at
/// `path` (made if missing). Whether the file changed.
pub fn install(path: &Path, ours: &str, name: &str, use_context: bool) -> io::Result<bool> {
    let path = real(path);
    let before = read(&path)?;
    let after = merge(before.as_deref().unwrap_or(""), ours, name, use_context)
        .map_err(|e| io::Error::other(format!("{}: {e}", path.display())))?;
    if before.as_deref() == Some(after.as_str()) {
        return Ok(false);
    }
    write(&path, before.as_deref(), &after)?;
    Ok(true)
}

/// The cluster, user and context called `name` taken out of the file at
/// `path`. Whether the file changed.
pub fn uninstall(path: &Path, name: &str) -> io::Result<bool> {
    let path = real(path);
    let Some(before) = read(&path)? else {
        return Ok(false);
    };
    let after =
        remove(&before, name).map_err(|e| io::Error::other(format!("{}: {e}", path.display())))?;
    if after == before {
        return Ok(false);
    }
    write(&path, Some(&before), &after)?;
    Ok(true)
}

/// A link (dotfiles kept elsewhere) is followed, so the rename replaces
/// its target rather than the link.
fn real(path: &Path) -> PathBuf {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
        }
        _ => path.to_owned(),
    }
}

fn read(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The new text by a temporary file and a rename, 0600; the one before it
/// kept as `config.bak.kiri`.
fn write(path: &Path, before: Option<&str>, text: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let file = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    if let Some(before) = before {
        let backup = dir.join(format!("{file}.bak.kiri"));
        std::fs::write(&backup, before)?;
        private(&backup)?;
    }
    let tmp = dir.join(format!(".{file}.kiri-{}", std::process::id()));
    let done = std::fs::write(&tmp, text)
        .and_then(|()| private(&tmp))
        .and_then(|()| std::fs::rename(&tmp, path));
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

#[cfg(unix)]
fn private(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
}

// %USERPROFILE% is the user's own already.
#[cfg(not(unix))]
fn private(_: &Path) -> io::Result<()> {
    Ok(())
}

/// One top-level key and the lines under it (the first is the key's own);
/// the lines before any key have the key "".
struct Top<'a> {
    key: &'a str,
    lines: Vec<&'a str>,
}

fn indent(l: &str) -> usize {
    l.len() - l.trim_start_matches(' ').len()
}

fn blank(l: &str) -> bool {
    let t = l.trim();
    t.is_empty() || t.starts_with('#')
}

fn tops(text: &str) -> Vec<Top<'_>> {
    let mut tops = vec![Top {
        key: "",
        lines: Vec::new(),
    }];
    for l in text.lines() {
        let top = !l.starts_with([' ', '\t', '-', '#']) && !l.trim().is_empty();
        match l.split_once(':').filter(|_| top) {
            Some((key, _)) => tops.push(Top {
                key: key.trim(),
                lines: vec![l],
            }),
            None => tops.last_mut().expect("there is one").lines.push(l),
        }
    }
    tops
}

/// The lines before a list's first item, its items (each its lines), and their indent.
type Items<'a> = (Vec<&'a str>, Vec<Vec<&'a str>>, usize);

/// A list's items; an error for a list that isn't a block list.
fn items<'a>(top: &Top<'a>) -> Result<Items<'a>, String> {
    let value = top.lines[0]
        .split_once(':')
        .map_or("", |(_, v)| v)
        .split(" #")
        .next()
        .unwrap_or("")
        .trim();
    if !matches!(value, "" | "[]" | "null" | "~") {
        return Err(format!(
            "`{}` is not a list kubectl writes; not changing it",
            top.key
        ));
    }
    let body = &top.lines[1..];
    let at = body
        .iter()
        .position(|l| !blank(l) && l.trim_start().starts_with('-'));
    let Some(at) = at else {
        return Ok((body.to_vec(), Vec::new(), 0));
    };
    let dash = indent(body[at]);
    let mut items: Vec<Vec<&str>> = Vec::new();
    for &l in &body[at..] {
        if indent(l) == dash && l.trim_start().starts_with('-') {
            items.push(vec![l]);
        } else {
            items.last_mut().expect("starts with one").push(l);
        }
    }
    Ok((body[..at].to_vec(), items, dash))
}

/// An item's `name`, the key at the column after its dash.
fn item_name<'a>(item: &[&'a str], dash: usize) -> Option<&'a str> {
    let rest = &item[0][dash + 1..];
    let col = dash + 1 + indent(rest);
    let first = rest.trim_start();
    let line = if first.starts_with("name:") {
        Some(first)
    } else {
        item[1..]
            .iter()
            .find(|l| indent(l) == col && l.trim_start().starts_with("name:"))
            .map(|l| l.trim_start())
    };
    line.map(|l| scalar(&l["name:".len()..]))
}

/// A plain or quoted YAML scalar, without a trailing comment.
fn scalar(v: &str) -> &str {
    let v = v.trim();
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q).and_then(|r| r.split(q).next()) {
            return inner;
        }
    }
    v.split(" #").next().unwrap_or("").trim()
}

fn not_json(text: &str) -> Result<(), String> {
    if text.trim_start().starts_with('{') {
        return Err("a JSON kubeconfig; not changing it".into());
    }
    Ok(())
}

/// The file's line ending: Windows editors may have left CRLF.
fn newline(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

fn join(lines: &[String], nl: &str) -> String {
    let mut out = lines.join(nl);
    out.push_str(nl);
    out
}

/// `existing` with `ours`' cluster, user and context called `name` in
/// place of any it had; the rest of it as it was.
pub fn merge(existing: &str, ours: &str, name: &str, use_context: bool) -> Result<String, String> {
    not_json(existing)?;
    if existing.trim().is_empty() {
        // A new file: ours as it is, its context the current one.
        return Ok(ours.to_owned());
    }
    // Ours, per list: the item called `name`, its lines from its dash on.
    let theirs = tops(ours);
    let mut new: Vec<(&str, Vec<String>)> = Vec::new();
    for list in LISTS {
        let Some(top) = theirs.iter().find(|t| t.key == list) else {
            continue;
        };
        let (_, items, dash) = items(top)?;
        if let Some(item) = items.iter().find(|i| item_name(i, dash) == Some(name)) {
            new.push((
                list,
                item.iter()
                    .map(|l| l.get(dash..).unwrap_or("").trim_end().to_owned())
                    .collect(),
            ));
        }
    }
    let nl = newline(existing);
    let mut out: Vec<String> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    let mut current = false;
    for top in tops(existing) {
        if top.key == "current-context" && use_context {
            out.push(format!("current-context: {name}"));
            out.extend(top.lines[1..].iter().map(|l| l.to_string()));
            current = true;
            continue;
        }
        let Some((_, lines)) = new.iter().find(|(l, _)| *l == top.key) else {
            out.extend(top.lines.iter().map(|l| l.to_string()));
            continue;
        };
        seen.push(top.key);
        let (before, items, dash) = items(&top)?;
        let dash = if items.is_empty() { 0 } else { dash };
        out.push(format!("{}:", top.key));
        out.extend(before.iter().map(|l| l.to_string()));
        let kept: Vec<&Vec<&str>> = items
            .iter()
            .filter(|i| item_name(i, dash) != Some(name))
            .collect();
        let mut body: Vec<String> = kept
            .iter()
            .flat_map(|i| i.iter().map(|l| l.to_string()))
            .collect();
        // Ours after the list's last line, before blank lines and comments that close it.
        let at = body.iter().rposition(|l| !blank(l)).map_or(0, |i| i + 1);
        let pad = " ".repeat(dash);
        body.splice(
            at..at,
            lines.iter().map(|l| {
                if l.is_empty() {
                    String::new()
                } else {
                    format!("{pad}{l}")
                }
            }),
        );
        out.extend(body);
    }
    // Lists the file didn't have, and the current context if asked for.
    for (list, lines) in &new {
        if !seen.contains(list) {
            out.push(format!("{list}:"));
            out.extend(lines.iter().cloned());
        }
    }
    if use_context && !current {
        out.push(format!("current-context: {name}"));
    }
    Ok(join(&out, nl))
}

/// `existing` without the cluster, user and context called `name`; a
/// current context that was it is cleared.
pub fn remove(existing: &str, name: &str) -> Result<String, String> {
    not_json(existing)?;
    if existing.trim().is_empty() {
        return Ok(existing.to_owned());
    }
    let mut out: Vec<String> = Vec::new();
    let mut changed = false;
    for top in tops(existing) {
        if top.key == "current-context"
            && scalar(top.lines[0].split_once(':').map_or("", |(_, v)| v)) == name
        {
            out.push("current-context: \"\"".into());
            out.extend(top.lines[1..].iter().map(|l| l.to_string()));
            changed = true;
            continue;
        }
        if !LISTS.contains(&top.key) {
            out.extend(top.lines.iter().map(|l| l.to_string()));
            continue;
        }
        let (before, items, dash) = items(&top)?;
        let kept: Vec<&Vec<&str>> = items
            .iter()
            .filter(|i| item_name(i, dash) != Some(name))
            .collect();
        if kept.len() == items.len() {
            out.extend(top.lines.iter().map(|l| l.to_string()));
            continue;
        }
        changed = true;
        // A list left empty is `null`, as kubectl writes one.
        out.push(if kept.is_empty() {
            format!("{}: null", top.key)
        } else {
            top.lines[0].to_owned()
        });
        out.extend(before.iter().map(|l| l.to_string()));
        out.extend(kept.iter().flat_map(|i| i.iter().map(|l| l.to_string())));
    }
    // Unchanged files come back byte for byte, whatever their last line.
    if !changed {
        return Ok(existing.to_owned());
    }
    Ok(join(&out, newline(existing)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const K3S: &str = "apiVersion: v1
clusters:
- cluster:
    certificate-authority-data: Q0E=
    server: https://127.0.0.1:6443
  name: default
contexts:
- context:
    cluster: default
    user: default
  name: default
current-context: default
kind: Config
users:
- name: default
  user:
    client-certificate-data: Q0VSVA==
    client-key-data: S0VZ
";

    /// A file as kubectl and `aws eks update-kubeconfig` leave it, with another Kiri's context.
    const MINE: &str = "apiVersion: v1
clusters:
- cluster:
    certificate-authority-data: RUtT
    server: https://ABC.gr7.eu-central-1.eks.amazonaws.com
  name: arn:aws:eks:eu-central-1:1:cluster/prod
- cluster:
    certificate-authority-data: REVW
    server: https://127.0.0.1:6444
  name: kiri-dev
contexts:
- context:
    cluster: arn:aws:eks:eu-central-1:1:cluster/prod
    user: arn:aws:eks:eu-central-1:1:cluster/prod
  name: prod
- context:
    cluster: kiri-dev
    user: kiri-dev
  name: kiri-dev
current-context: prod
kind: Config
preferences: {}
users:
- name: arn:aws:eks:eu-central-1:1:cluster/prod
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      args:
      - --region
      - eu-central-1
      command: aws
- name: kiri-dev
  user:
    client-certificate-data: REVWQ0VSVA==
";

    fn names(text: &str, list: &str) -> Vec<String> {
        let tops = tops(text);
        let top = tops.iter().find(|t| t.key == list).unwrap();
        let (_, items, dash) = items(top).unwrap();
        items
            .iter()
            .map(|i| item_name(i, dash).unwrap().to_owned())
            .collect()
    }

    #[test]
    fn k3s_names_become_the_engines() {
        let ours = from_k3s(K3S, "kiri", 6445);
        assert!(ours.contains("server: https://127.0.0.1:6445\n  name: kiri\n"));
        assert!(ours.contains("    cluster: kiri\n    user: kiri\n  name: kiri\n"));
        assert!(ours.contains("current-context: kiri\n") && ours.contains("- name: kiri\n"));
        assert!(!ours.contains("default"));
    }

    #[test]
    fn merging_adds_ours_and_keeps_every_other_line_in_place() {
        let ours = from_k3s(K3S, "kiri", 6445);
        let merged = merge(MINE, &ours, "kiri", false).unwrap();
        for list in LISTS {
            let want = if list == "contexts" {
                vec!["prod", "kiri-dev", "kiri"]
            } else {
                vec![
                    "arn:aws:eks:eu-central-1:1:cluster/prod",
                    "kiri-dev",
                    "kiri",
                ]
            };
            assert_eq!(names(&merged, list), want, "{list}");
        }
        // Taking ours out again gives the file back exactly.
        assert_eq!(remove(&merged, "kiri").unwrap(), MINE);
        // The current context stays the user's.
        assert!(merged.contains("current-context: prod\n"));
        assert!(merged.contains("    server: https://127.0.0.1:6445\n  name: kiri\ncontexts:\n"));
        // Nothing changes when merged again.
        assert_eq!(merge(&merged, &ours, "kiri", false).unwrap(), merged);
        // --use makes it current.
        assert!(
            merge(MINE, &ours, "kiri", true)
                .unwrap()
                .contains("current-context: kiri\n")
        );
    }

    #[test]
    fn a_new_port_replaces_the_old_entry() {
        let merged = merge(MINE, &from_k3s(K3S, "kiri", 6444), "kiri", false).unwrap();
        let moved = merge(
            &merged,
            &from_k3s(K3S, "kiri", 6445).replace("Q0E=", "TkVXQ0E="),
            "kiri",
            false,
        )
        .unwrap();
        assert!(moved.contains("https://127.0.0.1:6445") && moved.contains("TkVXQ0E="));
        assert_eq!(moved.matches("name: kiri\n").count(), 3);
        // kiri-dev's own port is untouched.
        assert!(moved.contains("server: https://127.0.0.1:6444\n  name: kiri-dev"));
        assert_eq!(names(&moved, "clusters").len(), 3);
    }

    #[test]
    fn indented_lists_and_crlf_are_kept() {
        let indented = "apiVersion: v1\r\nclusters:\r\n  - cluster:\r\n      server: https://x\r\n    name: \"x\"\r\ncontexts:\r\n  - name: x # theirs\r\n    context:\r\n      cluster: x\r\n      user: x\r\ncurrent-context: x\r\nkind: Config\r\nusers:\r\n  - name: x\r\n    user: {}\r\n";
        let merged = merge(indented, &from_k3s(K3S, "kiri", 6445), "kiri", false).unwrap();
        assert!(merged.contains("\r\n  - cluster:\r\n      certificate-authority-data: Q0E=\r\n      server: https://127.0.0.1:6445\r\n    name: kiri\r\n"));
        assert!(!merged.replace("\r\n", "").contains('\n'));
        assert_eq!(names(&merged, "contexts"), ["x", "kiri"]);
        assert_eq!(remove(&merged, "kiri").unwrap(), indented);
    }

    #[test]
    fn missing_or_empty_lists_and_files() {
        let ours = from_k3s(K3S, "kiri", 6445);
        assert_eq!(merge("", &ours, "kiri", false).unwrap(), ours);
        let bare = "apiVersion: v1\nclusters: null\ncontexts: []\nkind: Config\n";
        let merged = merge(bare, &ours, "kiri", false).unwrap();
        assert_eq!(names(&merged, "clusters"), ["kiri"]);
        assert_eq!(names(&merged, "contexts"), ["kiri"]);
        assert_eq!(names(&merged, "users"), ["kiri"]);
        // It had no current context, and still has none: a plain `kubectl` stays pointed at nothing.
        assert!(!merged.contains("current-context"));
        assert!(
            merge(bare, &ours, "kiri", true)
                .unwrap()
                .ends_with("current-context: kiri\n")
        );
        let removed = remove(&merge(bare, &ours, "kiri", true).unwrap(), "kiri").unwrap();
        assert!(
            removed.contains("clusters: null\n")
                && removed.contains("users: null\n")
                && removed.contains("current-context: \"\"\n")
        );
        assert!(merge("{\"apiVersion\": \"v1\"}", &ours, "kiri", false).is_err());
        assert!(merge("clusters: [{name: a}]\n", &ours, "kiri", false).is_err());
    }

    #[test]
    fn removing_ours_leaves_others_and_clears_a_current_ours() {
        let merged = merge(MINE, &from_k3s(K3S, "kiri-dev", 6446), "kiri-dev", true).unwrap();
        let removed = remove(&merged, "kiri-dev").unwrap();
        assert_eq!(names(&removed, "contexts"), ["prod"]);
        assert!(removed.contains("current-context: \"\"\n"));
        assert_eq!(remove(MINE, "kiri").unwrap(), MINE);
    }

    #[test]
    fn the_file_is_kubeconfigs_first_else_home() {
        let home = Some(PathBuf::from("/home/me"));
        assert_eq!(
            pick(None, home.clone()),
            Some(PathBuf::from("/home/me/.kube/config"))
        );
        let sep = if cfg!(windows) { ";" } else { ":" };
        assert_eq!(
            pick(Some(format!("/a/one{sep}/b/two").into()), home.clone()),
            Some(PathBuf::from("/a/one"))
        );
        assert_eq!(
            pick(Some("".into()), home),
            Some(PathBuf::from("/home/me/.kube/config"))
        );
        assert_eq!(pick(None, None), None);
    }

    #[test]
    fn install_writes_atomically_and_privately() {
        let dir = std::env::temp_dir().join(format!("kiri-kubeconfig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(".kube").join("config");
        let ours = from_k3s(K3S, "kiri", 6445);
        assert!(install(&path, &ours, "kiri", false).unwrap());
        assert!(!install(&path, &ours, "kiri", false).unwrap());
        std::fs::write(&path, MINE).unwrap();
        assert!(install(&path, &ours, "kiri", false).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.join(".kube").join("config.bak.kiri")).unwrap(),
            MINE
        );
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(
                &std::fs::metadata(&path).unwrap().permissions()
            ) & 0o777,
            0o600
        );
        assert!(uninstall(&path, "kiri").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), MINE);
        assert!(!uninstall(&path, "kiri").unwrap());
        // No temporary files left beside it.
        assert_eq!(std::fs::read_dir(dir.join(".kube")).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
