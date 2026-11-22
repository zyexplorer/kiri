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
/// Sibling default order: folders, then notes; each group by created time ascending.
/// A root `order.json` may override sibling order after the user drags rows.
fn walk(dir: &Path) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let order = load_order(dir);
    let mut items = Vec::new();
    let tree = walk_dir(dir, "", &mut items, &order);
    items.sort_by(|a, b| {
        b["updated"]
            .as_u64()
            .cmp(&a["updated"].as_u64())
            .then_with(|| a["title"].as_str().cmp(&b["title"].as_str()))
    });
    (tree, items)
}

fn walk_dir(
    dir: &Path,
    prefix: &str,
    items: &mut Vec<serde_json::Value>,
    order: &serde_json::Map<String, serde_json::Value>,
) -> Vec<serde_json::Value> {
    let mut nodes = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return nodes;
    };
    // (name, created_secs)
    let mut dirs = Vec::new();
    // (stem, path, meta bits)
    let mut notes = Vec::new();
    for ent in entries.flatten() {
        let path = ent.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name == "order.json" {
            continue;
        }
        if path.is_dir() {
            if !valid_seg(name) {
                continue;
            }
            dirs.push((name.to_owned(), file_created_secs(&path)));
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
    // Default: folders before notes; within a kind, oldest created first.
    dirs.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let mut note_nodes = Vec::new();
    for (stem, path) in notes {
        let id = if prefix.is_empty() {
            stem.clone()
        } else {
            format!("{prefix}/{stem}")
        };
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let meta = parse(&raw);
        let created = note_created(&stem, &meta, &path);
        let note = serde_json::json!({
            "id": id, "title": meta.title, "created": created,
            "updated": meta.updated, "collected": meta.collected,
        });
        items.push(note.clone());
        note_nodes.push((
            created,
            stem,
            serde_json::json!({
                "kind": "note",
                "id": id,
                "name": meta.title,
                "title": meta.title,
                "created": created,
                "updated": meta.updated,
                "collected": meta.collected,
            }),
        ));
    }
    note_nodes.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut folder_nodes = Vec::new();
    for (name, created) in dirs {
        let id = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let children = walk_dir(&dir.join(&name), &id, items, order);
        folder_nodes.push(serde_json::json!({
            "kind": "folder",
            "id": id,
            "name": name,
            "created": created,
            "children": children,
        }));
    }
    nodes.extend(folder_nodes);
    nodes.extend(note_nodes.into_iter().map(|(_, _, n)| n));
    apply_sibling_order(nodes, prefix, order)
}

fn read(dir: &Path, id: &str) -> io::Result<serde_json::Value> {
    let path = note_path(dir, id)?;
    let raw = fs::read_to_string(&path)?;
    let meta = parse(&raw);
    let created = note_created(id.rsplit('/').next().unwrap_or(id), &meta, &path);
    Ok(serde_json::json!({
        "type": "note", "id": id, "title": meta.title, "body": meta.body,
        "created": created, "updated": meta.updated, "collected": meta.collected,
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
    let (old_path, id, kept_collected, kept_created) = if id.is_empty() {
        let stem = note_stem(title, &new_id());
        let id = if folder.is_empty() {
            stem
        } else {
            if !valid_path(folder) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad folder"));
            }
            format!("{folder}/{stem}")
        };
        (None, id, collected_in.unwrap_or(false), now_secs())
    } else {
        if !valid_path(id) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad note id"));
        }
        let old = note_path(dir, id)?;
        let prev = fs::read_to_string(&old).ok().map(|raw| parse(&raw));
        let kept = collected_in.unwrap_or_else(|| prev.as_ref().map(|m| m.collected).unwrap_or(false));
        let created = prev
            .as_ref()
            .map(|m| note_created(id.rsplit('/').next().unwrap_or(id), m, &old))
            .unwrap_or_else(now_secs);
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
        (Some(old), new_id, kept, created)
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
    let raw = encode(title, body, kept_created, updated, kept_collected);
    let tmp = path.with_extension("md.tmp");
    fs::write(&tmp, raw)?;
    fs::rename(&tmp, &path)?;
    if let Some(old) = old_path {
        if old != path {
            let leaf_old = old.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let leaf_new = id.rsplit('/').next().unwrap_or(id.as_str());
            let parent = parent_of(&id);
            if leaf_old != leaf_new {
                order_rename_leaf(dir, &parent, leaf_old, leaf_new);
            }
            let _ = fs::remove_file(old);
        }
    } else {
        order_append(dir, &parent_of(&id), leaf_of(&id));
    }
    Ok(serde_json::json!({
        "type": "note", "id": id, "title": title, "body": body,
        "created": kept_created, "updated": updated, "collected": kept_collected,
    }))
}

fn delete_note(dir: &Path, id: &str) -> io::Result<()> {
    let path = note_path(dir, id)?;
    order_remove_leaf(dir, &parent_of(id), leaf_of(id));
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
    order_append(dir, parent, leaf_of(&id));
    Ok(id)
}

fn rmdir(dir: &Path, id: &str) -> io::Result<()> {
    let path = rel_dir(dir, id)?;
    if !path.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "Folder not found"));
    }
    order_remove_leaf(dir, &parent_of(id), leaf_of(id));
    order_drop_subtree(dir, id);
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
        let created = note_created(id.rsplit('/').next().unwrap_or(id), &meta, &note);
        fs::write(&tmp, encode(name, &meta.body, created, updated, meta.collected))?;
        fs::rename(&tmp, &dest)?;
        if note != dest {
            order_rename_leaf(dir, &parent_of(id), leaf_of(id), leaf_of(&new_id));
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
            None => slug.clone(),
        };
        order_rename_leaf(dir, &parent_of(id), leaf_of(id), &slug);
        order_rewrite_prefix(dir, id, &new_id);
        return Ok(new_id);
    }
    Err(io::Error::new(io::ErrorKind::NotFound, "Not found"))
}

