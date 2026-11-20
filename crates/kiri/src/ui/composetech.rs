//! What a Compose service is made of, for its logo and role in the window:
//! an off-the-shelf image (Postgres, Redis, nginx…) gets its brand and a
//! role (Database, Cache, Queue…), the way container tiles do; your own code
//! gets its language and framework.
//!
//! In this order, the first that answers:
//! 1. `build:` → the Dockerfile's FROM lines (node:22, golang:1.23, …), then
//!    the folder's package.json, go.mod, pyproject.toml / requirements.txt,
//!    *.csproj, Cargo.toml, pom.xml / build.gradle, Gemfile, composer.json,
//!    which also name the framework.
//! 2. An image → the ENV official base images leave (NODE_VERSION,
//!    GOLANG_VERSION, PYTHON_VERSION…), then its start command (node, python,
//!    dotnet x.dll, java -jar), then the OCI base-image label.
//! 3. A distroless or scratch image → its program's own build info (Go and
//!    Rust write theirs into the binary).
//!
//! Kept per Dockerfile (and its change time) or per image ID, so the window's
//! list costs nothing after the first look.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::collections::HashSet;
use std::sync::{Condvar, LazyLock, Mutex};

use serde_json::{Value, json};

use crate::client;

/// What the engine is told about a service.
pub struct Service {
    pub name: String,
    pub image: String,
    pub image_id: Option<String>,
    /// The build context and Dockerfile (relative to it), for `build:`.
    pub build: Option<(PathBuf, String)>,
    pub command: Vec<String>,
    pub ports: bool,
}

/// What a service is: its brand logo's slug (brands.rs), its language and
/// framework for your own code, its role, and how Kiri knows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tech {
    pub slug: Option<String>,
    pub lang: Option<String>,
    pub fw: Option<String>,
    pub role: String,
    /// Your own code (a build, or an image Kiri reads a language from).
    pub app: bool,
    /// How Kiri knows, in a few words ("Dockerfile FROM node:22 · package.json has next").
    pub how: String,
}

impl Tech {
    pub fn to_json(&self) -> Value {
        json!({"slug": self.slug, "lang": self.lang, "fw": self.fw, "role": self.role, "app": self.app, "how": self.how})
    }
}

/// The language part, cached; the role depends on the service too.
#[derive(Debug, Clone, PartialEq, Default)]
struct Lang {
    slug: String,
    fw: Option<String>,
    how: String,
}

static CACHE: LazyLock<Mutex<HashMap<String, Option<Lang>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Brand slug → what an off-the-shelf image of it does.
const ROLES: &[(&str, &str)] = &[
    ("apachekafka", "Queue"),
    ("apachepulsar", "Queue"),
    ("apachesolr", "Search"),
    ("arangodb", "Database"),
    ("caddy", "Proxy"),
    ("clickhouse", "Database"),
    ("cockroachlabs", "Database"),
    ("consul", "Service"),
    ("couchbase", "Database"),
    ("elasticsearch", "Search"),
    ("envoyproxy", "Proxy"),
    ("etcd", "Database"),
    ("grafana", "Metrics"),
    ("influxdb", "Database"),
    ("kong", "Proxy"),
    ("mariadb", "Database"),
    ("meilisearch", "Search"),
    ("minio", "Storage"),
    ("mongodb", "Database"),
    ("mysql", "Database"),
    ("natsdotio", "Queue"),
    ("neo4j", "Database"),
    ("nginx", "Proxy"),
    ("opensearch", "Search"),
    ("opentelemetry", "Metrics"),
    ("postgresql", "Database"),
    ("prometheus", "Metrics"),
    ("qdrant", "Database"),
    ("rabbitmq", "Queue"),
    ("redis", "Cache"),
    ("surrealdb", "Database"),
    ("timescale", "Database"),
    ("traefikproxy", "Proxy"),
];

