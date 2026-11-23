//! `kiri registry`: daemon.json's registry settings from the terminal, the
//! same lists as Settings → Registries.
//!
//! - `insecure`: registries reached without a verified certificate (a
//!   self-signed one, or plain HTTP), Docker's `insecure-registries`. Only
//!   the hosts and networks listed.
//! - `mirror`: Docker Hub's pull-through mirrors, Docker's
//!   `registry-mirrors`, tried in order before Docker Hub itself.
//!
//! The file is the daemon's (`~/.kiri/daemon.json`), because pulls, pushes
//! and logins happen there, so these commands go through its API as
//! `kiri machine` does. On Windows the whole command runs inside Kiri's WSL
//! distro (crates/kiri/src/wsl.rs), with the daemon's own HOME.

use std::io;

use serde_json::{Value, json};

use kiri_image::{Insecure, Mirror};

use crate::client;

const USAGE: &str = "\
usage: kiri registry                                  both lists and the file they are in
       kiri registry insecure [list] | add HOST... | remove HOST...
                             registries without a verified certificate (self-signed
                             or plain HTTP): registry.lan:5000, 10.0.0.0/8
       kiri registry mirror [list] | add URL... | remove URL...
                             Docker Hub mirrors, tried in order before Docker Hub";

/// Checks an entry and writes it the way daemon.json keeps it.
type Check = fn(&str) -> Result<String, String>;

const INSECURE: &str = "insecure-registries";
const MIRRORS: &str = "registry-mirrors";

pub fn run(args: &[String]) -> io::Result<()> {
    let arg = |i: usize| args.get(i).map(String::as_str);
    if matches!(arg(0), Some("-h" | "--help" | "help")) {
        println!("{USAGE}");
        return Ok(());
    }
    if !client::alive() {
        return Err(io::Error::other("kiri daemon is not running: open Kiri or run `kiri daemon`"));
    }
    let (key, check): (&str, Check) = match arg(0) {
        None => return show(&client::get("/kiri/registries")?),
        Some("insecure") => (INSECURE, |s| Insecure::parse(s).map(|e| e.as_string())),
        Some("mirror" | "mirrors") => (MIRRORS, |s| Mirror::parse(s).map(|m| m.as_url())),
        _ => return Err(io::Error::other(USAGE)),
    };
    let given = || -> io::Result<Vec<String>> {
        let names = &args[2.min(args.len())..];
        if names.is_empty() {
            return Err(io::Error::other(USAGE));
        }
        names.iter().map(|n| check(n).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))).collect()
    };
    match arg(1) {
        Some("list" | "ls") | None => {
            let now = client::get("/kiri/registries")?;
            list(key, &strings(&now[key]));
            problems(&now);
            Ok(())
        }
        Some("add") => {
            let mut now = strings(&client::get("/kiri/registries")?[key]);
            for n in given()? {
                if !now.contains(&n) {
                    now.push(n);
                }
            }
            save(key, &now)
        }
        Some("remove" | "rm") => {
            let mut now = strings(&client::get("/kiri/registries")?[key]);
            for n in given()? {
                let before = now.len();
                now.retain(|e| *e != n);
                if now.len() == before {
                    return Err(io::Error::other(format!("{n} is not in {key}")));
                }
            }
            save(key, &now)
        }
        _ => Err(io::Error::other(USAGE)),
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|s| s.as_str()).map(str::to_owned).collect()
}

fn list(key: &str, entries: &[String]) {
    match (key, entries.is_empty()) {
        (INSECURE, true) => println!("no insecure registries: every registry is reached over verified HTTPS"),
        (INSECURE, false) => println!("insecure registries (no verified certificate; HTTPS first, then plain HTTP):"),
        (_, true) => println!("no registry mirrors: Docker Hub images come from Docker Hub itself"),
        (_, false) => println!("registry mirrors of Docker Hub, in the order a pull tries them:"),
    }
    for e in entries {
        println!("  {e}");
    }
}

fn problems(report: &Value) {
    for p in strings(&report["problems"]) {
        eprintln!("skipped: {p}");
    }
}

fn show(report: &Value) -> io::Result<()> {
    list(INSECURE, &strings(&report[INSECURE]));
    list(MIRRORS, &strings(&report[MIRRORS]));
    if let Some(p) = report["path"].as_str() {
        println!("in {p}");
    }
    problems(report);
    Ok(())
}

fn save(key: &str, entries: &[String]) -> io::Result<()> {
    let r = client::request("POST", "/kiri/registries", Some(&json!({ key: entries })))?;
    if !r.ok() {
        return Err(io::Error::other(r.error()));
    }
    let now = client::get("/kiri/registries")?;
    list(key, &strings(&now[key]));
    Ok(())
}