/// Move a note or folder under `parent` (empty = notes root).
/// Optional `before` is a sibling id to insert before (omit/empty = append).
fn move_entry(dir: &Path, id: &str, value: &serde_json::Value) -> io::Result<String> {
    if !valid_path(id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad path"));
    }
    let parent = value
        .get("parent")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let before = value
        .get("before")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if !parent.is_empty() && !valid_path(parent) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad parent"));
    }
    if !before.is_empty() && !valid_path(before) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad before"));
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
    if !before.is_empty() && parent_of(before) != parent {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Before must be a sibling"));
    }
    if !before.is_empty() && before == new_id {
        return Ok(id.to_owned());
    }
    if !valid_path(&new_id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Bad destination"));
    }
    let old_parent = parent_of(id);
    if new_id != id {
        if let Ok(src) = note_path(dir, id) {
            let dest = rel_file(dir, &new_id, "md")?;
            if dest.exists() {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Note already exists"));
            }
            if let Some(p) = dest.parent() {
                fs::create_dir_all(p)?;
            }
            fs::rename(src, dest)?;
        } else {
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
            fs::rename(&src, &dest)?;
            order_rewrite_prefix(dir, id, &new_id);
        }
    } else if before.is_empty() && value.get("before").is_none() {
        // No parent change and no reorder request.
        return Ok(id.to_owned());
    }
    order_move_leaf(
        dir,
        &old_parent,
        parent,
        &stem,
        if before.is_empty() {
            None
        } else {
            Some(leaf_of(before))
        },
    );
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
    created: u64,
    updated: u64,
    collected: bool,
}