/// Brand slugs that are languages: an image of one runs your code.
const LANGS: &[(&str, &str)] = &[
    ("bun", "Bun"),
    ("deno", "Deno"),
    ("dotnet", ".NET"),
    ("elixir", "Elixir"),
    ("go", "Go"),
    ("kotlin", "Kotlin"),
    ("nodedotjs", "Node.js"),
    ("openjdk", "Java"),
    ("php", "PHP"),
    ("python", "Python"),
    ("ruby", "Ruby"),
    ("rust", "Rust"),
];

fn lang_name(slug: &str) -> Option<&'static str> {
    LANGS.iter().find(|(s, _)| *s == slug).map(|(_, n)| *n)
}

/// The brand slug for an image reference, as container tiles find it: the
/// whole repository path, then its last part.
pub fn image_slug(image: &str) -> Option<&'static str> {
    let name = image.split_once('@').map_or(image, |(n, _)| n);
    let repo = match name.rfind(':') {
        Some(i) if !name[i..].contains('/') => &name[..i],
        _ => name,
    };
    let last = repo.rsplit('/').next().unwrap_or(repo);
    let images = super::brands::IMAGES;
    [repo, last].into_iter().find_map(|k| images.binary_search_by_key(&k, |(i, _)| i).ok().map(|i| images[i].1))
}

/// A base image's language: official language images by name, and the
/// vendors' (Microsoft's .NET, Temurin, Corretto…) by path.
pub fn base_lang(image: &str) -> Option<&'static str> {
    let repo = image.split(['@']).next().unwrap_or(image);
    let repo = match repo.rfind(':') {
        Some(i) if !repo[i..].contains('/') => &repo[..i],
        _ => repo,
    };
    let last = repo.rsplit('/').next().unwrap_or(repo);
    if repo.contains("dotnet/") || repo.contains("/dotnet") {
        return Some("dotnet");
    }
    Some(match last {
        "node" => "nodedotjs",
        "golang" | "go" => "go",
        "python" | "pypy" => "python",
        "rust" => "rust",
        "ruby" => "ruby",
        "php" => "php",
        "deno" => "deno",
        "bun" => "bun",
        "elixir" => "elixir",
        "openjdk" | "eclipse-temurin" | "amazoncorretto" | "maven" | "gradle" | "ibmjava" | "sapmachine" => "openjdk",
        _ if repo.ends_with("/java") || last.starts_with("temurin") => "openjdk",
        _ => return image_slug(image).filter(|s| lang_name(s).is_some()),
    })
}

/// The FROM images of a Dockerfile, in order, past `--platform` and `AS`.
pub fn from_lines(dockerfile: &str) -> Vec<String> {
    dockerfile
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            if !w.next()?.eq_ignore_ascii_case("from") {
                return None;
            }
            w.find(|x| !x.starts_with("--")).map(str::to_owned)
        })
        .collect()
}

/// Frameworks a manifest names, first match wins.
fn framework(file: &str, text: &str) -> Option<&'static str> {
    let has = |w: &str| text.contains(w);
    let pick: &[(&str, &str)] = match file {
        "package.json" => &[("\"next\"", "Next.js"), ("\"nuxt\"", "Nuxt"), ("\"@remix-run/", "Remix"), ("\"astro\"", "Astro"), ("\"@sveltejs/kit\"", "SvelteKit"), ("\"@angular/core\"", "Angular"), ("\"@nestjs/core\"", "NestJS"), ("\"vite\"", "Vite"), ("\"fastify\"", "Fastify"), ("\"express\"", "Express")],
        "go.mod" => &[("gin-gonic/gin", "Gin"), ("labstack/echo", "Echo"), ("gofiber/fiber", "Fiber"), ("go-chi/chi", "chi")],
        "pyproject.toml" | "requirements.txt" => &[("celery", "Celery"), ("django", "Django"), ("fastapi", "FastAPI"), ("flask", "Flask")],
        "csproj" => &[("Microsoft.NET.Sdk.Web", "ASP.NET"), ("Microsoft.NET.Sdk.Worker", "Worker Service")],
        "Cargo.toml" => &[("axum", "axum"), ("actix-web", "Actix"), ("rocket", "Rocket")],
        "pom.xml" | "build.gradle" | "build.gradle.kts" => &[("spring-boot", "Spring"), ("quarkus", "Quarkus"), ("micronaut", "Micronaut")],
        "Gemfile" => &[("rails", "Rails"), ("sinatra", "Sinatra")],
        "composer.json" => &[("laravel/framework", "Laravel"), ("symfony/", "Symfony")],
        _ => &[],
    };
    let lower = text.to_ascii_lowercase();
    pick.iter().find(|(w, _)| if file == "csproj" { has(w) } else { lower.contains(&w.to_ascii_lowercase()) }).map(|(_, f)| *f)
}

