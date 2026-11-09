//! Healthchecks, as Docker runs them: the container's `Config.Healthcheck`
//! merged over the image's, a probe exec'd in the container every interval,
//! and `State.Health` (status, failing streak, the last five results).
//! The engine runs the probes; this module holds the rules.

use std::collections::VecDeque;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Docker's defaults for a field left at 0.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_START_PERIOD: Duration = Duration::ZERO;
pub const DEFAULT_START_INTERVAL: Duration = Duration::from_secs(5);
pub const DEFAULT_RETRIES: u32 = 3;
/// Results kept in `State.Health.Log`.
pub const LOG_ENTRIES: usize = 5;
/// Output kept per result; more is cut and marked with `...`.
pub const OUTPUT_LIMIT: usize = 4096;

pub const STARTING: &str = "starting";
pub const HEALTHY: &str = "healthy";
pub const UNHEALTHY: &str = "unhealthy";

fn zero(n: &i64) -> bool {
    *n == 0
}

/// Docker's `HealthConfig`: durations in nanoseconds, 0 meaning "default"
/// (or "inherit from the image" before merging).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Healthcheck {
    /// `[]` inherit, `["NONE"]` off, `["CMD", args…]`, `["CMD-SHELL", command]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub test: Vec<String>,
    #[serde(default, skip_serializing_if = "zero")]
    pub interval: i64,
    #[serde(default, skip_serializing_if = "zero")]
    pub timeout: i64,
    #[serde(default, skip_serializing_if = "zero")]
    pub start_period: i64,
    #[serde(default, skip_serializing_if = "zero")]
    pub start_interval: i64,
    #[serde(default, skip_serializing_if = "zero")]
    pub retries: i64,
}

impl Healthcheck {
    /// Reads one from the API or an image config; None if absent.
    pub fn from_json(v: &Value) -> Option<Healthcheck> {
        if !v.is_object() {
            return None;
        }
        let test = v["Test"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let n = |k: &str| v[k].as_i64().unwrap_or(0);
        Some(Healthcheck {
            test,
            interval: n("Interval"),
            timeout: n("Timeout"),
            start_period: n("StartPeriod"),
            start_interval: n("StartInterval"),
            retries: n("Retries"),
        })
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Docker's checks at create, with its messages.
    pub fn validate(&self) -> Result<(), String> {
        const MIN: i64 = 1_000_000; // 1ms
        for (name, v) in [
            ("Interval", self.interval),
            ("Timeout", self.timeout),
            ("StartPeriod", self.start_period),
            ("StartInterval", self.start_interval),
        ] {
            if v != 0 && v < MIN {
                return Err(format!("{name} in Healthcheck cannot be less than 1ms"));
            }
        }
        if self.retries < 0 {
            return Err("Retries in Healthcheck cannot be negative".into());
        }
        Ok(())
    }

    /// The command to exec, or None when there is nothing to run (`NONE`,
    /// no test, or a kind Docker does not know either).
    pub fn probe(&self, shell: &[String]) -> Option<Vec<String>> {
        let (kind, rest) = self.test.split_first()?;
        match kind.as_str() {
            "CMD" if !rest.is_empty() => Some(rest.to_vec()),
            "CMD-SHELL" if !rest.is_empty() => {
                let mut argv = if shell.is_empty() {
                    vec!["/bin/sh".to_owned(), "-c".to_owned()]
                } else {
                    shell.to_vec()
                };
                argv.extend(rest.iter().cloned());
                Some(argv)
            }
            _ => None,
        }
    }

    pub fn interval(&self) -> Duration {
        or_default(self.interval, DEFAULT_INTERVAL)
    }

    pub fn timeout(&self) -> Duration {
        or_default(self.timeout, DEFAULT_TIMEOUT)
    }

    pub fn start_period(&self) -> Duration {
        or_default(self.start_period, DEFAULT_START_PERIOD)
    }

    pub fn start_interval(&self) -> Duration {
        or_default(self.start_interval, DEFAULT_START_INTERVAL)
    }

    pub fn retries(&self) -> u32 {
        if self.retries <= 0 {
            DEFAULT_RETRIES
        } else {
            self.retries.min(i64::from(u32::MAX)) as u32
        }
    }
}

fn or_default(ns: i64, default: Duration) -> Duration {
    if ns <= 0 {
        default
    } else {
        Duration::from_nanos(ns as u64)
    }
}

/// Docker's merge of the container's healthcheck over the image's: no
/// container healthcheck takes the image's whole; otherwise each field
/// left empty or 0 comes from the image.
pub fn merge(container: Option<Healthcheck>, image: Option<Healthcheck>) -> Option<Healthcheck> {
    let Some(image) = image else { return container };
    let Some(mut c) = container else {
        return Some(image);
    };
    if c.test.is_empty() {
        c.test = image.test;
    }
    for (mine, theirs) in [
        (&mut c.interval, image.interval),
        (&mut c.timeout, image.timeout),
        (&mut c.start_period, image.start_period),
        (&mut c.start_interval, image.start_interval),
        (&mut c.retries, image.retries),
    ] {
        if *mine == 0 {
            *mine = theirs;
        }
    }
    Some(c)
}

/// One probe's result, as `State.Health.Log` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Probe {
    pub start: String,
    pub end: String,
    pub exit_code: i64,
    pub output: String,
}

/// `State.Health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Health {
    pub status: String,
    pub failing_streak: u32,
    #[serde(default)]
    pub log: VecDeque<Probe>,
}

