//! Local project workspaces: linked directories and distributable kits.
//!
//! Host-side only. A project points at a root folder (an IDE `src/docker`
//! tree, or an imported kit under `kits/<id>/`). The page lists scanned
//! scripts, compose files and Dockerfiles, pins favourites, runs scripts
//! against Kiri's engine, opens compose in the Compose section, and
//! exports/imports `.kirikit` zip packs.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const MAX_DEPTH: usize = 3;
const SKIP_DIRS: &[&str] = &[".git", "context", "node_modules", "target", ".idea", ".vscode", "__pycache__"];

/// True when `cmd` is handled here.
pub fn handle(
    cmd: &str,
    id: &str,
    value: &Value,
    state: Option<PathBuf>,
    emit: impl Fn(String) + Send + Sync + Clone + 'static,
    open_compose: impl Fn(PathBuf) + Send + Sync + Clone + 'static,
) -> bool {
    if !matches!(
        cmd,
        "projects"
            | "projectremove"
            | "projectscan"
            | "projectpin"
            | "projectmeta"
            | "projectrun"
            | "projectbuild"
            | "projectcompose"
            | "projectreveal"
            | "projecttree"
            | "projectexport"
            | "projectimport"
            | "projectadd"
            | "projectcancel"
            | "projectmkdir"
            | "projectrmpath"
            | "projectwrite"
            | "projectcopyin"
            | "projectpaste"
            | "projectread"
            | "projectsave"
            | "projectrename"
            | "projectexec"
            | "projectnewfile"
    ) {
        return false;
    }
    let Some(state) = state else {
        emit(err("Could not find Kiri's data folder"));
        return true;
    };
    match cmd {
        "projects" => emit(list_event(&state)),
        "projectadd" => match create_project(&state, value) {
            Ok(p) => {
                emit(json!({"type": "project", "project": project_view(&p)}).to_string());
                emit(list_event(&state));
                emit(notice("ok", &format!("Created {}", p.name)));
            }
            Err(e) => emit(err(&e)),
        },
        "projectimport" => match import_kit(&state, value) {
            Ok(p) => {
                emit(json!({"type": "project", "project": project_view(&p)}).to_string());
                emit(list_event(&state));
                emit(notice("ok", &format!("Imported {}", p.name)));
            }
            Err(e) => emit(err(&e)),
        },
        "projectremove" => match remove_project(&state, id) {
            Ok(()) => {
                emit(json!({"type": "projectremoved", "id": id}).to_string());
                emit(list_event(&state));
            }
            Err(e) => emit(err(&e)),
        },
        "projectscan" => match with_project(&state, id, |p| {
            p.missing = !Path::new(&p.root).is_dir();
            Ok(project_view(p))
        }) {
            Ok(v) => emit(json!({"type": "project", "project": v}).to_string()),
            Err(e) => emit(err(&e)),
        },
        "projectpin" => match set_pins(&state, id, value) {
            Ok(v) => {
                emit(json!({"type": "project", "project": v}).to_string());
                emit(list_event(&state));
            }
            Err(e) => emit(err(&e)),
        },
        "projectmeta" => match set_meta(&state, id, value) {
            Ok(v) => {
                emit(json!({"type": "project", "project": v}).to_string());
                emit(list_event(&state));
            }
            Err(e) => emit(err(&e)),
        },
        "projecttree" => match project_by_id(&state, id) {
            Ok(p) => emit(json!({"type": "projecttree", "id": id, "entries": tree_entries(Path::new(&p.root), 0)}).to_string()),
            Err(e) => emit(err(&e)),
        },
        "projectreveal" => match project_by_id(&state, id) {
            Ok(p) => {
                let path = value.get("path").and_then(|v| v.as_str()).map(|rel| Path::new(&p.root).join(rel)).unwrap_or_else(|| PathBuf::from(&p.root));
                super::reveal(&path);
            }
            Err(e) => emit(err(&e)),
        },
        "projectcompose" => match project_by_id(&state, id) {
            Ok(p) => {
                let rel = value.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let path = safe_join(Path::new(&p.root), rel);
                match path {
                    Ok(path) if path.is_file() => open_compose(path),
                    Ok(_) => emit(err("Compose file not found")),
                    Err(e) => emit(err(&e)),
                }
            }
            Err(e) => emit(err(&e)),
        },
        "projectrun" => {
            let emit2 = emit.clone();
            let state2 = state.clone();
            let id = id.to_owned();
            let value = value.clone();
            std::thread::spawn(move || match run_script(&state2, &id, &value, emit2.clone()) {
                Ok(()) => {}
                Err(e) => emit2(err(&e)),
            });
        }
        "projectcancel" => {
            CANCEL.store(true, Ordering::SeqCst);
            emit(json!({"type": "projectrun", "id": id, "phase": "cancelled"}).to_string());
        }
        "projectbuild" => {
            let emit2 = emit.clone();
            let state2 = state.clone();
            let id = id.to_owned();
            let value = value.clone();
            std::thread::spawn(move || match run_build(&state2, &id, &value, emit2.clone()) {
                Ok(()) => {}
                Err(e) => emit2(err(&e)),
            });
        }
        "projectexport" => match export_kit(&state, id, value) {
            Ok(out) => {
                emit(json!({"type": "projectexported", "id": id, "path": out}).to_string());
                emit(notice("ok", "Kit exported"));
                super::reveal(Path::new(&out));
            }
            Err(e) => emit(err(&e)),
        },
        "projectmkdir" => match mkdir_in(&state, id, value) {
            Ok(()) => {
                emit(json!({"type": "projecttree", "id": id, "entries": tree_for(&state, id)}).to_string());
                emit(refresh_project(&state, id));
            }
            Err(e) => emit(err(&e)),
        },
        "projectrmpath" => match rm_in(&state, id, value) {
            Ok(()) => {
                emit(json!({"type": "projecttree", "id": id, "entries": tree_for(&state, id)}).to_string());
                emit(refresh_project(&state, id));
            }
            Err(e) => emit(err(&e)),
        },
        "projectwrite" => match write_in(&state, id, value) {
            Ok(()) => {
                emit(json!({"type": "projecttree", "id": id, "entries": tree_for(&state, id)}).to_string());
                emit(refresh_project(&state, id));
                emit(notice("ok", "Uploaded"));
            }
            Err(e) => emit(err(&e)),
        },
        "projectcopyin" => match copy_paths_in(&state, id, value) {
            Ok(n) => {
                emit(json!({"type": "projecttree", "id": id, "entries": tree_for(&state, id)}).to_string());
                emit(refresh_project(&state, id));
                emit(notice("ok", &format!("Added {n} item(s)")));
            }
            Err(e) => emit(err(&e)),
        },
        // Tree Copy → Paste: duplicate paths already in this project into a folder.
        "projectpaste" => match paste_within(&state, id, value) {
            Ok(n) => {
                emit(json!({"type": "projecttree", "id": id, "entries": tree_for(&state, id)}).to_string());
                emit(refresh_project(&state, id));
                let _ = n;
                emit(notice("ok", "Pasted"));
            }
            Err(e) => emit(err(&e)),
        },
        "projectread" => match read_file(&state, id, value) {
            Ok((path, text)) => emit(json!({"type": "projectfile", "id": id, "path": path, "text": text}).to_string()),
            Err(e) => emit(err(&e)),
        },
        "projectsave" => match save_file(&state, id, value) {
            Ok(path) => {
                emit(json!({"type": "projectfilesaved", "id": id, "path": path}).to_string());
                emit(notice("ok", "Saved"));
            }
            Err(e) => emit(err(&e)),
        },
        "projectrename" => match rename_in(&state, id, value) {
            Ok(()) => {
                emit(json!({"type": "projecttree", "id": id, "entries": tree_for(&state, id)}).to_string());
                emit(refresh_project(&state, id));
            }
            Err(e) => emit(err(&e)),
        },
        "projectnewfile" => match new_file(&state, id, value) {
            Ok(path) => {
                emit(json!({"type": "projecttree", "id": id, "entries": tree_for(&state, id)}).to_string());
                emit(refresh_project(&state, id));
                emit(json!({"type": "projectfile", "id": id, "path": path, "text": ""}).to_string());
            }
            Err(e) => emit(err(&e)),
        },
        "projectexec" => {
            let emit2 = emit.clone();
            let state2 = state.clone();
            let id = id.to_owned();
            let value = value.clone();
            std::thread::spawn(move || match exec_line(&state2, &id, &value, emit2.clone()) {
                Ok(()) => {}
                Err(e) => emit2(err(&e)),
            });
        }
        _ => {}
    }
    true
}