/// The manifests a build folder may hold, and their language.
const MANIFESTS: &[(&str, &str)] = &[
    ("package.json", "nodedotjs"),
    ("go.mod", "go"),
    ("pyproject.toml", "python"),
    ("requirements.txt", "python"),
    ("Cargo.toml", "rust"),
    ("pom.xml", "openjdk"),
    ("build.gradle", "openjdk"),
    ("build.gradle.kts", "openjdk"),
    ("Gemfile", "ruby"),
    ("composer.json", "php"),
];

/// A build folder: the Dockerfile's FROM lines, then its manifests.
fn from_build(dir: &Path, dockerfile: &str) -> Option<Lang> {
    let df = if Path::new(dockerfile).is_absolute() { PathBuf::from(dockerfile) } else { dir.join(dockerfile) };
    let froms = std::fs::read_to_string(&df).map(|t| from_lines(&t)).unwrap_or_default();
    let from = froms.iter().find_map(|f| base_lang(f).map(|l| (l, f.clone())));
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok().map(|t| (f.to_owned(), t));
    let csproj = std::fs::read_dir(dir).ok().and_then(|d| d.flatten().find(|e| e.file_name().to_string_lossy().ends_with(".csproj"))).and_then(|e| std::fs::read_to_string(e.path()).ok());
    let manifest = MANIFESTS
        .iter()
        .filter(|(_, l)| from.as_ref().is_none_or(|(f, _)| f == l))
        .find_map(|(f, l)| read(f).map(|(name, t)| (*l, name, framework(f, &t))))
        .or_else(|| csproj.filter(|_| from.as_ref().is_none_or(|(f, _)| *f == "dotnet")).map(|t| ("dotnet", "the .csproj".to_owned(), framework("csproj", &t))));
    let mut how = Vec::new();
    if let Some((_, f)) = &from {
        how.push(format!("Dockerfile FROM {f}"));
    }
    let slug = match (&from, &manifest) {
        (Some((l, _)), _) => *l,
        (None, Some((l, _, _))) => *l,
        _ => return None,
    };
    let fw = manifest.as_ref().and_then(|(_, _, fw)| *fw).map(str::to_owned);
    if let Some((_, name, fw)) = &manifest {
        how.push(match fw {
            Some(f) => format!("{name} has {}", f.to_ascii_lowercase()),
            None => format!("has {name}"),
        });
    }
    Some(Lang { slug: slug.to_owned(), fw, how: how.join(" · ") })
}

/// ENV names official base images leave behind.
const ENVS: &[(&str, &str)] = &[
    ("NODE_VERSION", "nodedotjs"),
    ("GOLANG_VERSION", "go"),
    ("PYTHON_VERSION", "python"),
    ("DOTNET_VERSION", "dotnet"),
    ("ASPNET_VERSION", "dotnet"),
    ("ASPNETCORE_HTTP_PORTS", "dotnet"),
    ("JAVA_VERSION", "openjdk"),
    ("JAVA_HOME", "openjdk"),
    ("RUBY_VERSION", "ruby"),
    ("PHP_VERSION", "php"),
    ("RUST_VERSION", "rust"),
    ("BUN_INSTALL", "bun"),
    ("DENO_VERSION", "deno"),
    ("ELIXIR_VERSION", "elixir"),
];