impl Default for Health {
    fn default() -> Self {
        Health {
            status: STARTING.into(),
            failing_streak: 0,
            log: VecDeque::new(),
        }
    }
}

impl Health {
    /// Docker's `handleProbeResult`: records a result and moves the status.
    /// A failure while starting, inside the start period, does not count.
    /// `since_start` is how long after the container started the probe began.
    pub fn record(&mut self, probe: Probe, since_start: Duration, check: &Healthcheck) {
        while self.log.len() >= LOG_ENTRIES {
            self.log.pop_front();
        }
        let ok = probe.exit_code == 0;
        self.log.push_back(probe);
        if ok {
            self.failing_streak = 0;
            self.status = HEALTHY.into();
            return;
        }
        if self.status == STARTING && since_start < check.start_period() {
            return;
        }
        self.failing_streak += 1;
        if self.failing_streak >= check.retries() {
            self.status = UNHEALTHY.into();
        }
    }

    /// The wait before the next probe: the start interval while starting
    /// inside the start period, the interval otherwise.
    pub fn next_wait(&self, since_start: Duration, check: &Healthcheck) -> Duration {
        if since_start < check.start_period() && self.status == STARTING {
            check.start_interval()
        } else {
            check.interval()
        }
    }

    /// What `docker ps` puts in brackets after "Up …".
    pub fn ps_word(&self) -> String {
        if self.status == STARTING {
            "health: starting".into()
        } else {
            self.status.clone()
        }
    }
}

/// Probe output as Docker keeps it: the first 4 KiB, then `...`.
pub fn cap_output(out: &[u8]) -> String {
    if out.len() <= OUTPUT_LIMIT {
        return String::from_utf8_lossy(out).into_owned();
    }
    let mut s = String::from_utf8_lossy(&out[..OUTPUT_LIMIT]).into_owned();
    s.push_str("...");
    s
}

/// Go's `time.Duration` text, which Docker's timeout message uses: `30s`,
/// `1m30s`, `1.5s`, `500ms`.
pub fn go_duration(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns == 0 {
        return "0s".into();
    }
    let frac = |v: u128, unit: u128| {
        let whole = v / unit;
        let rest = v % unit;
        if rest == 0 {
            whole.to_string()
        } else {
            let digits = unit.to_string().len() - 1;
            let s = format!("{rest:0digits$}");
            format!("{whole}.{}", s.trim_end_matches('0'))
        }
    };
    if ns < 1_000 {
        return format!("{ns}ns");
    }
    if ns < 1_000_000 {
        return format!("{}µs", frac(ns, 1_000));
    }
    if ns < 1_000_000_000 {
        return format!("{}ms", frac(ns, 1_000_000));
    }
    let secs = ns / 1_000_000_000;
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    let s = frac(ns % 60_000_000_000, 1_000_000_000);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, m) => format!("{m}m{s}s"),
        (h, m) => format!("{h}h{m}m{s}s"),
    }
}

