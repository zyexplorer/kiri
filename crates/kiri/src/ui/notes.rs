//! Personal Markdown notes under the app state directory (`notes/`).
//!
//! Host-side only: not the daemon or the VM. Notes live as
//! `{title-slug}-{id}.md` files in a folder tree; the page lists, opens,
//! saves, renames and deletes through `notes` / `noteread` / `notesave` /
//! `notedelete` / `notemkdir` / `notermdir` / `noterename` / `notemove` /
//! `notereveal`.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// True when `cmd` is a notes command (handled here).
pub fn handle(
    cmd: &str,
    id: &str,
    value: &serde_json::Value,
    state: Option<PathBuf>,
    emit: impl Fn(String),
) -> bool {
    if !matches!(
        cmd,
        "notes"
            | "noteread"
            | "notesave"
            | "notedelete"
            | "notemkdir"
            | "notermdir"
            | "noterename"
            | "notemove"
            | "notereveal"
            | "imgnotes"
            | "imgnote"
            | "imgnotesave"
    ) {
        return false;
    }
    let Some(state) = state else {
        emit(err("Could not find Kiri's data folder"));
        return true;
    };
    // Per-image Markdown remarks (~/.kiri/img-notes.json), separate from the Notes tree.
    if matches!(cmd, "imgnotes" | "imgnote" | "imgnotesave") {
        match cmd {
            "imgnotes" => emit(img_notes_list(&state)),
            "imgnote" => emit(img_note_read(&state, id)),
            "imgnotesave" => match img_note_save(&state, id, value) {
                Ok(body) => {
                    emit(
                        serde_json::json!({"type": "imgnote", "name": id, "body": body, "saved": true})
                            .to_string(),
                    );
                    emit(img_notes_list(&state));
                    emit(
                        serde_json::json!({"type": "notice", "kind": "ok", "text": "Remark saved"})
                            .to_string(),
                    );
                }
                Err(e) => emit(err(&e.to_string())),
            },
            _ => {}
        }
        return true;
    }
    let dir = state.join("notes");
    if let Err(_e) = fs::create_dir_all(&dir) {
        // Fixed English key so the page can translate it (tr(e.text)).
        emit(err("Could not create the notes folder"));
        return true;
    }
    match cmd {
        "notes" => emit(list_event(&dir)),
        "noteread" => match read(&dir, id) {
            Ok(n) => emit(n.to_string()),
            Err(e) => emit(err(&e.to_string())),
        },
        "notesave" => match save(&dir, id, value) {
            Ok(n) => {
                emit(
                    serde_json::json!({
                        "type": "notesaved", "id": n["id"], "title": n["title"],
                        "updated": n["updated"], "collected": n["collected"],
                    })
                    .to_string(),
                );
                emit(n.to_string());
                emit(list_event(&dir));
            }
            Err(e) => emit(err(&e.to_string())),
        },
        "notedelete" => match delete_note(&dir, id) {
            Ok(()) => {
                emit(serde_json::json!({"type": "notedeleted", "id": id}).to_string());
                emit(list_event(&dir));
            }
            Err(e) => emit(err(&e.to_string())),
        },
        "notemkdir" => match mkdir(&dir, value) {
            Ok(folder) => {
                emit(serde_json::json!({"type": "notefolder", "op": "mkdir", "id": folder}).to_string());
                emit(list_event(&dir));
            }
            Err(e) => emit(err(&e.to_string())),
        },
        "notermdir" => match rmdir(&dir, id) {
            Ok(()) => {
                emit(serde_json::json!({"type": "notefolder", "op": "rmdir", "id": id}).to_string());
                emit(list_event(&dir));
            }
            Err(e) => emit(err(&e.to_string())),
        },
        "noterename" => match rename_entry(&dir, id, value) {
            Ok(new_id) => {
                emit(
                    serde_json::json!({"type": "noterename", "id": id, "newId": new_id}).to_string(),
                );
                emit(list_event(&dir));
            }
            Err(e) => emit(err(&e.to_string())),
        },
        "notemove" => match move_entry(&dir, id, value) {
            Ok(new_id) => {
                emit(
                    serde_json::json!({"type": "notemove", "id": id, "newId": new_id}).to_string(),
                );
                emit(list_event(&dir));
            }
            Err(e) => emit(err(&e.to_string())),
        },
        "notereveal" => {
            // Note file, folder, or the notes root — select it in Finder / file manager.
            if id.is_empty() {
                super::reveal(&dir);
            } else if let Ok(path) = note_path(&dir, id) {
                super::reveal(&path);
            } else if let Ok(folder) = rel_dir(&dir, id) {
                if folder.is_dir() {
                    super::reveal(&folder);
                } else {
                    emit(err("Folder not found"));
                }
            } else {
                emit(err("Not found"));
            }
        }
        _ => {}
    }
    true
}

