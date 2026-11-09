//! Docker-style generated container names: `adjective_scientist`.

const ADJECTIVES: &[&str] = &[
    "agile", "bold", "brave", "bright", "calm", "clever", "cosmic", "crisp", "daring", "eager",
    "fast", "fearless", "gentle", "happy", "jolly", "keen", "kind", "lively", "lucid", "mighty",
    "nimble", "noble", "quick", "quiet", "rapid", "serene", "sharp", "snappy", "steady", "swift",
    "tidy", "vivid", "wise", "zesty",
];

const NAMES: &[&str] = &[
    "achebe",
    "adichie",
    "babbage",
    "curie",
    "dijkstra",
    "einstein",
    "emeagwali",
    "faraday",
    "feynman",
    "franklin",
    "galileo",
    "hamilton",
    "hopper",
    "johnson",
    "kalam",
    "knuth",
    "lamarr",
    "lovelace",
    "maathai",
    "mandela",
    "meitner",
    "noether",
    "okeke",
    "pasteur",
    "ramanujan",
    "ritchie",
    "shannon",
    "soyinka",
    "tesla",
    "thompson",
    "torvalds",
    "turing",
    "wozniak",
    "yalow",
];

pub fn random() -> String {
    let r = super::engine::random_hex(4);
    let n = u32::from_str_radix(&r, 16).unwrap_or(0) as usize;
    format!(
        "{}_{}",
        ADJECTIVES[n % ADJECTIVES.len()],
        NAMES[(n / ADJECTIVES.len()) % NAMES.len()]
    )
}