/// After the folder picker when creating a linked project (dialog already has name/summary/icon).
pub fn add_linked_path(
    state: &Path,
    path: PathBuf,
    name: String,
    summary: String,
    icon: String,
    color: String,
    emit: impl Fn(String),
) {
    let value = json!({
        "source": "linked",
        "root": path.to_string_lossy(),
        "name": if name.is_empty() { path.file_name().and_then(|s| s.to_str()).unwrap_or("project") } else { name.as_str() },
        "summary": summary,
        "icon": icon,
        "color": color,
    });
    match create_project(state, &value) {
        Ok(p) => {
            emit(json!({"type": "project", "project": project_view(&p)}).to_string());
            emit(list_event(state));
            emit(notice("ok", &format!("Created {}", p.name)));
        }
        Err(e) => emit(err(&e)),
    }
}

/// Copy picked files/folders into a project (upload materials).
pub fn copy_picked_into(state: &Path, id: &str, paths: Vec<PathBuf>, into: &str, emit: impl Fn(String)) {
    let value = json!({
        "into": into,
        "paths": paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
    });
    match copy_paths_in(state, id, &value) {
        Ok(n) => {
            emit(json!({"type": "projecttree", "id": id, "entries": tree_for(state, id)}).to_string());
            emit(refresh_project(state, id));
            emit(notice("ok", &format!("Added {n} item(s)")));
        }
        Err(e) => emit(err(&e)),
    }
}

/// After the kit file picker.
pub fn import_kit_path(state: &Path, path: PathBuf, emit: impl Fn(String)) {
    let value = json!({"path": path.to_string_lossy()});
    match import_kit(state, &value) {
        Ok(p) => {
            emit(json!({"type": "project", "project": project_view(&p)}).to_string());
            emit(list_event(state));
            emit(notice("ok", &format!("Imported {}", p.name)));
        }
        Err(e) => emit(err(&e)),
    }
}

/// After the save panel for export.
pub fn export_kit_to(state: &Path, id: &str, dest: PathBuf, emit: impl Fn(String)) {
    let value = json!({"path": dest.to_string_lossy(), "includeContext": false});
    match export_kit(state, id, &value) {
        Ok(out) => {
            emit(json!({"type": "projectexported", "id": id, "path": out}).to_string());
            emit(notice("ok", "Kit exported"));
            super::reveal(Path::new(&out));
        }
        Err(e) => emit(err(&e)),
    }
}