/// Programs a start command runs, and their language.
fn program_lang(argv0: &str) -> Option<&'static str> {
    let b = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0);
    Some(match b {
        "node" | "npm" | "npx" | "yarn" | "pnpm" => "nodedotjs",
        "python" | "python3" | "gunicorn" | "uvicorn" | "celery" | "flask" | "django-admin" => "python",
        "dotnet" => "dotnet",
        "java" => "openjdk",
        "ruby" | "bundle" | "rails" | "puma" => "ruby",
        "php" | "php-fpm" | "apache2-foreground" => "php",
        "deno" => "deno",
        "bun" => "bun",
        _ => return None,
    })
}

/// An image's config: ENV, then its start command, then its base label.
fn from_image_config(cfg: &Value, labels: &Value) -> Option<Lang> {
    let strs = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect::<Vec<_>>()).unwrap_or_default();
    let env = strs(&cfg["Env"]);
    let cmd: Vec<String> = strs(&cfg["Entrypoint"]).into_iter().chain(strs(&cfg["Cmd"])).collect();
    let fw = cmd_framework(&cmd);
    for (k, l) in ENVS {
        if let Some(e) = env.iter().find(|e| e.split('=').next() == Some(*k)) {
            let mut how = format!("image ENV {e}");
            if !cmd.is_empty() {
                how.push_str(&format!(" · CMD {}", short_cmd(&cmd)));
            }
            return Some(Lang { slug: (*l).to_owned(), fw, how });
        }
    }
    if let Some(l) = cmd.iter().find_map(|a| program_lang(a)) {
        return Some(Lang { slug: l.to_owned(), fw, how: format!("CMD {}", short_cmd(&cmd)) });
    }
    let base = labels["org.opencontainers.image.base.name"].as_str().unwrap_or_default();
    base_lang(base).map(|l| Lang { slug: l.to_owned(), fw, how: format!("built on {base}") })
}

fn short_cmd(cmd: &[String]) -> String {
    let s = cmd.iter().take(4).cloned().collect::<Vec<_>>().join(" ");
    if s.len() > 48 { format!("{}…", &s[..s.char_indices().take(47).last().map_or(0, |(i, c)| i + c.len_utf8())]) } else { s }
}

fn cmd_framework(cmd: &[String]) -> Option<String> {
    let line = cmd.join(" ");
    [("celery", "Celery"), ("gunicorn", "Gunicorn"), ("uvicorn", "Uvicorn"), ("rails", "Rails"), ("next start", "Next.js")].iter().find(|(w, _)| line.contains(w)).map(|(_, f)| (*f).to_owned())
}

/// A program's own build info: Go writes `\xff Go buildinf:` and its
/// version; Rust leaves its standard library's paths (`/rustc/<hash>/library`).
fn from_binary(bytes: &[u8]) -> Option<Lang> {
    let find = |needle: &[u8]| bytes.windows(needle.len()).position(|w| w == needle);
    if find(b"\xff Go buildinf:").is_some() {
        let version = find(b"go1.").map(|i| bytes[i..].iter().take(12).take_while(|b| b.is_ascii_alphanumeric() || **b == b'.').map(|&b| b as char).collect::<String>());
        let how = match version {
            Some(v) => format!("Go {}, read from the binary", v.trim_start_matches("go")),
            None => "read from the binary's Go build info".to_owned(),
        };
        return Some(Lang { slug: "go".into(), fw: None, how });
    }
    if find(b"/rustc/").is_some() && find(b"/library/std/").is_some() {
        return Some(Lang { slug: "rust".into(), fw: None, how: "Rust, from the binary's build info".into() });
    }
    None
}

/// How big a program Kiri reads for its build info.
const BINARY_MAX: usize = 128 << 20;

