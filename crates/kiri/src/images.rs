//! `kiri raw pull`, `kiri raw images`, `kiri raw rmi`: the image store, without the daemon.

use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use kiri_image::{Client, Platform, Progress, Reference, Store};

fn store() -> io::Result<Store> {
    Store::open(Store::default_root()?)
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

pub fn pull(args: &[String]) -> io::Result<()> {
    let name = args
        .first()
        .ok_or_else(|| invalid("usage: kiri raw pull <image>"))?;
    let reference = Reference::parse(name).map_err(|e| invalid(e.to_string()))?;
    let store = store()?;
    let platform = Platform::host_linux();
    let started = Instant::now();
    let downloaded = AtomicU64::new(0);
    let cached = AtomicUsize::new(0);
    let image = Client::new()
        .pull(&store, &reference, &platform, &|p| match p {
            Progress::Resolved {
                layers,
                bytes,
                cached,
            } => {
                eprintln!(
                    "{reference}: {layers} layers, {} ({cached} already here)",
                    size(bytes)
                );
            }
            Progress::Layer { .. } | Progress::Downloading { .. } => {}
            Progress::LayerDone {
                bytes,
                cached: was_cached,
                ..
            } => {
                if was_cached {
                    cached.fetch_add(1, Ordering::Relaxed);
                } else {
                    downloaded.fetch_add(bytes, Ordering::Relaxed);
                }
            }
        })
        .map_err(io::Error::other)?;
    let secs = started.elapsed().as_secs_f64();
    let got = downloaded.load(Ordering::Relaxed);
    println!(
        "{} {} — downloaded {} in {:.2} s{}",
        image.name,
        short_digest(&image.digest),
        size(got),
        secs,
        if got > 0 && secs > 0.0 {
            format!(" ({}/s)", size((got as f64 / secs) as u64))
        } else {
            String::new()
        }
    );
    Ok(())
}

pub fn list(_args: &[String]) -> io::Result<()> {
    let mut images = store()?.images()?;
    images.sort_by(|a, b| a.name.cmp(&b.name));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let width = images
        .iter()
        .map(|i| i.name.len())
        .max()
        .unwrap_or(0)
        .max(5);
    println!(
        "{:width$}  {:12}  {:>9}  {:7}  PULLED",
        "IMAGE", "DIGEST", "SIZE", "LAYERS"
    );
    for i in &images {
        println!(
            "{:width$}  {:12}  {:>9}  {:7}  {}",
            i.name,
            short_digest(&i.digest),
            size(i.size),
            i.layers.len(),
            ago(now.saturating_sub(i.pulled_at))
        );
    }
    Ok(())
}

pub fn remove(args: &[String]) -> io::Result<()> {
    if args.is_empty() {
        return Err(invalid("usage: kiri raw rmi <image>..."));
    }
    let store = store()?;
    for name in args {
        let short = Reference::parse(name)
            .map(|r| r.short())
            .unwrap_or_else(|_| name.clone());
        match store.remove_image(&short)? {
            Some(i) => println!("removed {} {}", i.name, short_digest(&i.digest)),
            None => return Err(invalid(format!("no such image: {name}"))),
        }
    }
    Ok(())
}

pub fn short_digest(d: &str) -> &str {
    let hex = d.strip_prefix("sha256:").unwrap_or(d);
    &hex[..hex.len().min(12)]
}

pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn ago(secs: u64) -> String {
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86400),
    }
}