/// Split a note file into title, body, created/updated and collected flag.
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
            let mut created = 0u64;
            let mut updated = 0u64;
            let mut collected = false;
            for line in fm.lines() {
                if let Some(v) = line.strip_prefix("title:") {
                    title = v.trim().trim_matches('"').to_owned();
                } else if let Some(v) = line.strip_prefix("created:") {
                    created = v.trim().parse().unwrap_or(0);
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
            if created == 0 {
                created = updated;
            }
            return NoteMeta {
                title,
                body: body.to_owned(),
                created,
                updated,
                collected,
            };
        }
    }
    let body = raw.to_owned();
    let now = now_secs();
    NoteMeta {
        title: title_from_body(&body),
        body,
        created: now,
        updated: now,
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

fn encode(title: &str, body: &str, created: u64, updated: u64, collected: bool) -> String {
    let title = title.replace(['\n', '\r'], " ");
    let collected = if collected { "\ncollected: true" } else { "" };
    format!("---\ntitle: {title}\ncreated: {created}\nupdated: {updated}{collected}\n---\n\n{body}")
}

fn leaf_of(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

fn parent_of(id: &str) -> String {
    match id.rsplit_once('/') {
        Some((p, _)) => p.to_owned(),
        None => String::new(),
    }
}

fn file_created_secs(path: &Path) -> u64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.created().or_else(|_| m.modified()).ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Prefer frontmatter `created`, else the `n{millis}` unique id, else file time.
fn note_created(stem: &str, meta: &NoteMeta, path: &Path) -> u64 {
    if meta.created > 0 && meta.created != meta.updated {
        return meta.created;
    }
    if meta.created > 0 {
        // created was defaulted to updated in parse — still try the id clock.
        if let Some(t) = created_from_unique(&note_unique(stem)) {
            return t;
        }
        return meta.created;
    }
    created_from_unique(&note_unique(stem)).unwrap_or_else(|| file_created_secs(path))
}

fn created_from_unique(unique: &str) -> Option<u64> {
    let s = unique.strip_prefix('n')?;
    let ms: u128 = s.parse().ok()?;
    if ms < 1_000 {
        return None;
    }
    Some((ms / 1000) as u64)
}

fn order_path(root: &Path) -> PathBuf {
    root.join("order.json")
}

fn load_order(root: &Path) -> serde_json::Map<String, serde_json::Value> {
    let Ok(raw) = fs::read_to_string(order_path(root)) else {
        return serde_json::Map::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_order(root: &Path, map: &serde_json::Map<String, serde_json::Value>) {
    let path = order_path(root);
    if map.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    if let Ok(raw) = serde_json::to_string_pretty(map) {
        let _ = fs::write(path, raw);
    }
}

fn apply_sibling_order(
    mut nodes: Vec<serde_json::Value>,
    parent: &str,
    order: &serde_json::Map<String, serde_json::Value>,
) -> Vec<serde_json::Value> {
    let Some(arr) = order.get(parent).and_then(|v| v.as_array()) else {
        return nodes;
    };
    let mut by_leaf = std::collections::HashMap::new();
    for n in nodes.drain(..) {
        let leaf = n
            .get("id")
            .and_then(|v| v.as_str())
            .map(leaf_of)
            .unwrap_or("")
            .to_owned();
        by_leaf.insert(leaf, n);
    }
    let mut out = Vec::new();
    for leaf in arr.iter().filter_map(|v| v.as_str()) {
        if let Some(n) = by_leaf.remove(leaf) {
            out.push(n);
        }
    }
    // Unordered leftovers keep default kind/created order.
    let mut rest: Vec<_> = by_leaf.into_values().collect();
    rest.sort_by(|a, b| {
        let ka = if a["kind"] == "folder" { 0 } else { 1 };
        let kb = if b["kind"] == "folder" { 0 } else { 1 };
        ka.cmp(&kb).then_with(|| {
            a["created"]
                .as_u64()
                .cmp(&b["created"].as_u64())
                .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
        })
    });
    out.extend(rest);
    out
}

fn materialize_parent_order(root: &Path, parent: &str) -> Vec<String> {
    let mut map = load_order(root);
    if let Some(arr) = map.get(parent).and_then(|v| v.as_array()) {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
    }
    let path = if parent.is_empty() {
        root.to_path_buf()
    } else {
        match rel_dir(root, parent) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        }
    };
    let empty = serde_json::Map::new();
    let mut flat = Vec::new();
    let kids = walk_dir(&path, parent, &mut flat, &empty);
    let leaves: Vec<String> = kids
        .iter()
        .filter_map(|n| n.get("id").and_then(|v| v.as_str()).map(|id| leaf_of(id).to_owned()))
        .collect();
    map.insert(
        parent.to_owned(),
        serde_json::Value::Array(leaves.iter().cloned().map(serde_json::Value::String).collect()),
    );
    save_order(root, &map);
    leaves
}

fn order_append(root: &Path, parent: &str, leaf: &str) {
    let mut map = load_order(root);
    if !map.contains_key(parent) {
        // No custom order yet — default sort still applies until the user drags.
        return;
    }
    let arr = map
        .entry(parent.to_owned())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if let Some(a) = arr.as_array_mut() {
        if a.iter().any(|v| v.as_str() == Some(leaf)) {
            return;
        }
        a.push(serde_json::Value::String(leaf.to_owned()));
    }
    save_order(root, &map);
}

fn order_remove_leaf(root: &Path, parent: &str, leaf: &str) {
    let mut map = load_order(root);
    let Some(arr) = map.get_mut(parent).and_then(|v| v.as_array_mut()) else {
        return;
    };
    arr.retain(|v| v.as_str() != Some(leaf));
    if arr.is_empty() {
        map.remove(parent);
    }
    save_order(root, &map);
}

fn order_rename_leaf(root: &Path, parent: &str, old_leaf: &str, new_leaf: &str) {
    if old_leaf.is_empty() || old_leaf == new_leaf {
        return;
    }
    let mut map = load_order(root);
    let Some(arr) = map.get_mut(parent).and_then(|v| v.as_array_mut()) else {
        return;
    };
    for v in arr.iter_mut() {
        if v.as_str() == Some(old_leaf) {
            *v = serde_json::Value::String(new_leaf.to_owned());
        }
    }
    save_order(root, &map);
}

fn order_drop_subtree(root: &Path, id: &str) {
    let mut map = load_order(root);
    let prefix = format!("{id}/");
    map.retain(|k, _| k != id && !k.starts_with(&prefix));
    save_order(root, &map);
}

fn order_rewrite_prefix(root: &Path, old_id: &str, new_id: &str) {
    if old_id == new_id {
        return;
    }
    let mut map = load_order(root);
    let old_prefix = format!("{old_id}/");
    let new_prefix = format!("{new_id}/");
    let keys: Vec<_> = map.keys().cloned().collect();
    for k in keys {
        if k == old_id {
            if let Some(v) = map.remove(&k) {
                map.insert(new_id.to_owned(), v);
            }
        } else if let Some(rest) = k.strip_prefix(&old_prefix) {
            if let Some(v) = map.remove(&k) {
                map.insert(format!("{new_prefix}{rest}"), v);
            }
        }
    }
    save_order(root, &map);
}

fn order_move_leaf(
    root: &Path,
    old_parent: &str,
    new_parent: &str,
    leaf: &str,
    before_leaf: Option<&str>,
) {
    // Materialize both parents so a first drag captures the current default order.
    let mut old_list = materialize_parent_order(root, old_parent);
    let mut new_list = if old_parent == new_parent {
        old_list.clone()
    } else {
        materialize_parent_order(root, new_parent)
    };
    old_list.retain(|x| x != leaf);
    new_list.retain(|x| x != leaf);
    let at = before_leaf
        .and_then(|b| new_list.iter().position(|x| x == b))
        .unwrap_or(new_list.len());
    new_list.insert(at, leaf.to_owned());
    let mut map = load_order(root);
    if old_parent == new_parent {
        map.insert(
            new_parent.to_owned(),
            serde_json::Value::Array(new_list.into_iter().map(serde_json::Value::String).collect()),
        );
    } else {
        if old_list.is_empty() {
            map.remove(old_parent);
        } else {
            map.insert(
                old_parent.to_owned(),
                serde_json::Value::Array(old_list.into_iter().map(serde_json::Value::String).collect()),
            );
        }
        map.insert(
            new_parent.to_owned(),
            serde_json::Value::Array(new_list.into_iter().map(serde_json::Value::String).collect()),
        );
    }
    save_order(root, &map);
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
        let t = now_secs();
        fs::write(
            dir.join(format!("{legacy}.md")),
            encode("Old", "body", t, t, false),
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
        let collected = parse(&encode("Cmd", "```bash\ndocker run\n```", 1, 1, true));
        assert!(collected.collected);
        assert_eq!(collected.title, "Cmd");
        assert_eq!(collected.created, 1);
    }

    #[test]
    fn siblings_default_folders_then_notes_by_created() {
        let dir = scratch("sort");
        let t0 = now_secs().saturating_sub(30);
        fs::create_dir(dir.join("z-folder")).unwrap();
        // Older note id clock, newer folder name would sort first alphabetically —
        // created order must put the older note after folders still, and folders by time.
        let older = format!("alpha-n{}000", t0.saturating_sub(10));
        let newer = format!("zeta-n{}000", t0);
        fs::write(dir.join(format!("{older}.md")), encode("Alpha", "a", t0 - 10, t0, false)).unwrap();
        fs::write(dir.join(format!("{newer}.md")), encode("Zeta", "z", t0, t0, false)).unwrap();
        let (tree, _) = walk(&dir);
        assert_eq!(tree[0]["kind"], "folder");
        assert_eq!(tree[1]["kind"], "note");
        assert_eq!(tree[1]["title"], "Alpha");
        assert_eq!(tree[2]["title"], "Zeta");
        // Drag Zeta before Alpha among notes (materializes order).
        move_entry(
            &dir,
            &newer,
            &serde_json::json!({"parent": "", "before": older}),
        )
        .unwrap();
        let (tree2, _) = walk(&dir);
        assert_eq!(tree2[0]["kind"], "folder");
        assert_eq!(tree2[1]["title"], "Zeta");
        assert_eq!(tree2[2]["title"], "Alpha");
        let _ = fs::remove_dir_all(&dir);
    }
}