/// A distroless or scratch image's program: copied out of a container
/// that is created and never started, then removed.
fn image_binary(image: &str, cfg: &Value) -> Option<Lang> {
    let first = cfg["Entrypoint"].as_array().and_then(|a| a.first()).or_else(|| cfg["Cmd"].as_array().and_then(|a| a.first()))?.as_str()?.to_owned();
    if !first.starts_with('/') {
        return None;
    }
    // The program is no bigger than its image: a bigger image isn't copied out,
    // so the archive read below never holds more than BINARY_MAX.
    if !client::get_background(&format!("/images/{}/json", super::needs::esc(image))).ok()?["Size"].as_u64().is_some_and(|n| n as usize <= BINARY_MAX) {
        return None;
    }
    let created = client::request("POST", "/containers/create", Some(&json!({"Image": image, "Entrypoint": ["/kiri-never-runs"], "Cmd": [], "Labels": {"io.kiri.probe": "1"}, "NetworkDisabled": true}))).ok()?;
    if !created.ok() {
        return None;
    }
    let id = created.json()["Id"].as_str().unwrap_or_default().to_owned();
    let found = client::request("GET", &format!("/containers/{id}/archive?path={}", super::needs::esc(&first)), None)
        .ok()
        .filter(|r| r.ok() && r.body.len() <= BINARY_MAX)
        .and_then(|r| match super::needs::first_entry(&r.body) {
            Some(super::needs::Entry::File(data)) => from_binary(&data),
            _ => None,
        });
    let _ = client::request("DELETE", &format!("/containers/{id}?force=1&v=1"), None);
    found
}

/// The keys being worked out now: a second asker waits for the first's answer.
static WORKING: LazyLock<(Mutex<HashSet<String>>, Condvar)> = LazyLock::new(|| (Mutex::new(HashSet::new()), Condvar::new()));

/// The cached answer for `key`, else `work`'s, worked out once however many
/// ask at the same time (each window poll asks for every service).
fn cached(key: String, work: impl FnOnce() -> Option<Lang>) -> Option<Lang> {
    let (busy, done) = &*WORKING;
    {
        let mut b = busy.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(v) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
                return v.clone();
            }
            if b.insert(key.clone()) {
                break;
            }
            b = done.wait(b).unwrap_or_else(|e| e.into_inner());
        }
    }
    // Whatever happens in `work` (a panic too), the key is let go and the
    // ones waiting are woken: they work it out themselves or read the cache.
    struct Release(String);
    impl Drop for Release {
        fn drop(&mut self) {
            let (busy, done) = &*WORKING;
            busy.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
            done.notify_all();
        }
    }
    let _release = Release(key.clone());
    let v = work();
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(key, v.clone());
    v
}

/// What a service is made of: an off-the-shelf brand, else its language.
pub fn detect(s: &Service) -> Tech {
    let slug = (!s.image.is_empty()).then(|| image_slug(&s.image)).flatten();
    if let Some(slug) = slug.filter(|sl| s.build.is_none() && lang_name(sl).is_none()) {
        let role = ROLES.iter().find(|(k, _)| *k == slug).map_or("Service", |(_, r)| r);
        return Tech { slug: Some(slug.to_owned()), lang: None, fw: None, role: role.to_owned(), app: false, how: format!("the image is {}", repo_of(&s.image)) };
    }
    // Pulled, not built, and no brand Kiri knows: an image, not your code.
    if s.build.is_none() && slug.is_none() {
        return Tech { slug: None, lang: None, fw: None, role: "Image".into(), app: false, how: if s.image.is_empty() { String::new() } else { format!("the image is {}", repo_of(&s.image)) } };
    }
    let lang = if let Some((dir, df)) = &s.build {
        // The Dockerfile's and the manifests' change times: an edit to either is read again.
        let t = |f: &Path| std::fs::metadata(f).and_then(|m| m.modified()).ok();
        let stamps: Vec<_> = std::iter::once(df.as_str()).chain(MANIFESTS.iter().map(|(f, _)| *f)).map(|f| t(&dir.join(f))).collect();
        cached(format!("build\0{}\0{df}\0{stamps:?}", dir.display()), || from_build(dir, df))
    } else {
        None
    };
    let lang = lang.or_else(|| {
        // A build-only service has no image name to look up until it's built.
        let id = s.image_id.clone().or_else(|| (!s.image.is_empty()).then(|| client::get_background(&format!("/images/{}/json", super::needs::esc(&s.image))).ok().and_then(|v| v["Id"].as_str().map(str::to_owned))).flatten())?;
        cached(format!("image\0{id}"), || {
            let info = client::get_background(&format!("/images/{}/json", super::needs::esc(&id))).ok()?;
            from_image_config(&info["Config"], &info["Config"]["Labels"]).or_else(|| image_binary(&id, &info["Config"]))
        })
    });
    let role = app_role(s, lang.as_ref().and_then(|l| l.fw.as_deref()));
    match lang {
        Some(l) => Tech { lang: lang_name(&l.slug).map(str::to_owned), slug: Some(l.slug), fw: l.fw, role, app: true, how: l.how },
        None => Tech { slug: None, lang: None, fw: None, role, app: true, how: String::new() },
    }
}