// —— store ——

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Store {
    projects: Vec<Project>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Project {
    id: String,
    name: String,
    /// `workspace` | `linked` | `kit`
    source: String,
    root: String,
    #[serde(default)]
    summary: String,
    /// Phosphor icon key shown on the card (e.g. `briefcase`).
    #[serde(default = "default_icon")]
    icon: String,
    /// Accent colour for the card icon (`#RRGGBB`).
    #[serde(default = "default_color")]
    color: String,
    #[serde(default)]
    pins: Vec<Pin>,
    #[serde(default, skip_serializing)]
    missing: bool,
}

fn default_icon() -> String {
    "briefcase".into()
}

fn default_color() -> String {
    "#84CC16".into()
}

fn sanitize_icon(s: &str) -> String {
    // Keep in sync with PROJ_ICONS in index.html (plus a few aliases still on cards).
    let ok = [
        "briefcase", "cube", "stack", "hammer", "database", "globe", "lightning", "harddrive", "package", "rocket",
        "code", "folder", "network", "gear", "sparkle", "shieldcheck", "bug", "github", "lock", "note", "plug",
        "usb", "circuitry", "broadcast", "terminal", "desktop", "browser", "cloud", "gitbranch", "cpu", "tree",
        "appwindow", "flask", "atom", "robot", "puzzle", "paintbrush", "camera", "music", "gamepad",
        "storefront", "users", "chart", "mappin", "house", "car", "airplane", "leaf", "fire", "drop", "planet",
        "magicwand", "anchor", "coffee", "book", "heart", "star", "flower", "boat", "bike", "palette", "megaphone",
        "bell", "key", "wallet", "shopping", "firstaid", "gradcap", "lightbulb", "fingerprint", "compass", "flag",
        "trophy", "timer", "infinity", "diamond", "circles", "train", "cloudarrow", "buildings", "wrench",
        "calendar", "clock", "confetti", "trendup",
    ];
    if ok.contains(&s) {
        s.to_owned()
    } else {
        default_icon()
    }
}

fn sanitize_color(s: &str) -> String {
    let t = s.trim();
    if t.len() == 7
        && t.starts_with('#')
        && t.as_bytes()[1..].iter().all(|b| b.is_ascii_hexdigit())
    {
        t.to_ascii_uppercase()
    } else {
        default_color()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pin {
    kind: String,
    path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    args: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Target {
    kind: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

fn store_path(state: &Path) -> PathBuf {
    state.join("projects.json")
}

fn kits_dir(state: &Path) -> PathBuf {
    state.join("kits")
}

fn workspaces_dir(state: &Path) -> PathBuf {
    state.join("projects")
}

fn tree_for(state: &Path, id: &str) -> Vec<Value> {
    project_by_id(state, id)
        .map(|p| tree_entries(Path::new(&p.root), 0))
        .unwrap_or_default()
}

fn refresh_project(state: &Path, id: &str) -> String {
    match project_by_id(state, id) {
        Ok(p) => json!({"type": "project", "project": project_view(&p)}).to_string(),
        Err(e) => err(&e),
    }
}

fn load_store(state: &Path) -> Store {
    let path = store_path(state);
    match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or(Store { projects: Vec::new() }),
        Err(_) => Store { projects: Vec::new() },
    }
}

fn save_store(state: &Path, store: &Store) -> Result<(), String> {
    let path = store_path(state);
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    fs::write(&tmp, text).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

fn new_id() -> String {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    format!("p{n:x}")
}

fn list_event(state: &Path) -> String {
    let mut store = load_store(state);
    for p in &mut store.projects {
        p.missing = !Path::new(&p.root).is_dir();
    }
    let projects: Vec<Value> = store.projects.iter().map(project_view).collect();
    json!({"type": "projects", "projects": projects}).to_string()
}

fn project_view(p: &Project) -> Value {
    let root = Path::new(&p.root);
    let targets = if root.is_dir() { scan_targets(root) } else { Vec::new() };
    json!({
        "id": p.id,
        "name": p.name,
        "source": p.source,
        "root": p.root,
        "summary": p.summary,
        "icon": if p.icon.is_empty() { default_icon() } else { p.icon.clone() },
        "color": if p.color.is_empty() { default_color() } else { p.color.clone() },
        "pins": p.pins,
        "missing": p.missing || !root.is_dir(),
        "targets": targets,
    })
}

fn project_by_id(state: &Path, id: &str) -> Result<Project, String> {
    let store = load_store(state);
    store.projects.into_iter().find(|p| p.id == id).ok_or_else(|| "Project not found".into())
}

fn with_project<F>(state: &Path, id: &str, f: F) -> Result<Value, String>
where
    F: FnOnce(&mut Project) -> Result<Value, String>,
{
    let mut store = load_store(state);
    let p = store.projects.iter_mut().find(|p| p.id == id).ok_or_else(|| "Project not found".to_owned())?;
    let v = f(p)?;
    save_store(state, &store)?;
    Ok(v)
}

/// Folder name under `projects/`: the project name itself (Unicode kept), with
/// only path-illegal characters stripped. Names are unique in that directory.
fn workspace_folder_name(name: &str) -> Result<String, String> {
    let s = name.trim();
    if s.is_empty() {
        return Err("Name is required".into());
    }
    if s == "." || s == ".." {
        return Err("Invalid project name".into());
    }
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == '-' || c == ' ' || c == '.').to_owned();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return Err("Invalid project name".into());
    }
    Ok(cleaned)
}

fn create_project(state: &Path, value: &Value) -> Result<Project, String> {
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Name is required".to_owned())?
        .to_owned();
    let summary = value.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let icon = sanitize_icon(value.get("icon").and_then(|v| v.as_str()).unwrap_or(""));
    let color = sanitize_color(value.get("color").and_then(|v| v.as_str()).unwrap_or(""));
    let source = value.get("source").and_then(|v| v.as_str()).unwrap_or("workspace");
    let store = load_store(state);
    if store.projects.iter().any(|p| p.name == name) {
        return Err("A project with that name already exists".into());
    }
    let id = new_id();
    let (source, root) = if source == "linked" {
        let root = value.get("root").and_then(|v| v.as_str()).unwrap_or("").trim();
        if root.is_empty() {
            return Err("Pick a folder".into());
        }
        let path = PathBuf::from(root).canonicalize().map_err(|e| e.to_string())?;
        if !path.is_dir() {
            return Err("Not a directory".into());
        }
        if store.projects.iter().any(|p| Path::new(&p.root) == path) {
            return Err("That folder is already a project".into());
        }
        ("linked".into(), path.to_string_lossy().into_owned())
    } else {
        // Workspace root is ~/.kiri(-dev)/projects/<name>, not a random id.
        let dir_name = workspace_folder_name(&name)?;
        let root = workspaces_dir(state).join(&dir_name);
        if root.exists() || store.projects.iter().any(|p| Path::new(&p.root) == root) {
            return Err("A project with that name already exists".into());
        }
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        ("workspace".into(), root.to_string_lossy().into_owned())
    };
    let p = Project {
        id,
        name,
        source,
        root,
        summary,
        icon,
        color,
        pins: Vec::new(),
        missing: false,
    };
    let mut store = load_store(state);
    store.projects.insert(0, p.clone());
    save_store(state, &store)?;
    Ok(p)
}

fn remove_project(state: &Path, id: &str) -> Result<(), String> {
    let mut store = load_store(state);
    let Some(idx) = store.projects.iter().position(|p| p.id == id) else {
        return Err("Project not found".into());
    };
    let p = store.projects.remove(idx);
    save_store(state, &store)?;
    if p.source == "kit" {
        let kit = kits_dir(state).join(&p.id);
        if kit.is_dir() {
            let _ = fs::remove_dir_all(&kit);
        }
    } else if p.source == "workspace" {
        let root = PathBuf::from(&p.root);
        if root.starts_with(workspaces_dir(state)) && root.is_dir() {
            let _ = fs::remove_dir_all(&root);
        }
    }
    Ok(())
}

fn mkdir_in(state: &Path, id: &str, value: &Value) -> Result<(), String> {
    let p = project_by_id(state, id)?;
    let rel = value.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();
    if rel.is_empty() {
        return Err("Folder name required".into());
    }
    let dest = join_new(Path::new(&p.root), rel)?;
    fs::create_dir_all(&dest).map_err(|e| e.to_string())
}

fn rm_in(state: &Path, id: &str, value: &Value) -> Result<(), String> {
    let p = project_by_id(state, id)?;
    let rel = value.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();
    if rel.is_empty() {
        return Err("Path required".into());
    }
    let path = safe_join(Path::new(&p.root), rel)?;
    if path.is_dir() {
        fs::remove_dir_all(&path).map_err(|e| e.to_string())
    } else {
        fs::remove_file(&path).map_err(|e| e.to_string())
    }
}

fn write_in(state: &Path, id: &str, value: &Value) -> Result<(), String> {
    let p = project_by_id(state, id)?;
    let rel = value.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();
    if rel.is_empty() {
        return Err("Path required".into());
    }
    let b64 = value.get("base64").and_then(|v| v.as_str()).unwrap_or("");
    let bytes = decode_b64(b64)?;
    let dest = join_new(Path::new(&p.root), rel)?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&dest, bytes).map_err(|e| e.to_string())
}

fn copy_paths_in(state: &Path, id: &str, value: &Value) -> Result<usize, String> {
    let p = project_by_id(state, id)?;
    let into = value.get("into").and_then(|v| v.as_str()).unwrap_or("").trim();
    let base = if into.is_empty() {
        PathBuf::from(&p.root)
    } else {
        join_new(Path::new(&p.root), into)?
    };
    fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let paths = value
        .get("paths")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(PathBuf::from)).collect::<Vec<_>>())
        .unwrap_or_default();
    if paths.is_empty() {
        return Err("Nothing to add".into());
    }
    let mut n = 0;
    for src in paths {
        if !src.exists() {
            continue;
        }
        let name = src.file_name().ok_or_else(|| "Invalid path".to_owned())?;
        let dest = base.join(name);
        if src.is_dir() {
            copy_dir(&src, &dest)?;
        } else {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            fs::copy(&src, &dest).map_err(|e| e.to_string())?;
        }
        n += 1;
    }
    Ok(n)
}