/// Host-side Markdown remarks keyed by image name (`repo:tag` or id).
fn img_notes_path(state: &Path) -> PathBuf {
    state.join("img-notes.json")
}

fn img_notes_map(state: &Path) -> serde_json::Map<String, serde_json::Value> {
    let Ok(bytes) = fs::read(img_notes_path(state)) else {
        return serde_json::Map::new();
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn img_notes_list(state: &Path) -> String {
    let names: Vec<String> = img_notes_map(state)
        .into_iter()
        .filter_map(|(k, v)| {
            let body = v.as_str().unwrap_or("").trim();
            (!body.is_empty()).then_some(k)
        })
        .collect();
    serde_json::json!({"type": "imgnotes", "names": names}).to_string()
}

fn img_note_read(state: &Path, name: &str) -> String {
    let body = img_notes_map(state)
        .get(name)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    serde_json::json!({"type": "imgnote", "name": name, "body": body}).to_string()
}

fn img_note_save(state: &Path, name: &str, value: &serde_json::Value) -> io::Result<String> {
    if name.is_empty() || name.len() > 512 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid image name"));
    }
    let body = value
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut map = img_notes_map(state);
    if body.trim().is_empty() {
        map.remove(name);
    } else {
        map.insert(name.to_string(), serde_json::Value::String(body.clone()));
    }
    let path = img_notes_path(state);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::Value::Object(map).to_string();
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)?;
    Ok(body)
}

fn err(text: &str) -> String {
    serde_json::json!({"type": "notice", "kind": "error", "text": text}).to_string()
}

fn list_event(dir: &Path) -> String {
    let (tree, items) = walk(dir);
    serde_json::json!({"type": "notes", "tree": tree, "items": items}).to_string()
}

/// Recursive folder tree plus a flat note list (newest first) for the page.
fn walk(dir: &Path) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let mut items = Vec::new();
    let tree = walk_dir(dir, "", &mut items);
    items.sort_by(|a, b| {
        b["updated"]
            .as_u64()
            .cmp(&a["updated"].as_u64())
            .then_with(|| a["title"].as_str().cmp(&b["title"].as_str()))
    });
    (tree, items)
}

fn walk_dir(dir: &Path, prefix: &str, items: &mut Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut nodes = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return nodes;
    };
    let mut dirs = Vec::new();
    let mut notes = Vec::new();
    for ent in entries.flatten() {
        let path = ent.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if !valid_seg(name) {
                continue;
            }
            dirs.push(name.to_owned());
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !valid_seg(stem) {
                continue;
            }
            notes.push((stem.to_owned(), path));
        }
    }
    dirs.sort();
    notes.sort_by(|a, b| a.0.cmp(&b.0));
    for name in dirs {
        let id = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let children = walk_dir(&dir.join(&name), &id, items);
        nodes.push(serde_json::json!({
            "kind": "folder",
            "id": id,
            "name": name,
            "children": children,
        }));
    }
    for (stem, path) in notes {
        let id = if prefix.is_empty() {
            stem
        } else {
            format!("{prefix}/{stem}")
        };
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let meta = parse(&raw);
        let note = serde_json::json!({
            "id": id, "title": meta.title, "updated": meta.updated, "collected": meta.collected,
        });
        items.push(note.clone());
        nodes.push(serde_json::json!({
            "kind": "note",
            "id": id,
            "name": meta.title,
            "title": meta.title,
            "updated": meta.updated,
            "collected": meta.collected,
        }));
    }
    nodes
}

fn read(dir: &Path, id: &str) -> io::Result<serde_json::Value> {
    let path = note_path(dir, id)?;
    let raw = fs::read_to_string(&path)?;
    let meta = parse(&raw);
    Ok(serde_json::json!({
        "type": "note", "id": id, "title": meta.title, "body": meta.body,
        "updated": meta.updated, "collected": meta.collected,
    }))
}

