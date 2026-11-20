//! The window's language: the system's, or the one picked in Settings →
//! Appearance (the `lang` setting: "system", "en", or a file's tag below).
//!
//! The page does its own words (index.html's `tr()`): assemble.rs splices in
//! the English keys and the one language it opens in ([`strings_js`]), with
//! the system's languages and the saved pick from [`boot_js`]. A language the
//! page doesn't have yet it asks for (`langwords`), and the host answers with
//! [`words_js`]: every language in the page would pass WebView2's limit
//! (assemble.rs). The few words drawn natively (the Mac's menus, the Windows
//! tray menu) come from [`tr`], out of the same files, all compiled in.
//! English is the key, so a word missing from a file stays English. The Mac
//! and Linux window and kiri-win (by `#[path]`) share this file.
//!
//! Arabic, Persian and Central Kurdish read right to left ([`rtl`]). The page
//! mirrors itself; the words drawn or sent from here are made ready for it by
//! [`tr`] and [`fill`], so a name or a size keeps its own order inside a
//! right-to-left sentence.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Every translation Kiri ships, by language tag, as lang/<tag>.json maps
/// English to it. English itself has no file.
pub const FILES: [(&str, &str); 37] = [
    ("id", include_str!("lang/id.json")),
    ("ca", include_str!("lang/ca.json")),
    ("cs", include_str!("lang/cs.json")),
    ("de", include_str!("lang/de.json")),
    ("es", include_str!("lang/es.json")),
    ("fr", include_str!("lang/fr.json")),
    ("it", include_str!("lang/it.json")),
    ("lt", include_str!("lang/lt.json")),
    ("hu", include_str!("lang/hu.json")),
    ("nb", include_str!("lang/nb.json")),
    ("pl", include_str!("lang/pl.json")),
    ("pt-BR", include_str!("lang/pt-BR.json")),
    ("ro", include_str!("lang/ro.json")),
    ("sq", include_str!("lang/sq.json")),
    ("sl", include_str!("lang/sl.json")),
    ("fi", include_str!("lang/fi.json")),
    ("vi", include_str!("lang/vi.json")),
    ("tr", include_str!("lang/tr.json")),
    ("el", include_str!("lang/el.json")),
    ("bg", include_str!("lang/bg.json")),
    ("ru", include_str!("lang/ru.json")),
    ("uk", include_str!("lang/uk.json")),
    ("kk", include_str!("lang/kk.json")),
    ("ar", include_str!("lang/ar.json")),
    ("fa", include_str!("lang/fa.json")),
    ("ckb", include_str!("lang/ckb.json")),
    ("am", include_str!("lang/am.json")),
    ("hi", include_str!("lang/hi.json")),
    ("bn", include_str!("lang/bn.json")),
    ("ta", include_str!("lang/ta.json")),
    ("te", include_str!("lang/te.json")),
    ("th", include_str!("lang/th.json")),
    ("lo", include_str!("lang/lo.json")),
    ("ja", include_str!("lang/ja.json")),
    ("ko", include_str!("lang/ko.json")),
    ("zh-Hans", include_str!("lang/zh-Hans.json")),
    ("zh-Hant", include_str!("lang/zh-Hant.json")),
];

/// Every English key of every file, in one order (the pages line each
/// language's words up with it).
fn keys() -> &'static [&'static str] {
    static KEYS: OnceLock<Vec<&'static str>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let set: std::collections::BTreeSet<&'static str> = maps().values().flat_map(|m| m.keys().map(String::as_str)).collect();
        set.into_iter().collect()
    })
}

/// `tag`'s words in the keys' order, a missing word `null`.
fn words(tag: &str) -> Option<serde_json::Value> {
    let m = maps().get(tag)?;
    Some(keys().iter().map(|k| m.get(*k).map_or(serde_json::Value::Null, |w| w.as_str().into())).collect())
}

