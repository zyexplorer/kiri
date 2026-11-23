//! `kiri domains`: the ports of the container-domain routers
//! (`http://<name>.kiri.local`), as Settings → Domains shows and changes
//! them. On Windows it runs in WSL, by the engine's own `kiri` (wsl.rs).

use std::io;

use serde_json::{Value, json};

use crate::client;

const USAGE: &str = "usage: kiri domains                          the routers' ports, and whether each runs
       kiri domains set [--http N] [--https N]   move them (no restart)
       kiri domains reset                    back to the ports a start gives (a test copy's: off)";

#[derive(Debug, PartialEq)]
enum Cmd {
    Show,
    /// A port left out keeps its current one.
    Set { http: Option<u16>, https: Option<u16> },
    Reset,
}

fn port(flag: &str, v: Option<&str>) -> Result<u16, String> {
    let v = v.ok_or_else(|| format!("{flag} needs a port"))?;
    v.parse::<u16>().ok().filter(|&p| p != 0).ok_or_else(|| format!("{flag}: not a port from 1 to 65535: {v}"))
}

fn parse(args: &[String]) -> Result<Cmd, String> {
    match args.first().map(String::as_str) {
        None => Ok(Cmd::Show),
        Some("reset") if args.len() == 1 => Ok(Cmd::Reset),
        Some("set") => {
            let (mut http, mut https) = (None, None);
            let mut it = args[1..].iter().map(String::as_str);
            while let Some(a) = it.next() {
                let (flag, inline) = a.split_once('=').map_or((a, None), |(f, v)| (f, Some(v)));
                let slot = match flag {
                    "--http" => &mut http,
                    "--https" => &mut https,
                    _ => return Err(USAGE.into()),
                };
                *slot = Some(port(flag, inline.or_else(|| it.next()))?);
            }
            if http.is_none() && https.is_none() {
                return Err(USAGE.into());
            }
            Ok(Cmd::Set { http, https })
        }
        _ => Err(USAGE.into()),
    }
}

/// One line per router: its port and state, and the variable that fixes it.
fn format(v: &Value) -> String {
    let mut out = String::new();
    for (name, key) in [("HTTP", "http"), ("HTTPS", "https")] {
        let r = &v[key];
        let port = r["port"].as_u64().map_or_else(|| "?".into(), |p| p.to_string());
        let state = match (r["running"].as_bool(), r["serving"].as_bool()) {
            (Some(true), Some(true)) => "on",
            (Some(true), _) => "on, a container has the port for now",
            _ => "off",
        };
        out += &format!("{name:<6} {port:<6} {state}");
        if let Some(var) = r["env"].as_str() {
            out += &format!(" (set by {var})");
        }
        out.push('\n');
    }
    out
}

/// The engine's answer, or its own error text (exit non-zero).
fn answer(r: client::Response) -> io::Result<Value> {
    if r.ok() { Ok(r.json()) } else { Err(io::Error::other(r.error())) }
}

pub fn run(args: &[String]) -> io::Result<()> {
    let v = match parse(args).map_err(io::Error::other)? {
        Cmd::Show => client::get("/kiri/domains")?,
        Cmd::Set { http, https } => {
            let now = if http.is_none() || https.is_none() { client::get("/kiri/domains")? } else { Value::Null };
            let keep = |k: &str| now[k]["port"].as_u64().unwrap_or(0);
            let body = json!({"http": http.map_or_else(|| keep("http"), u64::from), "https": https.map_or_else(|| keep("https"), u64::from)});
            answer(client::request("POST", "/kiri/domains", Some(&body))?)?
        }
        Cmd::Reset => answer(client::request("DELETE", "/kiri/domains", None)?)?,
    };
    print!("{}", format(&v));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Cmd, format, parse};
    use serde_json::json;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_show_set_and_reset() {
        assert_eq!(parse(&[]), Ok(Cmd::Show));
        assert_eq!(parse(&args("reset")), Ok(Cmd::Reset));
        assert_eq!(parse(&args("set --http 8080 --https 8443")), Ok(Cmd::Set { http: Some(8080), https: Some(8443) }));
        assert_eq!(parse(&args("set --https=8443")), Ok(Cmd::Set { http: None, https: Some(8443) }));
        assert_eq!(parse(&args("set --http 8080")), Ok(Cmd::Set { http: Some(8080), https: None }));
    }

    #[test]
    fn refuses_what_is_not_a_port_or_a_command() {
        assert_eq!(parse(&args("set --http 0")).unwrap_err(), "--http: not a port from 1 to 65535: 0");
        assert_eq!(parse(&args("set --https 70000")).unwrap_err(), "--https: not a port from 1 to 65535: 70000");
        assert_eq!(parse(&args("set --http")).unwrap_err(), "--http needs a port");
        for bad in ["set", "set --port 80", "reset now", "show", "list", "set 8080"] {
            assert!(parse(&args(bad)).unwrap_err().starts_with("usage:"), "{bad}");
        }
    }

    #[test]
    fn not_a_docker_command() {
        assert!(!crate::docker::handles("domains"));
    }

    #[test]
    fn one_line_per_router() {
        let off = json!({"http": {"port": 80, "running": false, "serving": false, "env": null}, "https": {"port": 443, "running": false, "serving": false, "env": null}});
        assert_eq!(format(&off), "HTTP   80     off\nHTTPS  443    off\n");
        let moved = json!({"http": {"port": 18780, "running": true, "serving": false, "env": "KIRI_HTTP_PORT"}, "https": {"port": 18743, "running": true, "serving": true, "env": null}});
        assert_eq!(format(&moved), "HTTP   18780  on, a container has the port for now (set by KIRI_HTTP_PORT)\nHTTPS  18743  on\n");
    }
}