/// Paste copied project-relative paths into `into` ("" = project root).
/// Names that already exist become `name copy.ext`, then `name copy 2.ext`, …
fn paste_within(state: &Path, id: &str, value: &Value) -> Result<usize, String> {
    let p = project_by_id(state, id)?;
    let root = Path::new(&p.root);
    let into = value.get("into").and_then(|v| v.as_str()).unwrap_or("").trim();
    let base = if into.is_empty() {
        root.to_path_buf()
    } else {
        let b = safe_join(root, into)?;
        if !b.is_dir() {
            return Err("Paste into a folder".into());
        }
        b
    };
    let from = value
        .get("from")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect::<Vec<_>>())
        .unwrap_or_default();
    if from.is_empty() {
        return Err("Nothing to paste".into());
    }
    let mut n = 0;
    for rel in from {
        let rel = rel.trim().trim_start_matches('/').trim_start_matches('\\');
        if rel.is_empty() {
            continue;
        }
        let src = safe_join(root, rel)?;
        if !src.exists() {
            continue;
        }
        if src.is_dir() && path_is_under(&base, &src) {
            return Err("Can't paste a folder into itself".into());
        }
        let name = src.file_name().ok_or_else(|| "Invalid path".to_owned())?;
        // Same folder as the source: always make a new name (`file copy.ext`), or
        // pasting would land on the original and look like nothing happened.
        // Other folders: keep the name, and only rename when that name is taken.
        let same_folder = src.parent().is_some_and(|p| paths_equal(p, &base));
        let dest = if same_folder {
            unique_copy_name(&base, name)
        } else {
            unique_child(&base, name)
        };
        if paths_equal(&dest, &src) {
            return Err("Can't paste onto the same file".into());
        }
        if src.is_dir() {
            copy_dir(&src, &dest)?;
        } else {
            fs::copy(&src, &dest).map_err(|e| e.to_string())?;
        }
        n += 1;
    }
    if n == 0 {
        return Err("Nothing to paste".into());
    }
    Ok(n)
}

/// Whether `path` is `dir` or lives under it (after canonicalize when possible).
fn path_is_under(path: &Path, dir: &Path) -> bool {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    path == dir || path.starts_with(&dir)
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `file.txt` → `file copy.txt` → `file copy 2.txt` when the name is taken.
fn unique_child(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let dest = dir.join(name);
    if !dest.exists() {
        return dest;
    }
    unique_copy_name(dir, name)
}

/// Always start at `stem copy.ext` (Finder / Explorer style for a same-folder paste).
fn unique_copy_name(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("copy");
    let ext = path.extension().and_then(|s| s.to_str());
    for i in 1..10_000 {
        let label = if i == 1 {
            format!("{stem} copy")
        } else {
            format!("{stem} copy {i}")
        };
        let candidate = match ext {
            Some(e) => dir.join(format!("{label}.{e}")),
            None => dir.join(label),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{}-{}", name.to_string_lossy(), uuid_stub()))
}

fn uuid_stub() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    format!("{t:x}")
}

fn copy_dir(src: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    for ent in fs::read_dir(src).map_err(|e| e.to_string())?.flatten() {
        let path = ent.path();
        let name = ent.file_name();
        let name_s = name.to_string_lossy();
        if SKIP_DIRS.iter().any(|s| name_s.eq_ignore_ascii_case(s)) {
            continue;
        }
        let out = dest.join(&name);
        if path.is_dir() {
            copy_dir(&path, &out)?;
        } else {
            fs::copy(&path, &out).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Join a relative path that may not exist yet (upload / mkdir).
fn join_new(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim_start_matches('/').trim_start_matches('\\');
    for c in Path::new(rel).components() {
        if matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)) {
            return Err("Invalid path".into());
        }
    }
    let joined = root.join(rel);
    let root_c = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    // Ensure destination stays under root even if not created yet.
    let mut check = joined.clone();
    while !check.exists() {
        match check.parent() {
            Some(p) if p != check => check = p.to_path_buf(),
            _ => break,
        }
    }
    let check_c = check.canonicalize().unwrap_or(check);
    if !check_c.starts_with(&root_c) && check_c != root_c {
        return Err("Path outside project".into());
    }
    Ok(joined)
}

fn decode_b64(s: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.len() % 4 != 0 {
        return Err("Invalid upload data".into());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let (a, b, c, d) = (
            val(chunk[0]).ok_or("Invalid upload data")?,
            val(chunk[1]).ok_or("Invalid upload data")?,
            if chunk[2] == b'=' { 0 } else { val(chunk[2]).ok_or("Invalid upload data")? },
            if chunk[3] == b'=' { 0 } else { val(chunk[3]).ok_or("Invalid upload data")? },
        );
        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            out.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            out.push((c << 6) | d);
        }
    }
    Ok(out)
}

fn set_pins(state: &Path, id: &str, value: &Value) -> Result<Value, String> {
    with_project(state, id, |p| {
        let pins: Vec<Pin> = serde_json::from_value(value.get("pins").cloned().unwrap_or(json!([]))).map_err(|e| e.to_string())?;
        p.pins = pins;
        Ok(project_view(p))
    })
}

fn set_meta(state: &Path, id: &str, value: &Value) -> Result<Value, String> {
    let rename = value.get("name").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    if let Some(ref new_name) = rename {
        let store = load_store(state);
        if store.projects.iter().any(|p| p.id != id && p.name == *new_name) {
            return Err("A project with that name already exists".into());
        }
    }
    with_project(state, id, |p| {
        if let Some(n) = rename {
            if p.source == "workspace" && n != p.name {
                let dir_name = workspace_folder_name(&n)?;
                let old_root = PathBuf::from(&p.root);
                let new_root = workspaces_dir(state).join(&dir_name);
                if new_root != old_root {
                    if new_root.exists() {
                        return Err("A project with that name already exists".into());
                    }
                    if old_root.is_dir() && old_root.starts_with(workspaces_dir(state)) {
                        fs::rename(&old_root, &new_root).map_err(|e| e.to_string())?;
                        p.root = new_root.to_string_lossy().into_owned();
                    }
                }
            }
            p.name = n;
        }
        if let Some(s) = value.get("summary").and_then(|v| v.as_str()) {
            p.summary = s.to_owned();
        }
        if let Some(i) = value.get("icon").and_then(|v| v.as_str()) {
            p.icon = sanitize_icon(i);
        }
        if let Some(c) = value.get("color").and_then(|v| v.as_str()) {
            p.color = sanitize_color(c);
        }
        Ok(project_view(p))
    })
}

// —— scan ——

fn scan_targets(root: &Path) -> Vec<Target> {
    let mut out = Vec::new();
    walk(root, root, 0, &mut out);
    out.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.path.cmp(&b.path)));
    out
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Target>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for ent in rd.flatten() {
        let path = ent.path();
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && name != ".env" {
            continue;
        }
        if path.is_dir() {
            if SKIP_DIRS.iter().any(|s| name.eq_ignore_ascii_case(s)) {
                continue;
            }
            walk(root, &path, depth + 1, out);
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        let lower = name.to_ascii_lowercase();
        if is_compose_name(&lower) {
            out.push(Target { kind: "compose".into(), path: rel, hint: None });
        } else if lower == "dockerfile" || lower.starts_with("dockerfile.") {
            let hint = if path.with_file_name("prepare.sh").is_file() {
                Some("Prefer prepare.sh / built.sh when present".into())
            } else {
                None
            };
            out.push(Target { kind: "dockerfile".into(), path: rel, hint });
        } else if lower.ends_with(".sh") && (depth == 0 || matches!(lower.as_str(), "prepare.sh" | "built.sh")) {
            out.push(Target { kind: "script".into(), path: rel, hint: None });
        }
    }
}

fn is_compose_name(name: &str) -> bool {
    if !(name.ends_with(".yml") || name.ends_with(".yaml")) {
        return false;
    }
    name.starts_with("docker-compose") || name.starts_with("compose.") || name == "compose.yml" || name == "compose.yaml"
}

fn tree_entries(dir: &Path, depth: usize) -> Vec<Value> {
    if depth > MAX_DEPTH {
        return Vec::new();
    }
    let Ok(mut ents) = fs::read_dir(dir).map(|rd| rd.flatten().collect::<Vec<_>>()) else {
        return Vec::new();
    };
    ents.sort_by_key(|e| (!e.path().is_dir(), e.file_name()));
    let mut out = Vec::new();
    for ent in ents {
        let path = ent.path();
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() && SKIP_DIRS.iter().any(|s| name.eq_ignore_ascii_case(s)) {
            continue;
        }
        if path.is_dir() {
            out.push(json!({"name": name, "dir": true, "children": tree_entries(&path, depth + 1)}));
        } else {
            out.push(json!({"name": name, "dir": false}));
        }
    }
    out
}

fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim_start_matches('/').trim_start_matches('\\');
    let joined = root.join(rel);
    let root_c = root.canonicalize().map_err(|e| e.to_string())?;
    let full = if joined.exists() {
        joined.canonicalize().map_err(|e| e.to_string())?
    } else {
        // Allow checking non-existent? No — must exist for compose/run.
        return Err("Path not found".into());
    };
    if !full.starts_with(&root_c) {
        return Err("Path outside project".into());
    }
    for c in Path::new(rel).components() {
        if matches!(c, Component::ParentDir) {
            return Err("Invalid path".into());
        }
    }
    Ok(full)
}