/// The words a page opens with, as one JS object literal: `{"en": [every
/// English key], "<tag>": [its words, in the keys' order]}`, for the language
/// `lang` (a tag [`resolve`] gave; "en" sends the keys alone). The keys go in
/// once, so a language added later ([`words_js`]) costs only its words.
pub fn strings_js(lang: &str) -> String {
    let mut out = serde_json::Map::new();
    out.insert("en".into(), keys().iter().map(|k| serde_json::Value::from(*k)).collect());
    if let Some(w) = words(lang) {
        out.insert(lang.into(), w);
    }
    serde_json::Value::Object(out).to_string()
}

/// The host's answer to a page's `langwords`: JS that hands the page `tag`'s
/// words, packed as [`strings_js`] packs them, and shows them if `tag` is the
/// language picked. `None` for a tag Kiri has no file for.
pub fn words_js(tag: &str) -> Option<String> {
    let w = words(tag)?;
    Some(format!("window.kiriWords && kiriWords({})", serde_json::json!({ tag: w })))
}

/// Each file parsed, English to its words, by language tag.
fn maps() -> &'static HashMap<&'static str, HashMap<String, String>> {
    static MAPS: OnceLock<HashMap<&'static str, HashMap<String, String>>> = OnceLock::new();
    MAPS.get_or_init(|| FILES.iter().map(|(tag, json)| (*tag, serde_json::from_str(json).unwrap_or_default())).collect())
}

/// A language tag to one Kiri has, as the pages' `langFor` does: Chinese
/// written in Taiwan, Hong Kong and Macao (zh-TW, zh-HK, zh-MO, zh-Hant…)
/// reads Traditional, any other Chinese Simplified; any Portuguese reads
/// Brazilian; `in` is Indonesian's old tag (Java, older Android); any
/// Norwegian (no, nn) reads Bokmål; Kurdish in Arabic letters (Windows'
/// ku-Arab) is Central Kurdish.
fn lang_for(tag: &str) -> Option<&'static str> {
    let t = tag.to_ascii_lowercase().replace('_', "-");
    let is = |id: &str| t == id || t.starts_with(&format!("{id}-"));
    if is("zh") {
        let hant = t.starts_with("zh-hant") || ["zh-tw", "zh-hk", "zh-mo"].iter().any(|r| is(r));
        return Some(if hant && !t.starts_with("zh-hans") { "zh-Hant" } else { "zh-Hans" });
    }
    if is("pt") {
        return Some("pt-BR");
    }
    if is("in") {
        return Some("id");
    }
    if is("no") || is("nn") {
        return Some("nb");
    }
    if is("ku-arab") {
        return Some("ckb");
    }
    if is("en") {
        return Some("en");
    }
    FILES.iter().map(|(id, _)| *id).find(|id| is(&id.to_ascii_lowercase()))
}

/// The language the window shows for the `lang` setting.
pub fn resolve(pick: Option<&str>) -> &'static str {
    match pick {
        Some(p) if !p.is_empty() && p != "system" => lang_for(p).unwrap_or("en"),
        _ => os_languages().iter().find_map(|l| lang_for(l)).unwrap_or("en"),
    }
}

/// Whether the window asks for a language when it opens. Once, on a first
/// start, for everyone; after an update, only for someone whose computer is in
/// a language Kiri now speaks other than English and who never picked one in
/// Settings: the window has just changed language under them, and they may
/// want English back. An English computer has nothing new to choose, and
/// neither does one in a language Kiri doesn't have yet.
pub fn should_ask(fresh: bool, prefs: &serde_json::Value, os: &[String]) -> bool {
    if prefs["langAsked"] == true {
        return false;
    }
    if fresh {
        return true;
    }
    let picked = prefs["lang"].as_str().is_some_and(|l| !l.is_empty() && l != "system");
    !picked && os.iter().find_map(|l| lang_for(l)).is_some_and(|id| id != "en")
}