fn repo_of(image: &str) -> &str {
    let name = image.split_once('@').map_or(image, |(n, _)| n);
    match name.rfind(':') {
        Some(i) if !name[i..].contains('/') => &name[..i],
        _ => name,
    }
}

/// Your own code's role: by its name and command, then its framework and ports.
pub fn app_role(s: &Service, fw: Option<&str>) -> String {
    let text = format!("{} {}", s.name, s.command.join(" ")).to_ascii_lowercase();
    let r = if ["scheduler", "cron", "beat"].iter().any(|w| text.contains(w)) {
        "Scheduler"
    } else if ["worker", "consumer", "job", "queue"].iter().any(|w| text.contains(w)) || fw == Some("Celery") || fw == Some("Worker Service") {
        "Worker"
    } else if fw.is_some_and(|f| ["Next.js", "Nuxt", "Remix", "Astro", "SvelteKit", "Angular", "Vite"].contains(&f)) || ["web", "frontend", "ui", "site", "admin"].iter().any(|w| s.name.to_ascii_lowercase() == *w || s.name.to_ascii_lowercase().ends_with(&format!("-{w}"))) {
        "Web"
    } else if s.ports {
        "API"
    } else {
        "Service"
    };
    r.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, image: &str) -> Service {
        Service { name: name.into(), image: image.into(), image_id: None, build: None, command: Vec::new(), ports: false }
    }

    #[test]
    fn off_the_shelf_images_get_their_brand_and_role() {
        let t = detect(&svc("db", "postgres:16"));
        assert_eq!((t.slug.as_deref(), t.role.as_str(), t.app), (Some("postgresql"), "Database", false));
        assert_eq!(detect(&svc("cache", "redis:7-alpine")).role, "Cache");
        assert_eq!(detect(&svc("proxy", "nginx")).role, "Proxy");
    }

    #[test]
    fn pulled_images_without_a_brand_are_images_not_your_code() {
        for image in ["busybox", "gcr.io/distroless/static:nonroot", "axllent/mailpit:latest"] {
            let t = detect(&svc("x", image));
            assert_eq!((t.app, t.lang.as_deref(), t.role.as_str(), t.slug.as_deref()), (false, None, "Image", None), "{image}");
        }
        assert_eq!(detect(&svc("proxy", "traefik:v3")).slug.as_deref(), Some("traefikproxy"), "a Go program with a brand keeps it");
    }

    #[test]
    fn asks_at_once_work_it_out_once() {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let key = format!("test\0{}", std::process::id());
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let key = key.clone();
                std::thread::spawn(move || {
                    cached(key, || {
                        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        None
                    })
                })
            })
            .collect();
        for t in threads {
            let _ = t.join();
        }
        assert_eq!(N.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn reads_base_images() {
        assert_eq!(base_lang("node:22-alpine"), Some("nodedotjs"));
        assert_eq!(base_lang("golang:1.23"), Some("go"));
        assert_eq!(base_lang("mcr.microsoft.com/dotnet/sdk:8.0"), Some("dotnet"));
        assert_eq!(base_lang("eclipse-temurin:21-jre"), Some("openjdk"));
        assert_eq!(base_lang("gcr.io/distroless/static"), None);
        assert_eq!(from_lines("# x\nFROM --platform=$B golang:1.23 AS build\nRUN go build\nfrom gcr.io/distroless/static\n"), ["golang:1.23", "gcr.io/distroless/static"]);
    }

    #[test]
    fn reads_a_build_folder() {
        let dir = std::env::temp_dir().join(format!("kiri-composetech-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Dockerfile"), "FROM node:22-alpine\nCOPY . .\n").unwrap();
        std::fs::write(dir.join("package.json"), r#"{"dependencies": {"next": "15", "react": "19"}}"#).unwrap();
        let l = from_build(&dir, "Dockerfile").unwrap();
        assert_eq!((l.slug.as_str(), l.fw.as_deref()), ("nodedotjs", Some("Next.js")));
        assert_eq!(l.how, "Dockerfile FROM node:22-alpine · package.json has next.js");
        std::fs::remove_file(dir.join("Dockerfile")).unwrap();
        std::fs::remove_file(dir.join("package.json")).unwrap();
        std::fs::write(dir.join("Api.csproj"), r#"<Project Sdk="Microsoft.NET.Sdk.Web">"#).unwrap();
        let l = from_build(&dir, "Dockerfile").unwrap();
        assert_eq!((l.slug.as_str(), l.fw.as_deref()), ("dotnet", Some("ASP.NET")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_an_image_config() {
        let cfg = json!({"Env": ["PATH=/usr/bin", "DOTNET_VERSION=8.0.11"], "Entrypoint": ["dotnet", "Platform.Api.dll"]});
        let l = from_image_config(&cfg, &Value::Null).unwrap();
        assert_eq!((l.slug.as_str(), l.how.as_str()), ("dotnet", "image ENV DOTNET_VERSION=8.0.11 · CMD dotnet Platform.Api.dll"));
        let cfg = json!({"Cmd": ["celery", "-A", "notify", "worker"]});
        let l = from_image_config(&cfg, &Value::Null).unwrap();
        assert_eq!((l.slug.as_str(), l.fw.as_deref()), ("python", Some("Celery")));
        let l = from_image_config(&json!({}), &json!({"org.opencontainers.image.base.name": "docker.io/library/ruby:3.3"})).unwrap();
        assert_eq!(l.slug, "ruby");
    }

    #[test]
    fn reads_build_info_from_binaries() {
        let mut go = b"\x7fELF....".to_vec();
        go.extend_from_slice(b"\xff Go buildinf:\x08\x02....go1.23.4\x00");
        assert_eq!(from_binary(&go).unwrap().how, "Go 1.23.4, read from the binary");
        assert_eq!(from_binary(b"/rustc/90b35a6239c3d8bdabc530a6a0816f7ff89a0aaf/library/std/src/panicking.rs").unwrap().slug, "rust");
        assert!(from_binary(b"plain C program").is_none());
    }

    #[test]
    fn guesses_your_codes_role() {
        let mut s = svc("notify", "x");
        assert_eq!(app_role(&s, Some("Celery")), "Worker");
        s.name = "cron".into();
        assert_eq!(app_role(&s, None), "Scheduler");
        s.name = "web".into();
        assert_eq!(app_role(&s, None), "Web");
        s.name = "api".into();
        s.ports = true;
        assert_eq!(app_role(&s, None), "API");
    }

    #[test]
    fn a_panic_while_working_lets_the_next_asker_in() {
        let key = "panics-once".to_owned();
        let k = key.clone();
        let r = std::thread::spawn(move || cached(k, || panic!("boom"))).join();
        assert!(r.is_err());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let v = cached(key, || Some(Lang { slug: "go".into(), fw: None, how: String::new() }));
            let _ = tx.send(v.map(|l| l.slug));
        });
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap().as_deref(), Some("go"), "the second asker doesn't wait forever");
    }
}