// —— run / build ——

static CANCEL: AtomicBool = AtomicBool::new(false);

fn docker_cmd() -> Result<Command, String> {
    let docker = crate::client::docker_cli().ok_or_else(|| "no docker CLI found".to_owned())?;
    let mut cmd = Command::new(docker);
    apply_docker_env(&mut cmd)?;
    Ok(cmd)
}

fn apply_docker_env(cmd: &mut Command) -> Result<(), String> {
    #[cfg(windows)]
    {
        // Same as Compose on Windows: the kiri / kiri-dev docker context.
        cmd.args(["--context", crate::flavor::CONTEXT.get()])
            .env_remove("DOCKER_HOST")
            .env_remove("DOCKER_CONTEXT");
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let sock = crate::client::socket().map_err(|e| e.to_string())?;
        cmd.env("DOCKER_HOST", format!("unix://{}", sock.display()))
            .env_remove("DOCKER_CONTEXT");
        Ok(())
    }
}

fn apply_docker_env_to_shell(cmd: &mut Command) -> Result<(), String> {
    #[cfg(windows)]
    {
        // Child shells inherit DOCKER_CONTEXT pointing at Kiri.
        cmd.env("DOCKER_CONTEXT", crate::flavor::CONTEXT.get()).env_remove("DOCKER_HOST");
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let sock = crate::client::socket().map_err(|e| e.to_string())?;
        cmd.env("DOCKER_HOST", format!("unix://{}", sock.display()))
            .env_remove("DOCKER_CONTEXT");
        Ok(())
    }
}

/// Per-project data root so seed/compose scripts don't clash across projects.
/// `DATA_ROOT=/$PROJECT_SLUG/data` (slug mirrors the workspace folder name).
fn apply_project_data_env(cmd: &mut Command, p: &Project) {
    let slug = workspace_folder_name(&p.name).unwrap_or_else(|_| {
        if p.id.is_empty() {
            "project".into()
        } else {
            p.id.clone()
        }
    });
    let data_root = format!("/{slug}/data");
    cmd.env("PROJECT_SLUG", &slug)
        .env("DATA_ROOT", &data_root)
        .env("DATA_ROOT_DOCKER", &data_root)
        .env("KIRI_PROJECT_ID", &p.id);
}

fn run_script(state: &Path, id: &str, value: &Value, emit: impl Fn(String)) -> Result<(), String> {
    CANCEL.store(false, Ordering::SeqCst);
    let p = project_by_id(state, id)?;
    let rel = value.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let script = safe_join(Path::new(&p.root), rel)?;
    if !script.is_file() {
        return Err("Script not found".into());
    }
    let args = value.get("args").and_then(|v| v.as_str()).unwrap_or("").trim();
    emit(json!({"type": "projectrun", "id": id, "phase": "start", "path": rel}).to_string());

    let mut cmd = script_command(&script, args)?;
    apply_docker_env_to_shell(&mut cmd)?;
    apply_project_data_env(&mut cmd, &p);
    cmd.current_dir(&p.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let emit_out = |stream: &str, chunk: &str| {
        if chunk.is_empty() {
            return;
        }
        emit(json!({"type": "projectrun", "id": id, "phase": "log", "stream": stream, "text": chunk}).to_string());
    };
    let (tx, rx) = std::sync::mpsc::channel::<(String, String)>();
    if let Some(out) = stdout {
        let tx = tx.clone();
        std::thread::spawn(move || pump_lines(out, "stdout", tx));
    }
    if let Some(err) = stderr {
        let tx = tx.clone();
        std::thread::spawn(move || pump_lines(err, "stderr", tx));
    }
    drop(tx);
    while let Ok((stream, text)) = rx.recv() {
        if CANCEL.load(Ordering::SeqCst) {
            let _ = child.kill();
            emit(json!({"type": "projectrun", "id": id, "phase": "cancelled"}).to_string());
            return Ok(());
        }
        emit_out(&stream, &text);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let code = status.code().unwrap_or(-1);
    emit(json!({"type": "projectrun", "id": id, "phase": "done", "code": code}).to_string());
    if status.success() {
        emit(notice("ok", &format!("Finished {rel}")));
    } else {
        emit(notice("error", &format!("{rel} exited {code}")));
    }
    Ok(())
}

fn pump_lines(mut r: impl Read, stream: &str, tx: std::sync::mpsc::Sender<(String, String)>) {
    let mut buf = [0u8; 4096];
    let mut carry = String::new();
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                carry.push_str(&String::from_utf8_lossy(&buf[..n]));
                while let Some(i) = carry.find('\n') {
                    let mut line = carry[..i].to_owned();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                    let _ = tx.send((stream.to_owned(), line + "\n"));
                    carry = carry[i + 1..].to_owned();
                }
                if carry.len() > 8000 {
                    let _ = tx.send((stream.to_owned(), std::mem::take(&mut carry)));
                }
            }
            Err(_) => break,
        }
    }
    if !carry.is_empty() {
        let _ = tx.send((stream.to_owned(), carry));
    }
}