fn save(dir: &Path, id: &str, value: &serde_json::Value) -> io::Result<serde_json::Value> {
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let title = if title.is_empty() { "Untitled" } else { title };
    let body = value.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let folder = value
        .get("folder")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let collected_in = value.get("collected").and_then(|v| v.as_bool());
    let (old_path, id, kept_collected) = if id.is_empty() {
        let stem = note_stem(title, &new_id());
        let id = if folder.is_empty() {
            stem
        } else {
            if !valid_path(folder) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad folder"));
            }
            format!("{folder}/{stem}")
        };
        (None, id, collected_in.unwrap_or(false))
    } else {
        if !valid_path(id) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad note id"));
        }
        let old = note_path(dir, id)?;
        let kept = collected_in.unwrap_or_else(|| {
            fs::read_to_string(&old)
                .map(|raw| parse(&raw).collected)
                .unwrap_or(false)
        });
        let folder = match id.rsplit_once('/') {
            Some((p, _)) => p,
            None => "",
        };
        let unique = note_unique(id.rsplit('/').next().unwrap_or(id));
        let stem = note_stem(title, &unique);
        let new_id = if folder.is_empty() {
            stem
        } else {
            format!("{folder}/{stem}")
        };
        if !valid_path(&new_id) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad note id"));
        }
        (Some(old), new_id, kept)
    };
    let path = rel_file(dir, &id, "md")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(old) = &old_path {
        if old != &path && path.exists() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Note already exists"));
        }
    }
    let updated = now_secs();
    let raw = encode(title, body, updated, kept_collected);
    let tmp = path.with_extension("md.tmp");
    fs::write(&tmp, raw)?;
    fs::rename(&tmp, &path)?;
    if let Some(old) = old_path {
        if old != path {
            let _ = fs::remove_file(old);
        }
    }
    Ok(serde_json::json!({
        "type": "note", "id": id, "title": title, "body": body,
        "updated": updated, "collected": kept_collected,
    }))
}

fn delete_note(dir: &Path, id: &str) -> io::Result<()> {
    let path = note_path(dir, id)?;
    fs::remove_file(path)
}

fn mkdir(dir: &Path, value: &serde_json::Value) -> io::Result<String> {
    let parent = value
        .get("parent")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let name = if name.is_empty() { "New folder" } else { name };
    let name = slug_name(name);
    if !valid_seg(&name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad folder name"));
    }
    let id = if parent.is_empty() {
        name.clone()
    } else {
        if !valid_path(parent) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad parent folder"));
        }
        format!("{parent}/{name}")
    };
    if !valid_path(&id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad folder path"));
    }
    let path = rel_dir(dir, &id)?;
    if path.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Folder already exists"));
    }
    fs::create_dir_all(&path)?;
    Ok(id)
}

fn rmdir(dir: &Path, id: &str) -> io::Result<()> {
    let path = rel_dir(dir, id)?;
    if !path.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "Folder not found"));
    }
    fs::remove_dir_all(path)
}

fn rename_entry(dir: &Path, id: &str, value: &serde_json::Value) -> io::Result<String> {
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Name required"));
    }
    if let Ok(note) = note_path(dir, id) {
        // Rename the title and the on-disk `{name}-{id}.md` stem; body kept.
        let raw = fs::read_to_string(&note)?;
        let meta = parse(&raw);
        let updated = now_secs();
        let folder = match id.rsplit_once('/') {
            Some((p, _)) => p,
            None => "",
        };
        let unique = note_unique(id.rsplit('/').next().unwrap_or(id));
        let stem = note_stem(name, &unique);
        let new_id = if folder.is_empty() {
            stem
        } else {
            format!("{folder}/{stem}")
        };
        if !valid_path(&new_id) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad note name"));
        }
        let dest = rel_file(dir, &new_id, "md")?;
        if new_id != id && dest.exists() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Note already exists"));
        }
        let tmp = dest.with_extension("md.tmp");
        fs::write(&tmp, encode(name, &meta.body, updated, meta.collected))?;
        fs::rename(&tmp, &dest)?;
        if note != dest {
            let _ = fs::remove_file(note);
        }
        return Ok(new_id);
    }
    let folder = rel_dir(dir, id)?;
    if folder.is_dir() {
        let slug = slug_name(name);
        if !valid_seg(&slug) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad folder name"));
        }
        let parent = folder.parent().unwrap_or(dir);
        let dest = parent.join(&slug);
        if dest.exists() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Folder already exists"));
        }
        fs::rename(&folder, &dest)?;
        let new_id = match id.rsplit_once('/') {
            Some((p, _)) => format!("{p}/{slug}"),
            None => slug,
        };
        return Ok(new_id);
    }
    Err(io::Error::new(io::ErrorKind::NotFound, "Not found"))
}