/// Before the page's script: the system's languages and the saved pick, so
/// the first paint is already in the right language.
pub fn boot_js(pick: Option<&str>) -> String {
    let v = serde_json::json!({ "os": os_languages(), "pick": pick.unwrap_or("system") });
    format!("window.KIRI_LANG = {v};")
}

/// Whether the language `tag` (one of [`FILES`]' or "en") is written right to left.
pub fn rtl(tag: &str) -> bool {
    matches!(tag, "ar" | "fa" | "ckb")
}

/// A letter of a right-to-left script (Hebrew, Arabic and their presentation forms).
fn rtl_letter(c: char) -> bool {
    matches!(c, '\u{0590}'..='\u{08FF}' | '\u{FB1D}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFC}')
}

/// Right-to-left words that open with a Latin name ("docker يستخدم Kiri")
/// would be laid out left to right wherever the text's direction comes from
/// its first letter (the page, a menu, a notification). A right-to-left mark
/// (U+200F, no width) in front says which way the sentence runs. A
/// `{placeholder}` doesn't count as a first letter: [`fill`] makes it an
/// island of its own. The page's rtlReady does the same for its words.
fn rtl_ready(s: String) -> String {
    if !s.chars().any(rtl_letter) {
        return s;
    }
    let mut hole = false;
    let first = s.chars().find(|&c| {
        match c {
            '{' => hole = true,
            '}' => hole = false,
            _ => {}
        }
        !hole && (c.is_alphabetic() || rtl_letter(c))
    });
    match first {
        Some(c) if !rtl_letter(c) => format!("\u{200F}{s}"),
        _ => s,
    }
}

/// `en` in `lang`, for words drawn natively; English when there's no translation.
pub fn tr(lang: &str, en: &str) -> String {
    match maps().get(lang).and_then(|m| m.get(en)) {
        Some(w) if rtl(lang) => rtl_ready(w.clone()),
        Some(w) => w.clone(),
        None => en.to_owned(),
    }
}

/// `{name}` placeholders in `s` filled from `vars`, as the page's tr() does:
/// in one pass, so a value that holds braces (an error's text) stays as it is.
/// In right-to-left words each value is an island (U+2068 … U+2069, a
/// first-strong isolate), so "5.0 GB" or "postgres (localhost:5432)" keeps its
/// own order inside the Arabic around it; a bare number needs none.
pub fn fill(s: String, vars: &[(&str, &dyn std::fmt::Display)]) -> String {
    let islands = s.chars().any(rtl_letter);
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let name = after.find('}').map(|close| &after[..close]).filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        match name.and_then(|n| vars.iter().find(|(k, _)| *k == n)) {
            Some((k, v)) => {
                let v = v.to_string();
                if islands && !v.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | ' ')) {
                    out.push('\u{2068}');
                    out.push_str(&v);
                    out.push('\u{2069}');
                } else {
                    out.push_str(&v);
                }
                rest = &after[k.len() + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The system's preferred languages, most preferred first, as BCP 47 tags.
#[cfg(target_os = "macos")]
pub fn os_languages() -> Vec<String> {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;
    // SAFETY: +[NSLocale preferredLanguages] returns an NSArray of NSString,
    // read with plain count/objectAtIndex: on any thread.
    unsafe {
        let Some(list): Option<Retained<AnyObject>> = msg_send![class!(NSLocale), preferredLanguages] else { return Vec::new() };
        let n: usize = msg_send![&list, count];
        (0..n)
            .filter_map(|i| {
                let s: Option<Retained<NSString>> = msg_send![&list, objectAtIndex: i];
                s.map(|s| s.to_string())
            })
            .collect()
    }
}

/// The system's preferred languages, from Windows' display language list.
#[cfg(windows)]
pub fn os_languages() -> Vec<String> {
    use windows_sys::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
    let (mut count, mut len) = (0u32, 0u32);
    // SAFETY: the first call sizes the buffer, the second fills it; both
    // write only through the pointers given.
    unsafe {
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, std::ptr::null_mut(), &mut len) == 0 || len == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, buf.as_mut_ptr(), &mut len) == 0 {
            return Vec::new();
        }
        buf.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
    }
}