fn script_command(script: &Path, args: &str) -> Result<Command, String> {
    let ext = script.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    #[cfg(windows)]
    {
        if ext == "ps1" {
            let mut cmd = Command::new("powershell");
            cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(script);
            for a in shell_split(args) {
                cmd.arg(a);
            }
            return Ok(cmd);
        }
        if ext == "cmd" || ext == "bat" {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C"]).arg(script);
            for a in shell_split(args) {
                cmd.arg(a);
            }
            return Ok(cmd);
        }
        // .sh: Git Bash / WSL bash when present.
        let bash = ["bash.exe", r"C:\Program Files\Git\bin\bash.exe"]
            .into_iter()
            .find(|b| which_exists(b));
        let Some(bash) = bash else {
            return Err("Shell scripts need Git Bash or bash.exe on Windows".into());
        };
        let mut cmd = Command::new(bash);
        cmd.arg(script);
        for a in shell_split(args) {
            cmd.arg(a);
        }
        return Ok(cmd);
    }
    #[cfg(not(windows))]
    {
        let _ = ext;
        let mut cmd = Command::new("bash");
        cmd.arg(script);
        for a in shell_split(args) {
            cmd.arg(a);
        }
        Ok(cmd)
    }
}

#[cfg(windows)]
fn which_exists(name: &str) -> bool {
    if Path::new(name).is_file() {
        return true;
    }
    Command::new("where").arg(name).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn read_file(state: &Path, id: &str, value: &Value) -> Result<(String, String), String> {
    let p = project_by_id(state, id)?;
    let rel = value.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();
    let path = safe_join(Path::new(&p.root), rel)?;
    if !path.is_file() {
        return Err("Not a file".into());
    }
    let bytes = fs::read(&path).map_err(|e| e.to_string())?;
    if bytes.iter().any(|&b| b == 0) {
        return Err("Binary file — open in Finder / Explorer instead".into());
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok((rel.replace('\\', "/"), text))
}

fn save_file(state: &Path, id: &str, value: &Value) -> Result<String, String> {
    let p = project_by_id(state, id)?;
    let rel = value.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();
    let text = value.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let path = safe_join(Path::new(&p.root), rel)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&path, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(rel.replace('\\', "/"))
}

fn rename_in(state: &Path, id: &str, value: &Value) -> Result<(), String> {
    let p = project_by_id(state, id)?;
    let from = value.get("from").and_then(|v| v.as_str()).unwrap_or("").trim();
    let to = value.get("to").and_then(|v| v.as_str()).unwrap_or("").trim();
    if from.is_empty() || to.is_empty() {
        return Err("Paths required".into());
    }
    let src = safe_join(Path::new(&p.root), from)?;
    let dest = join_new(Path::new(&p.root), to)?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::rename(&src, &dest).map_err(|e| e.to_string())
}

fn new_file(state: &Path, id: &str, value: &Value) -> Result<String, String> {
    let p = project_by_id(state, id)?;
    let rel = value.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();
    if rel.is_empty() {
        return Err("Path required".into());
    }
    let dest = join_new(Path::new(&p.root), rel)?;
    if dest.exists() {
        return Err("Already exists".into());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&dest, b"").map_err(|e| e.to_string())?;
    Ok(rel.replace('\\', "/"))
}

fn exec_line(state: &Path, id: &str, value: &Value, emit: impl Fn(String)) -> Result<(), String> {
    CANCEL.store(false, Ordering::SeqCst);
    let p = project_by_id(state, id)?;
    let line = value.get("line").and_then(|v| v.as_str()).unwrap_or("").trim();
    if line.is_empty() {
        return Ok(());
    }
    // Soft cwd: optional relative subdir under project root.
    let cwd_rel = value.get("cwd").and_then(|v| v.as_str()).unwrap_or("").trim();
    let cwd = if cwd_rel.is_empty() {
        PathBuf::from(&p.root)
    } else {
        let c = safe_join(Path::new(&p.root), cwd_rel)?;
        if !c.is_dir() {
            return Err("cwd is not a directory".into());
        }
        c
    };
    emit(json!({"type": "projectrun", "id": id, "phase": "start", "path": line, "kind": "exec"}).to_string());
    let mut cmd = line_shell(line)?;
    apply_docker_env_to_shell(&mut cmd)?;
    apply_project_data_env(&mut cmd, &p);
    cmd.current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || pump_lines(out, "stdout", tx));
    }
    if let Some(err) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || pump_lines(err, "stderr", tx));
    }
    drop(tx);
    while let Ok((stream, text)) = rx.recv() {
        if CANCEL.load(Ordering::SeqCst) {
            let _ = child.kill();
            emit(json!({"type": "projectrun", "id": id, "phase": "cancelled"}).to_string());
            return Ok(());
        }
        emit(json!({"type": "projectrun", "id": id, "phase": "log", "stream": stream, "text": text}).to_string());
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let code = status.code().unwrap_or(-1);
    emit(json!({"type": "projectrun", "id": id, "phase": "done", "code": code}).to_string());
    Ok(())
}