/// Move a note or folder under `parent` (empty = notes root).
fn move_entry(dir: &Path, id: &str, value: &serde_json::Value) -> io::Result<String> {
    if !valid_path(id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad path"));
    }
    let parent = value
        .get("parent")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if !parent.is_empty() && !valid_path(parent) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad parent"));
    }
    // Refuse moving a folder into itself or a descendant.
    if !parent.is_empty() && (parent == id || parent.starts_with(&format!("{id}/"))) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Cannot move into itself"));
    }
    let stem = id.rsplit('/').next().unwrap_or(id).to_owned();
    let new_id = if parent.is_empty() {
        stem.clone()
    } else {
        format!("{parent}/{stem}")
    };
    if new_id == id {
        return Ok(id.to_owned());
    }
    if !valid_path(&new_id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad destination"));
    }
    if let Ok(src) = note_path(dir, id) {
        let dest = rel_file(dir, &new_id, "md")?;
        if dest.exists() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Note already exists"));
        }
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p)?;
        }
        fs::rename(src, dest)?;
        return Ok(new_id);
    }
    let src = rel_dir(dir, id)?;
    if !src.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "Not found"));
    }
    let dest = rel_dir(dir, &new_id)?;
    if dest.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Folder already exists"));
    }
    if let Some(p) = dest.parent() {
        fs::create_dir_all(p)?;
    }
    fs::rename(src, dest)?;
    Ok(new_id)
}

fn note_path(dir: &Path, id: &str) -> io::Result<PathBuf> {
    let path = rel_file(dir, id, "md")?;
    if !path.is_file() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "Note not found"));
    }
    Ok(path)
}

fn rel_file(root: &Path, id: &str, ext: &str) -> io::Result<PathBuf> {
    let path = rel_path(root, id)?.with_extension(ext);
    Ok(path)
}

fn rel_dir(root: &Path, id: &str) -> io::Result<PathBuf> {
    rel_path(root, id)
}

fn rel_path(root: &Path, id: &str) -> io::Result<PathBuf> {
    if !valid_path(id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad path"));
    }
    let mut out = root.to_path_buf();
    for seg in id.split('/') {
        out.push(seg);
    }
    // Refuse anything that escapes the notes root.
    let root_c = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if let Ok(c) = out.canonicalize() {
        if !c.starts_with(&root_c) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad path"));
        }
    } else {
        // Not created yet: check components only.
        for c in Path::new(id).components() {
            if !matches!(c, Component::Normal(_)) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad path"));
            }
        }
    }
    Ok(out)
}

fn valid_seg(s: &str) -> bool {
    if s.is_empty() || s == "." || s == ".." || s.chars().count() > 64 {
        return false;
    }
    let mut chars = s.chars();
    match chars.next() {
        // Letters/digits in any script (ASCII, CJK, …); refuse `.` / `/` via the rest.
        Some(c) if c.is_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

fn valid_path(id: &str) -> bool {
    if id.is_empty() || id.chars().count() > 200 {
        return false;
    }
    let parts: Vec<_> = id.split('/').collect();
    (1..=6).contains(&parts.len()) && parts.iter().all(|p| valid_seg(p))
}

fn slug_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.push(c);
        } else if (c == '_' || c == '-' || c.is_whitespace()) && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').chars().take(48).collect::<String>();
    if out.is_empty() {
        "folder".into()
    } else {
        out
    }
}

/// On-disk note stem: `{slug(title)}-{unique}` so Finder shows the name.
fn note_stem(title: &str, unique: &str) -> String {
    format!("{}-{unique}", slug_name(title))
}

/// The trailing `n…` id from a stem (`Title-n123` or legacy bare `n123`).
fn note_unique(stem: &str) -> String {
    if let Some(i) = stem.rfind("-n") {
        let suf = &stem[i + 1..];
        if suf.len() > 1 && suf.as_bytes()[0] == b'n' && suf[1..].chars().all(|c| c.is_ascii_digit())
        {
            return suf.to_owned();
        }
    }
    if stem.starts_with('n')
        && stem.len() > 1
        && stem.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return stem.to_owned();
    }
    stem.to_owned()
}

