//! Docker's `filters` query parameter: `{"label":["a=b"],"status":["running"]}`
//! (or the old `{"label":{"a=b":true}}` form). Values of one key are ORed,
//! keys are ANDed, as in Docker.

use serde_json::Value;

#[derive(Debug, Default)]
pub struct Filters(Vec<(String, Vec<String>)>);

impl Filters {
    pub fn parse(raw: Option<&str>) -> Result<Filters, String> {
        let Some(raw) = raw.filter(|r| !r.is_empty()) else {
            return Ok(Filters::default());
        };
        let v: Value = serde_json::from_str(raw).map_err(|e| format!("invalid filters: {e}"))?;
        let Value::Object(map) = v else {
            return Err("invalid filters: not an object".into());
        };
        let mut out = Vec::with_capacity(map.len());
        for (k, v) in map {
            let values = match v {
                Value::Array(a) => a
                    .into_iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect(),
                Value::Object(o) => o
                    .into_iter()
                    .filter(|(_, on)| on.as_bool().unwrap_or(false))
                    .map(|(s, _)| s)
                    .collect(),
                Value::String(s) => vec![s],
                _ => Vec::new(),
            };
            out.push((k, values));
        }
        Ok(Filters(out))
    }

    /// Rejects keys this list does not understand, as Docker does.
    pub fn only(&self, known: &[&str]) -> Result<(), String> {
        match self.0.iter().find(|(k, _)| !known.contains(&k.as_str())) {
            Some((k, _)) => Err(format!("invalid filter '{k}'")),
            None => Ok(()),
        }
    }

    pub fn values(&self, key: &str) -> Option<&[String]> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_slice())
            .filter(|v| !v.is_empty())
    }

    /// True when `key` is absent or any of its values satisfies `f`.
    pub fn any(&self, key: &str, f: impl Fn(&str) -> bool) -> bool {
        self.values(key).is_none_or(|v| v.iter().any(|s| f(s)))
    }

    /// `label=key` or `label=key=value` against a labels object; every label
    /// filter must match (Docker ANDs labels). `label!=` must not match.
    pub fn labels(&self, labels: &Value) -> bool {
        let has = |want: &str| match want.split_once('=') {
            Some((k, v)) => labels.get(k).and_then(Value::as_str) == Some(v),
            None => labels.get(want).is_some(),
        };
        self.values("label")
            .is_none_or(|v| v.iter().all(|w| has(w)))
            && self
                .values("label!")
                .is_none_or(|v| !v.iter().any(|w| has(w)))
    }

    /// The `name` filter, which Docker matches as a regular expression:
    /// clients send `^kind$` to mean exactly "kind" (kind, Compose and
    /// Testcontainers all do). Anchors are honoured; any other pattern is
    /// matched as a substring, which is what an unanchored literal means.
    pub fn name(&self, name: &str) -> bool {
        self.any("name", |p| name_matches(p, name))
    }

    /// `true`/`1` or `false`/`0` for a boolean filter, if given.
    pub fn bool(&self, key: &str) -> Option<bool> {
        self.values(key)
            .and_then(|v| v.first())
            .map(|s| matches!(s.as_str(), "true" | "1"))
    }
}

/// `pattern` against `name` with Docker's `name` filter semantics for the
/// patterns clients use: `^x$` exact, `^x` prefix, `x$` suffix, `x` substring.
/// A leading `/` (container names) is optional on both sides.
pub fn name_matches(pattern: &str, name: &str) -> bool {
    let (start, rest) = match pattern.strip_prefix('^') {
        Some(r) => (true, r),
        None => (false, pattern),
    };
    let (end, lit) = match rest.strip_suffix('$') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let lit = lit.trim_start_matches('/');
    let name = name.trim_start_matches('/');
    match (start, end) {
        (true, true) => name == lit,
        (true, false) => name.starts_with(lit),
        (false, true) => name.ends_with(lit),
        (false, false) => name.contains(lit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_both_forms() {
        let f = Filters::parse(Some(r#"{"label":["a=b"],"status":{"running":true}}"#)).unwrap();
        assert_eq!(f.values("label").unwrap(), ["a=b"]);
        assert_eq!(f.values("status").unwrap(), ["running"]);
        assert!(f.values("name").is_none());
    }

    #[test]
    fn labels_and_together() {
        let f = Filters::parse(Some(r#"{"label":["p=x","s"]}"#)).unwrap();
        assert!(f.labels(&json!({"p":"x","s":"web"})));
        assert!(!f.labels(&json!({"p":"x"})));
        assert!(!f.labels(&json!({"p":"y","s":"web"})));
        assert!(Filters::default().labels(&json!({})));
    }

    #[test]
    fn values_or_together() {
        let f = Filters::parse(Some(r#"{"name":["a","b"]}"#)).unwrap();
        assert!(f.any("name", |n| n == "b"));
        assert!(!f.any("name", |n| n == "c"));
    }

    #[test]
    fn name_filter_is_anchored_like_docker() {
        assert!(name_matches("^kind$", "kind"));
        assert!(!name_matches("^kind$", "kind-2"));
        assert!(!name_matches("^kind$", "mykind"));
        assert!(name_matches("^kind", "kind-2"));
        assert!(name_matches("plane$", "dev-control-plane"));
        assert!(name_matches("ind", "kind"));
        assert!(name_matches("^/web$", "web"));
        assert!(name_matches("^web$", "/web"));
        let f = Filters::parse(Some(r#"{"name":["^kind$"]}"#)).unwrap();
        assert!(f.name("kind") && !f.name("kind-old"));
        assert!(f.any("status", |_| false));
    }

    #[test]
    fn rejects_unknown() {
        let f = Filters::parse(Some(r#"{"bogus":["x"]}"#)).unwrap();
        assert!(f.only(&["label"]).is_err());
        assert!(Filters::parse(Some("[1]")).is_err());
    }
}