/// The user's login shell (same idea as Terminal.app / iTerm: `$SHELL`), so
/// aliases like `ll` from `.zshrc` / profile load. Falls back to zsh, then bash.
fn user_shell() -> PathBuf {
    if let Some(s) = std::env::var_os("SHELL") {
        let p = PathBuf::from(s);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    for cand in ["/bin/zsh", "/bin/bash", "/usr/bin/zsh", "/usr/bin/bash"] {
        let p = PathBuf::from(cand);
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("bash")
}

fn line_shell(line: &str) -> Result<Command, String> {
    #[cfg(windows)]
    {
        // Load the user's PowerShell profile (aliases / functions), like a normal console.
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoLogo", "-Command", line]);
        Ok(cmd)
    }
    #[cfg(not(windows))]
    {
        // Source the same rc/profile files a normal terminal would, then eval the
        // line — so aliases like `ll` work. Avoid `bash -i` (ioctl / job-control noise
        // when stdin is not a TTY).
        let shell = user_shell();
        let name = shell.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let mut cmd = Command::new(&shell);
        let prelude = if name.contains("zsh") {
            r#"[ -f "$HOME/.zprofile" ] && . "$HOME/.zprofile"
[ -f "$HOME/.zshrc" ] && . "$HOME/.zshrc"
eval "$1""#
        } else {
            // bash / sh: enable alias expansion (off by default for non-interactive).
            r#"shopt -s expand_aliases 2>/dev/null || true
[ -f "$HOME/.bash_profile" ] && . "$HOME/.bash_profile"
[ -f "$HOME/.profile" ] && . "$HOME/.profile"
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"
eval "$1""#
        };
        cmd.arg("-c").arg(prelude).arg("kiri").arg(line);
        Ok(cmd)
    }
}

fn shell_split(s: &str) -> Vec<String> {
    // Light split: spaces; keep "quoted" chunks.
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q: Option<char> = None;
    for c in s.chars() {
        if let Some(qc) = q {
            if c == qc {
                q = None;
            } else {
                cur.push(c);
            }
        } else if c == '"' || c == '\'' {
            q = Some(c);
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn run_build(state: &Path, id: &str, value: &Value, emit: impl Fn(String)) -> Result<(), String> {
    CANCEL.store(false, Ordering::SeqCst);
    let p = project_by_id(state, id)?;
    let rel = value.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let dockerfile = safe_join(Path::new(&p.root), rel)?;
    let context = dockerfile.parent().unwrap_or(Path::new(&p.root));
    let tag = value
        .get("tag")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let base = context.file_name().and_then(|s| s.to_str()).unwrap_or("app");
            format!("kiri-project/{base}:latest")
        });
    emit(json!({"type": "projectrun", "id": id, "phase": "start", "path": rel, "kind": "build"}).to_string());
    let mut cmd = docker_cmd()?;
    cmd.args(["build", "-f"]).arg(&dockerfile).arg("-t").arg(&tag).arg(context);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || pump_lines(out, "stdout", tx));
    }
    if let Some(err) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || pump_lines(err, "stderr", tx));
    }
    drop(tx);
    while let Ok((stream, text)) = rx.recv() {
        if CANCEL.load(Ordering::SeqCst) {
            let _ = child.kill();
            emit(json!({"type": "projectrun", "id": id, "phase": "cancelled"}).to_string());
            return Ok(());
        }
        emit(json!({"type": "projectrun", "id": id, "phase": "log", "stream": stream, "text": text}).to_string());
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let code = status.code().unwrap_or(-1);
    emit(json!({"type": "projectrun", "id": id, "phase": "done", "code": code}).to_string());
    if status.success() {
        emit(notice("ok", &format!("Built {tag}")));
    } else {
        emit(notice("error", &format!("Build exited {code}")));
    }
    Ok(())
}

// —— kit export / import ——

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KitManifest {
    name: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    pins: Vec<Pin>,
    #[serde(default)]
    kiri_min: String,
    #[serde(default)]
    exclude_note: String,
}

fn export_kit(state: &Path, id: &str, value: &Value) -> Result<String, String> {
    let p = project_by_id(state, id)?;
    let root = Path::new(&p.root);
    if !root.is_dir() {
        return Err("Project folder missing".into());
    }
    let dest = value
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
            home.join("Downloads").join(format!("{}.kirikit", slug(&p.name)))
        });
    if let Some(parent) = dest.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let include_context = value.get("includeContext").and_then(|v| v.as_bool()).unwrap_or(false);
    let manifest = KitManifest {
        name: p.name.clone(),
        summary: p.summary.clone(),
        pins: p.pins.clone(),
        kiri_min: "0.2.0".into(),
        exclude_note: if include_context {
            String::new()
        } else {
            "context/ directories were omitted; run prepare scripts before build if needed.".into()
        },
    };

    let file = File::create(&dest).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("manifest.json", opts).map_err(|e| e.to_string())?;
    zip.write_all(serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?.as_bytes())
        .map_err(|e| e.to_string())?;

    let readme = root.join("README.md");
    if readme.is_file() {
        zip.start_file("README.md", opts).map_err(|e| e.to_string())?;
        zip.write_all(&fs::read(&readme).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    }

    pack_dir(&mut zip, root, root, "files", include_context, opts)?;
    zip.finish().map_err(|e| e.to_string())?;
    Ok(dest.to_string_lossy().into_owned())
}