fn new_id() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("n{ms}")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parsed note frontmatter + body. `collected` marks Save-to-Notes captures.
struct NoteMeta {
    title: String,
    body: String,
    updated: u64,
    collected: bool,
}

/// Split a note file into title, body, updated and collected flag.
fn parse(raw: &str) -> NoteMeta {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    if let Some(rest) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    {
        let close = rest
            .find("\n---\n")
            .map(|i| (i, "\n---\n".len()))
            .or_else(|| rest.find("\r\n---\r\n").map(|i| (i, "\r\n---\r\n".len())))
            .or_else(|| rest.find("\n---\r\n").map(|i| (i, "\n---\r\n".len())));
        if let Some((end, mark)) = close {
            let fm = &rest[..end];
            let body = rest[end + mark..].trim_start_matches(['\r', '\n']);
            let mut title = String::new();
            let mut updated = 0u64;
            let mut collected = false;
            for line in fm.lines() {
                if let Some(v) = line.strip_prefix("title:") {
                    title = v.trim().trim_matches('"').to_owned();
                } else if let Some(v) = line.strip_prefix("updated:") {
                    updated = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = line.strip_prefix("collected:") {
                    let v = v.trim().to_ascii_lowercase();
                    collected = matches!(v.as_str(), "true" | "yes" | "1");
                }
            }
            if title.is_empty() {
                title = title_from_body(body);
            }
            if updated == 0 {
                updated = now_secs();
            }
            return NoteMeta {
                title,
                body: body.to_owned(),
                updated,
                collected,
            };
        }
    }
    let body = raw.to_owned();
    NoteMeta {
        title: title_from_body(&body),
        body,
        updated: now_secs(),
        collected: false,
    }
}

fn title_from_body(body: &str) -> String {
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let t = t.strip_prefix('#').map(str::trim).unwrap_or(t);
        if !t.is_empty() {
            return t.chars().take(80).collect();
        }
    }
    "Untitled".into()
}