/// The system's preferred languages, from the locale variables, as gettext
/// reads them: LANGUAGE's list first, then LC_ALL, LC_MESSAGES and LANG.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn os_languages() -> Vec<String> {
    from_env(|k| std::env::var(k).ok())
}

#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
fn from_env(var: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut add = |raw: &str| {
        // zh_CN.UTF-8@pinyin → zh-CN; C and POSIX name no language.
        let tag = raw.split(['.', '@']).next().unwrap_or("").replace('_', "-");
        if !tag.is_empty() && tag != "C" && tag != "POSIX" && !out.contains(&tag) {
            out.push(tag);
        }
    };
    if let Some(list) = var("LANGUAGE") {
        list.split(':').for_each(&mut add);
    }
    if let Some(one) = ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|k| var(k).filter(|v| !v.is_empty())) {
        add(&one);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn asks_on_first_start_and_after_an_update_only_when_it_brings_a_language() {
        let os = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let none = serde_json::json!({});
        // A first start asks whatever the computer's language.
        assert!(should_ask(true, &none, &os(&["en-US"])));
        assert!(should_ask(true, &none, &os(&["nl-NL"])));
        // An update on a Spanish, Brazilian or Taiwanese computer asks.
        assert!(should_ask(false, &none, &os(&["es-MX"])));
        assert!(should_ask(false, &serde_json::json!({"lang": "system"}), &os(&["pt-BR"])));
        assert!(should_ask(false, &none, &os(&["zh-TW"])));
        // Dutch first, German second: German is what the window shows, so ask.
        assert!(should_ask(false, &none, &os(&["nl-NL", "de-DE"])));
        // An English computer, or one Kiri can't speak yet, sees nothing new.
        assert!(!should_ask(false, &none, &os(&["en-GB"])));
        assert!(!should_ask(false, &none, &os(&["nl-NL", "he"])));
        // An Arabic computer asks: the window turned right to left under its owner.
        assert!(should_ask(false, &none, &os(&["ar-EG"])));
        assert!(!should_ask(false, &none, &os(&[])));
        // A language picked in Settings, or the step answered, is never asked again.
        assert!(!should_ask(false, &serde_json::json!({"lang": "fr"}), &os(&["es-ES"])));
        assert!(!should_ask(false, &serde_json::json!({"lang": "en"}), &os(&["es-ES"])));
        assert!(!should_ask(false, &serde_json::json!({"langAsked": true}), &os(&["es-ES"])));
        assert!(!should_ask(true, &serde_json::json!({"langAsked": true}), &os(&["en-US"])));
    }

    use super::*;

    #[test]
    fn tags_map_to_the_languages_kiri_has() {
        for (tag, want) in [
            ("zh", Some("zh-Hans")),
            ("zh-CN", Some("zh-Hans")),
            ("zh-SG", Some("zh-Hans")),
            ("zh-Hans", Some("zh-Hans")),
            ("zh-Hans-SG", Some("zh-Hans")),
            ("zh-Hans-HK", Some("zh-Hans")),
            ("zh_CN", Some("zh-Hans")),
            ("zh-TW", Some("zh-Hant")),
            ("zh_TW", Some("zh-Hant")),
            ("zh-HK", Some("zh-Hant")),
            ("zh-MO", Some("zh-Hant")),
            ("zh-Hant", Some("zh-Hant")),
            ("zh-Hant-TW", Some("zh-Hant")),
            ("zh-Hant-CN", Some("zh-Hant")),
            ("zhx", None),
            ("en-GB", Some("en")),
            ("fr-FR", Some("fr")),
            ("fr_CA", Some("fr")),
            ("es-419", Some("es")),
            ("es", Some("es")),
            ("de-AT", Some("de")),
            ("pt-BR", Some("pt-BR")),
            ("pt-PT", Some("pt-BR")),
            ("pt", Some("pt-BR")),
            ("it-IT", Some("it")),
            ("it-CH", Some("it")),
            ("pl-PL", Some("pl")),
            ("ru-RU", Some("ru")),
            ("ru", Some("ru")),
            ("uk", Some("uk")),
            ("uk-UA", Some("uk")),
            ("uk_UA", Some("uk")),
            ("ar", Some("ar")),
            ("ar-SA", Some("ar")),
            ("ar-EG", Some("ar")),
            ("ar_AE", Some("ar")),
            ("arz", None),
            ("hi", Some("hi")),
            ("hi-IN", Some("hi")),
            ("bn-IN", Some("bn")),
            ("bn-BD", Some("bn")),
            ("ta-IN", Some("ta")),
            ("ta-LK", Some("ta")),
            ("ta_IN", Some("ta")),
            ("ca-ES", Some("ca")),
            ("cs-CZ", Some("cs")),
            ("lt-LT", Some("lt")),
            ("hu-HU", Some("hu")),
            ("nb-NO", Some("nb")),
            ("no", Some("nb")),
            ("nn-NO", Some("nb")),
            ("ro-RO", Some("ro")),
            ("sq-AL", Some("sq")),
            ("sl-SI", Some("sl")),
            ("fi-FI", Some("fi")),
            ("el-GR", Some("el")),
            ("bg-BG", Some("bg")),
            ("kk-KZ", Some("kk")),
            ("fa-IR", Some("fa")),
            ("fa", Some("fa")),
            ("ckb", Some("ckb")),
            ("ckb-IQ", Some("ckb")),
            ("ku-Arab-IQ", Some("ckb")),
            ("ku", None),
            ("ku-TR", None),
            ("te-IN", Some("te")),
            ("am", Some("am")),
            ("am-ET", Some("am")),
            ("th-TH", Some("th")),
            ("lo-LA", Some("lo")),
            ("tr-TR", Some("tr")),
            ("ja", Some("ja")),
            ("ja-JP", Some("ja")),
            ("ko-KR", Some("ko")),
            ("vi-VN", Some("vi")),
            ("id-ID", Some("id")),
            ("in", Some("id")),
            ("in-ID", Some("id")),
            ("iw", None),
            ("iw-IL", None),
            ("he", None),
            ("nl", None),
            ("", None),
        ] {
            assert_eq!(lang_for(tag), want, "{tag}");
        }
        assert_eq!(resolve(Some("zh-Hans")), "zh-Hans");
        assert_eq!(resolve(Some("en")), "en");
        assert_eq!(resolve(Some("klingon")), "en");
        assert_eq!(resolve(Some("pt-BR")), "pt-BR");
        assert_eq!(resolve(Some("de")), "de");
        for (tag, _) in FILES {
            assert_eq!(resolve(Some(tag)), tag, "every file's own tag picks it");
        }
    }

    #[test]
    fn locale_variables_read_like_gettext() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned());
        assert_eq!(from_env(env(&[("LANG", "zh_CN.UTF-8")])), ["zh-CN"]);
        assert_eq!(from_env(env(&[("LANGUAGE", "fr:zh_CN"), ("LANG", "en_US.UTF-8")])), ["fr", "zh-CN", "en-US"]);
        assert_eq!(from_env(env(&[("LC_ALL", "C"), ("LANG", "de_DE")])), Vec::<String>::new());
        assert_eq!(from_env(env(&[])), Vec::<String>::new());
    }

    #[test]
    fn files_parse_and_the_native_words_translate() {
        for (tag, json) in FILES {
            let m: HashMap<String, String> = serde_json::from_str(json).unwrap_or_else(|e| panic!("lang/{tag}.json: {e}"));
            assert!(!m.is_empty(), "lang/{tag}.json is empty");
        }
        assert_eq!(tr("zh-Hans", "Settings…"), "设置…");
        for (tag, _) in FILES {
            assert_ne!(tr(tag, "Settings…"), "Settings…", "lang/{tag}.json lacks Settings…");
        }
        assert_eq!(tr("en", "Settings…"), "Settings…");
        assert_eq!(tr("zh-Hans", "no such words"), "no such words");
        // The page opens with the keys and one language; the rest come by words_js,
        // lined up with the same keys.
        let en: serde_json::Value = serde_json::from_str(&strings_js("en")).unwrap();
        assert_eq!(en.as_object().map(|o| o.len()), Some(1), "English sends the keys alone");
        let keys = en["en"].as_array().unwrap();
        let at = keys.iter().position(|k| k == "Settings…").unwrap();
        for (tag, _) in FILES {
            let packed: serde_json::Value = serde_json::from_str(&strings_js(tag)).unwrap();
            assert_eq!(packed.as_object().map(|o| o.len()), Some(2), "{tag}: the keys and its words only");
            assert_eq!(&packed["en"], &en["en"]);
            assert_eq!(packed[tag].as_array().map(Vec::len), Some(keys.len()), "{tag}'s words line up with the keys");
            assert_eq!(packed[tag][at], tr(tag, "Settings…").as_str());
            let js = words_js(tag).unwrap();
            let body = js.strip_prefix("window.kiriWords && kiriWords(").and_then(|r| r.strip_suffix(')')).unwrap();
            let sent: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(sent, serde_json::json!({ tag: packed[tag] }), "{tag}: words_js sends what strings_js would");
        }
        assert_eq!(words_js("en"), None);
        assert_eq!(words_js("klingon"), None);
        assert_eq!(words_js("</script>"), None);
        assert_eq!(fill("Started {name} on {ports}".into(), &[("name", &"web"), ("ports", &"8080")]), "Started web on 8080");
        assert_eq!(fill("{n} of {n}".into(), &[("n", &2)]), "2 of 2");
        assert_eq!(fill("Couldn't stop {name}: {error}".into(), &[("name", &"{error}"), ("error", &"no {such} thing")]), "Couldn't stop {error}: no {such} thing");
        assert_eq!(fill("{missing} and {} and {".into(), &[("n", &1)]), "{missing} and {} and {");
        assert_eq!(fill("宽 {w}，高 {h}".into(), &[("w", &"中"), ("h", &2)]), "宽 中，高 2");
    }

    #[test]
    fn right_to_left_words_keep_names_and_sizes_whole() {
        assert!(rtl("ar"));
        for (tag, _) in FILES {
            assert_eq!(rtl(tag), ["ar", "fa", "ckb"].contains(&tag), "{tag}");
        }
        assert!(!rtl("en"));
        // Arabic first: nothing to add. A Latin name first: a right-to-left mark in front.
        assert_eq!(rtl_ready("الإعدادات…".into()), "الإعدادات…");
        assert_eq!(rtl_ready("Kiri يعمل".into()), "\u{200F}Kiri يعمل");
        // A placeholder isn't the first letter (it's filled as an island), and words with no Arabic stay as they are.
        assert_eq!(rtl_ready("{name} توقف".into()), "{name} توقف");
        assert_eq!(rtl_ready("Kubernetes".into()), "Kubernetes");
        assert_eq!(tr("ar", "Settings…"), "الإعدادات…");
        // Values in an Arabic sentence are islands; bare numbers and other languages' values aren't.
        assert_eq!(fill("تم بدء {name} على {ports}".into(), &[("name", &"web"), ("ports", &8080)]), "تم بدء \u{2068}web\u{2069} على 8080");
        assert_eq!(fill("من {cap}".into(), &[("cap", &"5.0 GB")]), "من \u{2068}5.0 GB\u{2069}");
        assert_eq!(fill("Started {name}".into(), &[("name", &"web")]), "Started web");
    }
}