/// The `health` filter's values, as Docker accepts them.
pub fn valid_filter(v: &str) -> bool {
    matches!(v, STARTING | HEALTHY | UNHEALTHY | "none")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn probe(code: i64) -> Probe {
        Probe {
            start: String::new(),
            end: String::new(),
            exit_code: code,
            output: String::new(),
        }
    }

    #[test]
    fn reads_the_api_form_and_writes_it_back() {
        let v = json!({"Test": ["CMD-SHELL", "pg_isready"], "Interval": 1_000_000_000i64, "Retries": 5});
        let h = Healthcheck::from_json(&v).unwrap();
        assert_eq!(h.test, ["CMD-SHELL", "pg_isready"]);
        assert_eq!(h.interval(), Duration::from_secs(1));
        assert_eq!(h.timeout(), DEFAULT_TIMEOUT);
        assert_eq!(h.retries(), 5);
        assert_eq!(h.to_json(), v);
        assert!(Healthcheck::from_json(&Value::Null).is_none());
    }

    #[test]
    fn defaults_are_dockers() {
        let h = Healthcheck::default();
        assert_eq!(
            (
                h.interval(),
                h.timeout(),
                h.start_period(),
                h.start_interval(),
                h.retries()
            ),
            (
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::ZERO,
                Duration::from_secs(5),
                3
            )
        );
    }

    #[test]
    fn the_container_overrides_the_image_field_by_field() {
        let image = Healthcheck {
            test: vec!["CMD".into(), "true".into()],
            interval: 5,
            timeout: 6,
            start_period: 7,
            start_interval: 8,
            retries: 9,
        };
        assert_eq!(merge(None, Some(image.clone())), Some(image.clone()));
        let mine = Healthcheck {
            interval: 1,
            ..Default::default()
        };
        let merged = merge(Some(mine), Some(image.clone())).unwrap();
        assert_eq!(merged.test, image.test);
        assert_eq!((merged.interval, merged.timeout, merged.retries), (1, 6, 9));
        let off = merge(
            Some(Healthcheck {
                test: vec!["NONE".into()],
                ..Default::default()
            }),
            Some(image),
        )
        .unwrap();
        assert_eq!(off.probe(&[]), None);
    }

    #[test]
    fn probes_run_as_docker_runs_them() {
        let cmd = Healthcheck {
            test: vec!["CMD".into(), "curl".into(), "-f".into(), "x".into()],
            ..Default::default()
        };
        assert_eq!(cmd.probe(&[]).unwrap(), ["curl", "-f", "x"]);
        let shell = Healthcheck {
            test: vec!["CMD-SHELL".into(), "exit 1".into()],
            ..Default::default()
        };
        assert_eq!(shell.probe(&[]).unwrap(), ["/bin/sh", "-c", "exit 1"]);
        assert_eq!(
            shell.probe(&["/bin/bash".into(), "-ec".into()]).unwrap(),
            ["/bin/bash", "-ec", "exit 1"]
        );
        for off in [
            vec![],
            vec!["NONE".to_owned()],
            vec!["CMD".to_owned()],
            vec!["WHAT".to_owned(), "x".to_owned()],
        ] {
            assert_eq!(
                Healthcheck {
                    test: off,
                    ..Default::default()
                }
                .probe(&[]),
                None
            );
        }
    }

    #[test]
    fn validation_matches_docker() {
        assert!(Healthcheck::default().validate().is_ok());
        assert_eq!(
            Healthcheck {
                interval: 999,
                ..Default::default()
            }
            .validate()
            .unwrap_err(),
            "Interval in Healthcheck cannot be less than 1ms"
        );
        assert_eq!(
            Healthcheck {
                retries: -1,
                ..Default::default()
            }
            .validate()
            .unwrap_err(),
            "Retries in Healthcheck cannot be negative"
        );
    }

    #[test]
    fn status_moves_like_dockers() {
        let check = Healthcheck {
            retries: 2,
            ..Default::default()
        };
        let mut h = Health::default();
        h.record(probe(1), Duration::from_secs(1), &check);
        assert_eq!((h.status.as_str(), h.failing_streak), (STARTING, 1));
        h.record(probe(1), Duration::from_secs(2), &check);
        assert_eq!((h.status.as_str(), h.failing_streak), (UNHEALTHY, 2));
        h.record(probe(0), Duration::from_secs(3), &check);
        assert_eq!((h.status.as_str(), h.failing_streak), (HEALTHY, 0));
        // A timeout (-1) is a failure too.
        h.record(probe(-1), Duration::from_secs(4), &check);
        assert_eq!((h.status.as_str(), h.failing_streak), (HEALTHY, 1));
        for _ in 0..10 {
            h.record(probe(1), Duration::from_secs(5), &check);
        }
        assert_eq!(h.log.len(), LOG_ENTRIES);
        assert_eq!(h.ps_word(), "unhealthy");
        assert_eq!(Health::default().ps_word(), "health: starting");
    }

    #[test]
    fn the_start_period_forgives_failures_until_the_first_success() {
        let check = Healthcheck {
            retries: 1,
            start_period: 10_000_000_000,
            start_interval: 1_000_000_000,
            ..Default::default()
        };
        let mut h = Health::default();
        h.record(probe(1), Duration::from_secs(3), &check);
        assert_eq!((h.status.as_str(), h.failing_streak), (STARTING, 0));
        assert_eq!(
            h.next_wait(Duration::from_secs(3), &check),
            Duration::from_secs(1)
        );
        assert_eq!(
            h.next_wait(Duration::from_secs(11), &check),
            DEFAULT_INTERVAL
        );
        h.record(probe(1), Duration::from_secs(11), &check);
        assert_eq!(h.status, UNHEALTHY);
    }

    #[test]
    fn output_is_capped_at_4_kib() {
        assert_eq!(cap_output(b"ok\n"), "ok\n");
        let long = vec![b'a'; OUTPUT_LIMIT + 10];
        let s = cap_output(&long);
        assert_eq!(s.len(), OUTPUT_LIMIT + 3);
        assert!(s.ends_with("a..."));
    }

    #[test]
    fn durations_print_as_go_does() {
        assert_eq!(go_duration(Duration::from_secs(30)), "30s");
        assert_eq!(go_duration(Duration::from_secs(90)), "1m30s");
        assert_eq!(go_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(go_duration(Duration::from_millis(500)), "500ms");
        assert_eq!(go_duration(Duration::from_secs(3600)), "1h0m0s");
    }
}