fn encode(title: &str, body: &str, updated: u64, collected: bool) -> String {
    let title = title.replace(['\n', '\r'], " ");
    let collected = if collected { "\ncollected: true" } else { "" };
    format!("---\ntitle: {title}\nupdated: {updated}{collected}\n---\n\n{body}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kiri-notes-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trip_save_list_read_delete() {
        let dir = scratch("round");
        let saved = save(
            &dir,
            "",
            &serde_json::json!({"title": "Compose tip", "body": "# Hi\n\n```yaml\nservices:\n  web:\n    image: nginx\n```"}),
        )
        .unwrap();
        let id = saved["id"].as_str().unwrap();
        assert!(valid_path(id));
        assert!(id.starts_with("Compose-tip-n"), "id={id}");
        assert!(dir.join(format!("{id}.md")).is_file());
        let (tree, items) = walk(&dir);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "Compose tip");
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0]["kind"], "note");
        let got = read(&dir, id).unwrap();
        assert_eq!(got["title"], "Compose tip");
        assert!(got["body"].as_str().unwrap().contains("nginx"));
        delete_note(&dir, id).unwrap();
        assert!(walk(&dir).1.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_note_updates_filename_keeps_unique() {
        let dir = scratch("rename-file");
        let saved = save(
            &dir,
            "",
            &serde_json::json!({"title": "nginx 的启动命令", "body": "docker run"}),
        )
        .unwrap();
        let id = saved["id"].as_str().unwrap().to_owned();
        assert!(id.starts_with("nginx-的启动命令-n"), "id={id}");
        let unique = note_unique(id.rsplit('/').next().unwrap());
        let renamed = rename_entry(&dir, &id, &serde_json::json!({"name": "新名字"})).unwrap();
        assert_eq!(renamed, format!("新名字-{unique}"));
        assert!(!dir.join(format!("{id}.md")).exists());
        assert!(dir.join(format!("{renamed}.md")).is_file());
        assert_eq!(read(&dir, &renamed).unwrap()["title"], "新名字");
        // Saving with a new title also renames the file.
        let saved2 = save(
            &dir,
            &renamed,
            &serde_json::json!({"title": "again", "body": "docker run"}),
        )
        .unwrap();
        let id2 = saved2["id"].as_str().unwrap();
        assert_eq!(id2, format!("again-{unique}"));
        assert!(dir.join(format!("{id2}.md")).is_file());
        // Legacy bare `n….md` still opens and migrates on rename.
        let legacy = format!("n{}", 1_794_000_000_000u64);
        fs::write(
            dir.join(format!("{legacy}.md")),
            encode("Old", "body", now_secs(), false),
        )
        .unwrap();
        let migrated = rename_entry(&dir, &legacy, &serde_json::json!({"name": "Migrated"})).unwrap();
        assert_eq!(migrated, format!("Migrated-{legacy}"));
        assert!(dir.join(format!("{migrated}.md")).is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_tree_crud() {
        let dir = scratch("tree");
        let folder = mkdir(&dir, &serde_json::json!({"parent": "", "name": "Ops Docs"})).unwrap();
        assert_eq!(folder, "Ops-Docs");
        let note = save(
            &dir,
            "",
            &serde_json::json!({"title": "Nginx", "body": "run", "folder": folder}),
        )
        .unwrap();
        let id = note["id"].as_str().unwrap();
        assert!(id.starts_with("Ops-Docs/Nginx-n"), "id={id}");
        let (tree, items) = walk(&dir);
        assert_eq!(items.len(), 1);
        assert_eq!(tree[0]["kind"], "folder");
        assert_eq!(tree[0]["children"][0]["kind"], "note");
        let renamed = rename_entry(&dir, &folder, &serde_json::json!({"name": "prod"})).unwrap();
        assert_eq!(renamed, "prod");
        let stem = id.split('/').next_back().unwrap();
        assert!(read(&dir, &format!("prod/{stem}")).is_ok());
        let moved = move_entry(
            &dir,
            &format!("prod/{stem}"),
            &serde_json::json!({"parent": ""}),
        )
        .unwrap();
        assert_eq!(moved, stem);
        assert!(read(&dir, &stem).is_ok());
        rmdir(&dir, "prod").unwrap();
        delete_note(&dir, &stem).unwrap();
        assert!(walk(&dir).1.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mkdir_accepts_cjk_names_and_nested_parent() {
        let dir = scratch("cjk");
        let root = mkdir(&dir, &serde_json::json!({"parent": "", "name": "工作"})).unwrap();
        assert_eq!(root, "工作");
        let child = mkdir(&dir, &serde_json::json!({"parent": root, "name": "子目录"})).unwrap();
        assert_eq!(child, "工作/子目录");
        assert!(dir.join("工作").join("子目录").is_dir());
        let (tree, _) = walk(&dir);
        assert_eq!(tree[0]["name"], "工作");
        assert_eq!(tree[0]["children"][0]["name"], "子目录");
        rmdir(&dir, &child).unwrap();
        rmdir(&dir, &root).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_path_traversal_ids() {
        assert!(!valid_path("../x"));
        assert!(!valid_path("a/../b"));
        assert!(!valid_path(""));
        assert!(!valid_seg(""));
        assert!(valid_path("n1730000000000"));
        assert!(valid_path("ops/docker/n1"));
    }

    #[test]
    fn image_remarks_round_trip() {
        let dir = scratch("img-notes");
        assert!(img_note_save(&dir, "nginx:alpine", &serde_json::json!({"body": "# Hello\n\nworld"}))
            .unwrap()
            .contains("Hello"));
        let read = img_note_read(&dir, "nginx:alpine");
        let v: serde_json::Value = serde_json::from_str(&read).unwrap();
        assert_eq!(v["body"], "# Hello\n\nworld");
        let list: serde_json::Value = serde_json::from_str(&img_notes_list(&dir)).unwrap();
        assert_eq!(list["names"], serde_json::json!(["nginx:alpine"]));
        img_note_save(&dir, "nginx:alpine", &serde_json::json!({"body": "  "})).unwrap();
        let list2: serde_json::Value = serde_json::from_str(&img_notes_list(&dir)).unwrap();
        assert_eq!(list2["names"], serde_json::json!([]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_without_front_matter_uses_heading() {
        let meta = parse("# Docker run\n\ndocker run -d nginx\n");
        assert_eq!(meta.title, "Docker run");
        assert!(meta.body.starts_with("# Docker run"));
        assert!(!meta.collected);
        let collected = parse(&encode("Cmd", "```bash\ndocker run\n```", 1, true));
        assert!(collected.collected);
        assert_eq!(collected.title, "Cmd");
    }
}