fn pack_dir(
    zip: &mut zip::ZipWriter<File>,
    root: &Path,
    dir: &Path,
    prefix: &str,
    include_context: bool,
    opts: zip::write::SimpleFileOptions,
) -> Result<(), String> {
    let Ok(rd) = fs::read_dir(dir) else { return Ok(()) };
    for ent in rd.flatten() {
        let path = ent.path();
        let name = ent.file_name();
        let name_s = name.to_string_lossy();
        if name_s.starts_with('.') && name_s != ".env" && name_s != ".editorconfig" && name_s != ".gitattributes" {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel_s = rel.to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            if !include_context && SKIP_DIRS.iter().any(|s| name_s.eq_ignore_ascii_case(s)) {
                continue;
            }
            // Depth guard for pack: skip deep product trees beyond scan depth + 1
            if rel.components().count() > MAX_DEPTH + 2 {
                continue;
            }
            pack_dir(zip, root, &path, prefix, include_context, opts)?;
        } else {
            let entry = format!("{prefix}/{rel_s}");
            zip.start_file(&entry, opts).map_err(|e| e.to_string())?;
            zip.write_all(&fs::read(&path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn import_kit(state: &Path, value: &Value) -> Result<Project, String> {
    let path = value.get("path").and_then(|v| v.as_str()).unwrap_or("");
    if path.is_empty() {
        return Err("Pick a kit file".into());
    }
    let kit_path = PathBuf::from(path);
    if !kit_path.is_file() {
        return Err("Kit file not found".into());
    }
    let id = new_id();
    let dest = kits_dir(state).join(&id);
    fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    let files_root = dest.join("files");
    fs::create_dir_all(&files_root).map_err(|e| e.to_string())?;

    let file = File::open(&kit_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut manifest = KitManifest {
        name: id.clone(),
        summary: String::new(),
        pins: Vec::new(),
        kiri_min: String::new(),
        exclude_note: String::new(),
    };
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = f.name().to_owned();
        if name.ends_with('/') {
            continue;
        }
        // Zip-slip guard
        let out_path = if name == "manifest.json" {
            dest.join("manifest.json")
        } else if name == "README.md" {
            dest.join("README.md")
        } else if let Some(rest) = name.strip_prefix("files/") {
            safe_kit_out(&files_root, rest)?
        } else {
            continue;
        };
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut outfile = File::create(&out_path).map_err(|e| e.to_string())?;
        io::copy(&mut f, &mut outfile).map_err(|e| e.to_string())?;
        if name == "manifest.json" {
            let text = fs::read_to_string(&out_path).map_err(|e| e.to_string())?;
            if let Ok(m) = serde_json::from_str::<KitManifest>(&text) {
                manifest = m;
            }
        }
    }

    // Prefer files/ as root if it has content; else dest.
    let root = if files_root.read_dir().ok().and_then(|mut d| d.next()).is_some() {
        files_root
    } else {
        dest.clone()
    };

    let mut store = load_store(state);
    let p = Project {
        id: id.clone(),
        name: if manifest.name.is_empty() { id } else { manifest.name },
        source: "kit".into(),
        root: root.to_string_lossy().into_owned(),
        summary: manifest.summary,
        icon: default_icon(),
        color: default_color(),
        pins: manifest.pins,
        missing: false,
    };
    store.projects.insert(0, p.clone());
    save_store(state, &store)?;
    Ok(p)
}

fn safe_kit_out(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim_start_matches('/');
    for c in Path::new(rel).components() {
        if matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)) {
            return Err("Invalid kit path".into());
        }
    }
    Ok(root.join(rel))
}

fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_owned();
    if s.is_empty() { "project".into() } else { s }
}

fn err(text: &str) -> String {
    json!({"type": "notice", "kind": "error", "text": text}).to_string()
}

fn notice(kind: &str, text: &str) -> String {
    json!({"type": "notice", "kind": kind, "text": text}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_names() {
        assert!(is_compose_name("docker-compose.yml"));
        assert!(is_compose_name("docker-compose.middleware.linux.yml"));
        assert!(is_compose_name("compose.yaml"));
        assert!(!is_compose_name("version.env"));
    }

    #[test]
    fn unique_child_avoids_existing_names() {
        let dir = tempfile_dir();
        fs::write(dir.join("all.cmd"), "a").unwrap();
        let once = unique_child(&dir, std::ffi::OsStr::new("all.cmd"));
        assert_eq!(once.file_name().unwrap(), "all copy.cmd");
        fs::write(&once, "b").unwrap();
        let twice = unique_child(&dir, std::ffi::OsStr::new("all.cmd"));
        assert_eq!(twice.file_name().unwrap(), "all copy 2.cmd");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn paste_within_duplicates_file_and_refuses_self_folder() {
        let state = tempfile_dir();
        let root = state.join("ws").join("demo");
        fs::create_dir_all(root.join("base")).unwrap();
        fs::write(root.join("all.cmd"), "@echo hi\n").unwrap();
        fs::write(root.join("base").join("x.txt"), "x\n").unwrap();
        let mut store = Store { projects: vec![] };
        store.projects.push(Project {
            id: "p1".into(),
            name: "demo".into(),
            source: "linked".into(),
            root: root.to_string_lossy().into_owned(),
            summary: String::new(),
            icon: default_icon(),
            color: default_color(),
            pins: vec![],
            missing: false,
        });
        save_store(&state, &store).unwrap();

        assert_eq!(paste_within(&state, "p1", &json!({"from": ["all.cmd"], "into": "base"})).unwrap(), 1);
        assert!(root.join("base/all.cmd").is_file());
        // Second paste into another folder gets a unique name.
        assert_eq!(paste_within(&state, "p1", &json!({"from": ["all.cmd"], "into": "base"})).unwrap(), 1);
        assert!(root.join("base/all copy.cmd").is_file());
        // Same folder as the source: always `name copy.ext`, never overwrite the original.
        assert_eq!(paste_within(&state, "p1", &json!({"from": ["all.cmd"], "into": ""})).unwrap(), 1);
        assert!(root.join("all.cmd").is_file());
        assert!(root.join("all copy.cmd").is_file());
        assert_eq!(paste_within(&state, "p1", &json!({"from": ["all.cmd"], "into": ""})).unwrap(), 1);
        assert!(root.join("all copy 2.cmd").is_file());
        // Folder into itself is refused.
        let err = paste_within(&state, "p1", &json!({"from": ["base"], "into": "base"})).unwrap_err();
        assert!(err.contains("itself"), "{err}");
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn scans_szda_like_tree() {
        let dir = tempfile_dir();
        fs::write(dir.join("built.sh"), "#!/bin/bash\n").unwrap();
        fs::write(dir.join("docker-compose.yml"), "services: {}\n").unwrap();
        fs::create_dir(dir.join("thin")).unwrap();
        fs::write(dir.join("thin/Dockerfile"), "FROM scratch\n").unwrap();
        fs::write(dir.join("thin/prepare.sh"), "#!/bin/bash\n").unwrap();
        fs::create_dir(dir.join("thin/context")).unwrap();
        fs::write(dir.join("thin/context/app.jar"), "x").unwrap();
        let t = scan_targets(&dir);
        assert!(t.iter().any(|x| x.kind == "script" && x.path == "built.sh"));
        assert!(t.iter().any(|x| x.kind == "compose" && x.path == "docker-compose.yml"));
        assert!(t.iter().any(|x| x.kind == "dockerfile" && x.path == "thin/Dockerfile"));
        assert!(t.iter().any(|x| x.kind == "script" && x.path == "thin/prepare.sh"));
        assert!(!t.iter().any(|x| x.path.contains("context")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn linked_export_import_roundtrip() {
        let Some(home) = std::env::var_os("HOME") else { return };
        let szda = PathBuf::from(home).join("IdeaProjects/tl/tl-szda-springboot/szda-application/szda-web/src/docker");
        if !szda.is_dir() {
            return;
        }
        let state = tempfile_dir();
        let linked = create_project(
            &state,
            &json!({
                "source": "linked",
                "root": szda.to_string_lossy(),
                "name": "szda-web docker",
                "summary": "先 built.sh，再 apps.sh up"
            }),
        )
        .unwrap();
        assert!(!linked.missing);
        let targets = scan_targets(Path::new(&linked.root));
        assert!(targets.iter().any(|t| t.path == "built.sh"));
        assert!(targets.iter().any(|t| t.path == "apps.sh"));
        assert!(targets.iter().any(|t| t.path.starts_with("docker-compose")));

        set_pins(
            &state,
            &linked.id,
            &json!({
                "pins": [
                    {"kind": "script", "path": "built.sh", "label": "构建应用镜像"},
                    {"kind": "script", "path": "apps.sh", "args": "up", "label": "启动应用"}
                ]
            }),
        )
        .unwrap();

        let kit_path = state.join("out.kirikit");
        let exported = export_kit(
            &state,
            &linked.id,
            &json!({"path": kit_path.to_string_lossy(), "includeContext": false}),
        )
        .unwrap();
        assert!(Path::new(&exported).is_file());

        let imported = import_kit(&state, &json!({"path": exported})).unwrap();
        assert_eq!(imported.source, "kit");
        assert!(Path::new(&imported.root).is_dir());
        assert_eq!(imported.pins.len(), 2);
        assert_eq!(imported.pins[0].path, "built.sh");
        let imported_targets = scan_targets(Path::new(&imported.root));
        assert!(imported_targets.iter().any(|t| t.path == "built.sh"));
        assert!(imported_targets.iter().any(|t| t.path == "apps.sh"));
        assert!(!imported_targets.iter().any(|t| t.path.contains("context")));

        let blank = create_project(&state, &json!({"source": "workspace", "name": "blank", "summary": ""})).unwrap();
        assert_eq!(blank.source, "workspace");
        assert!(Path::new(&blank.root).is_dir());
        assert_eq!(Path::new(&blank.root).file_name().and_then(|s| s.to_str()), Some("blank"));
        write_in(&state, &blank.id, &json!({"path": "hello.sh", "base64": "IyEvYmluL2Jhc2gK"})).unwrap();
        assert!(Path::new(&blank.root).join("hello.sh").is_file());
        let cn = create_project(&state, &json!({"source": "workspace", "name": "产品档案", "summary": ""})).unwrap();
        assert_eq!(Path::new(&cn.root).file_name().and_then(|s| s.to_str()), Some("产品档案"));
        assert!(create_project(&state, &json!({"source": "workspace", "name": "blank", "summary": ""})).is_err());
        let _ = fs::remove_dir_all(&state);
    }

    fn tempfile_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!("kiri-proj-test-{}-{}", new_id(), N.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&p).unwrap();
        p
    }
}
