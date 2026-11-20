//! The window's page as every system loads it: index.html with its icons
//! and the terminal emulator (xterm.js, vendored unmodified in `xterm/`)
//! spliced into their placeholders. The Mac and Linux window (crates/kiri)
//! and Windows (kiri-win includes this file) load the same string, and
//! scripts/ui-test.py builds it the same way.
//!
//! Its words: the English keys and the one language it opens in (the
//! `lang` setting's, or the system's for System; `lang::strings_js`), and the
//! system's languages and the saved pick (`lang::boot_js`). Another language
//! comes when the page switches to it: the page sends `langwords` and the
//! host evaluates `lang::words_js`.
//!
//! WKWebView and WebKitGTK load the page from memory as a string, and
//! WebView2 from memory at an address of its own (kiri-win's page.rs), so no
//! system limits its size. With xterm.js and the keys it is about 1.5 M
//! UTF-16 units, and a language adds its words: 40 to 60 K units. All of
//! them would add some 2 M units to every start, which is why the page
//! carries only one.

const ICONS_JS: &str = include_str!("icons.js");
/// The languages and the first start's language step, which both pages show.
const LANGSTEP_JS: &str = include_str!("langstep.js");
const XTERM_CSS: &str = include_str!("xterm/xterm.css");
/// xterm.js 6.0.0 and its fit, search and web-links add-ons (UMD builds,
/// each defining a global), in the order they load.
const XTERM_JS: [&str; 4] = [include_str!("xterm/xterm.js"), include_str!("xterm/addon-fit.js"), include_str!("xterm/addon-search.js"), include_str!("xterm/addon-web-links.js")];

/// `index` (index.html) with everything spliced in, `lang` (the `lang`
/// setting) picking its language.
pub fn page(index: &str, lang: Option<&str>) -> String {
    let mut js = String::with_capacity(XTERM_JS.iter().map(|s| s.len() + 1).sum());
    for part in XTERM_JS {
        js.push_str(part);
        // Each ends in a source-map comment: the next must start on a line of its own.
        js.push('\n');
    }
    without_indent(&without_js_comment_lines(&without_css_comments(index)))
        .replacen("/*ICONS*/", ICONS_JS, 1)
        .replacen("/*LANGSTEP*/", LANGSTEP_JS, 1)
        .replacen("/*XTERM_CSS*/", XTERM_CSS, 1)
        .replacen("/*XTERM_JS*/", &js, 1)
        .replacen("/*LANG*/", &super::lang::boot_js(lang), 1)
        .replacen("/*STRINGS*/{}", &super::lang::strings_js(super::lang::resolve(lang)), 1)
}

/// Where the page's own stylesheet is: its plain `<style>` (one with
/// attributes, `<style media=…>`, isn't it) to the `</style>` after it.
fn style_span(index: &str) -> Option<(usize, usize)> {
    let start = index.find("<style>")?;
    let end = index[start..].find("</style>")? + start;
    Some((start, end))
}

/// Where a tag that starts with `name` (`<script`, `</script`) is in `text`
/// from `from`, as HTML reads it: in any case, followed by a space, `/` or `>`.
fn find_tag(text: &str, name: &str, from: usize) -> Option<usize> {
    let lower = text.as_bytes();
    let n = name.len();
    (from..text.len().saturating_sub(n - 1)).find(|&i| lower[i..i + n].eq_ignore_ascii_case(name.as_bytes()) && lower.get(i + n).is_some_and(|c| c.is_ascii_whitespace() || *c == b'/' || *c == b'>'))
}

/// The page's own stylesheet (its plain `<style>`) without its comments,
/// which are for whoever reads index.html, not for every start to load:
/// the splice points (`/*XTERM_CSS*/`) stay.
fn without_css_comments(index: &str) -> String {
    let Some((start, end)) = style_span(index) else { return index.to_owned() };
    let mut out = String::with_capacity(index.len());
    out.push_str(&index[..start]);
    let mut css = &index[start..end];
    while let Some(i) = css.find("/*") {
        let Some(j) = css[i + 2..].find("*/").map(|j| i + 2 + j + 2) else { break };
        let comment = &css[i..j];
        out.push_str(&tight_css(&css[..i]));
        if comment[2..comment.len() - 2].bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
            out.push_str(comment);
        }
        css = &css[j..];
    }
    out.push_str(&tight_css(css));
    out.push_str(&index[end..]);
    out
}

/// The stylesheet without the space after `;` and `{` or before `{`, outside
/// its strings: about 12 K units of the page. An escaped character (`\{`,
/// `\"`) is copied as it is, and so is a custom property's value
/// (`--name: …;`), whose spaces may matter to what reads it.
fn tight_css(css: &str) -> String {
    let c: Vec<char> = css.chars().collect();
    let mut out = String::with_capacity(css.len());
    // `decl`: a declaration may start here (at the start, after `{` or `;`, spaces aside).
    let (mut quote, mut i, mut decl) = (None, 0, true);
    while i < c.len() {
        if let Some(end) = custom_value(&c, i).filter(|_| quote.is_none() && decl) {
            out.extend(&c[i..end]);
            (i, decl) = (end, false);
            continue;
        }
        let ch = c[i];
        out.push(ch);
        i += 1;
        if !CSS_SPACE.contains(&ch) {
            decl = quote.is_none() && matches!(ch, '{' | ';');
        }
        match (quote, ch) {
            (_, '\\') => {
                out.extend(c.get(i));
                (i, decl) = (i + 1, false);
            }
            (Some(q), _) if ch == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(ch),
            (None, ';' | '{') if c.get(i) == Some(&' ') => i += 1,
            (None, ' ') if c.get(i) == Some(&'{') => {
                out.pop();
            }
            _ => {}
        }
    }
    out
}

/// CSS's own white space.
const CSS_SPACE: [char; 5] = [' ', '\t', '\n', '\r', '\x0c'];

/// Where the custom property declaration at `i` (`--name:`) ends: at the
/// `;` or `}` after its value, outside strings, escapes and brackets.
/// None when no custom property starts at `i`.
fn custom_value(c: &[char], i: usize) -> Option<usize> {
    if c.get(i..i + 2) != Some(&['-', '-']) {
        return None;
    }
    let mut j = i + 2;
    while c.get(j).is_some_and(|x| x.is_ascii_alphanumeric() || *x == '-' || *x == '_') {
        j += 1;
    }
    while c.get(j).is_some_and(|x| CSS_SPACE.contains(x)) {
        j += 1;
    }
    if c.get(j) != Some(&':') {
        return None;
    }
    let (mut quote, mut depth) = (None, 0usize);
    j += 1;
    while j < c.len() {
        match (quote, c[j]) {
            (_, '\\') => j += 1,
            (Some(q), x) if x == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c[j]),
            (None, '(' | '[' | '{') => depth += 1,
            (None, ')' | ']') => depth = depth.saturating_sub(1),
            (None, ';' | '}') if depth == 0 => return Some(j),
            (None, '}') => depth -= 1,
            _ => {}
        }
        j += 1;
    }
    Some(c.len())
}

/// The page's script without its whole-line `// ` comments (after the
/// stylesheet, before anything is spliced in): like the stylesheet's, they
/// are for whoever reads index.html. A comment at the end of a line of code
/// stays, so a `//` in a string or a URL is never touched.
fn without_js_comment_lines(index: &str) -> String {
    let Some((_, start)) = style_span(index) else { return index.to_owned() };
    let mut out = String::with_capacity(index.len());
    out.push_str(&index[..start]);
    for l in index[start..].split_inclusive('\n') {
        if !l.trim_start().starts_with("// ") {
            out.push_str(l);
        }
    }
    out
}

/// What the scanner is in at a point of a script.
#[derive(Clone, Copy, PartialEq, Debug)]
enum S {
    Code,
    Quote(char),
    Template,
    Line,
    Block,
    /// A regular expression; `true` inside its `[…]`.
    Regex(bool),
}

/// The state each line of `script` starts in (lines as `split_inclusive('\n')`
/// gives them), and whether it ends in code with every template closed.
/// Strings, comments, templates with `${…}` (nested to any depth) and
/// regular expressions are read as such; a `/` is a regular expression
/// after `(`, `,`, `=`, `:`, `[`, `!`, `&`, `|`, `?`, `{`, `}`, `;`, a
/// line's start or a keyword such as `return`, else a division.
fn line_states(script: &str) -> (Vec<S>, bool) {
    scan(script, None)
}

/// [`line_states`], and with `marks` the state each character is read in.
fn scan(script: &str, mut marks: Option<&mut Vec<S>>) -> (Vec<S>, bool) {
    const BEFORE_REGEX: [&str; 13] = ["return", "typeof", "case", "do", "else", "in", "of", "void", "throw", "new", "delete", "yield", "await"];
    // A template's `${` opens code: the stack says what each `}` closes.
    let mut stack: Vec<S> = Vec::new();
    let (mut s, mut depth) = (S::Code, Vec::<usize>::new());
    let mut braces = 0usize;
    let (mut prev, mut raw) = ('\n', '\n');
    // The code character before `prev` (`++` and `--` end an operand), and
    // whether each open `(` follows `if`, `while`, `for` or `with`: a `/`
    // after its `)` starts a regular expression, not a division.
    let mut prev2 = '\n';
    let mut parens: Vec<bool> = Vec::new();
    let mut after_cond = false;
    let mut word = String::new();
    let mut out = Vec::new();
    for line in script.split_inclusive('\n') {
        out.push(s);
        let cs: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < cs.len() {
            let (at, from) = (s, i);
            let c = cs[i];
            let next = cs.get(i + 1).copied().unwrap_or('\0');
            let mut ident = false;
            match s {
                S::Code => match c {
                    '/' if next == '/' => s = S::Line,
                    '/' if next == '*' => {
                        s = S::Block;
                        i += 1;
                    }
                    '/' if "(,=:[!&|?{};\n><*%~^".contains(prev)
                        || (matches!(prev, '+' | '-') && prev2 != prev)
                        || (prev == ')' && after_cond)
                        || (prev.is_alphanumeric() && BEFORE_REGEX.contains(&word.as_str())) =>
                    {
                        s = S::Regex(false)
                    }
                    '(' => parens.push(["if", "while", "for", "with"].contains(&word.as_str())),
                    ')' => after_cond = parens.pop().unwrap_or(false),
                    '\'' | '"' => s = S::Quote(c),
                    '`' => s = S::Template,
                    '{' => braces += 1,
                    '}' if depth.last() == Some(&braces) => {
                        depth.pop();
                        s = stack.pop().unwrap_or(S::Code);
                    }
                    '}' => braces = braces.saturating_sub(1),
                    c if c.is_alphanumeric() || c == '_' || c == '$' => {
                        if !(raw.is_alphanumeric() || raw == '_' || raw == '$') {
                            word.clear();
                        }
                        word.push(c);
                        ident = true;
                    }
                    _ => {}
                },
                // A `\` at a line's end (`\r\n` too) goes on to the next line.
                S::Quote(_) | S::Template if c == '\\' => i += if next == '\r' && cs.get(i + 2) == Some(&'\n') { 2 } else { 1 },
                S::Quote(q) if c == q || c == '\n' => s = S::Code,
                S::Template if c == '`' => s = S::Code,
                S::Template if c == '$' && next == '{' => {
                    stack.push(S::Template);
                    depth.push(braces);
                    s = S::Code;
                    i += 1;
                }
                S::Line if c == '\n' => s = S::Code,
                S::Block if c == '*' && next == '/' => {
                    s = S::Code;
                    i += 1;
                }
                S::Regex(_) if c == '\\' => i += 1,
                S::Regex(false) if c == '[' => s = S::Regex(true),
                S::Regex(true) if c == ']' => s = S::Regex(false),
                S::Regex(false) if c == '/' || c == '\n' => s = S::Code,
                _ => {}
            }
            if !ident && !c.is_whitespace() {
                word.clear();
            }
            raw = c;
            // What came last (a `${` ends on its `{`, so a regular expression may follow).
            let last = cs.get(i).copied().unwrap_or(c);
            if !last.is_whitespace() {
                prev2 = if last == '\n' { '\n' } else { prev };
                prev = last;
            }
            if last == '\n' {
                prev = '\n';
            }
            if let Some(m) = marks.as_deref_mut() {
                m.extend(std::iter::repeat_n(at, (i + 1).min(cs.len()) - from));
            }
            i += 1;
        }
    }
    (out, s == S::Code && stack.is_empty())
}

/// Each `<script>`'s text after the stylesheet, as HTML reads it: from the
/// tag's `>` to the first `</script>`, whatever string that is in.
fn scripts(index: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut at = style_span(index).map_or(0, |(_, e)| e);
    while let Some(i) = find_tag(index, "<script", at) {
        let Some(open) = index[i..].find('>').map(|j| i + j + 1) else { break };
        let close = find_tag(index, "</script", open).unwrap_or(index.len());
        out.push((open, close));
        at = close;
    }
    out
}

/// The page without the indentation of its stylesheet's and scripts' lines
/// (after the comments go, before anything is spliced in): 20 K units off
/// the page, which no system needs to fit a limit now; it is only smaller
/// to load. A line starting inside a string, a template literal, a
/// comment or a regular expression keeps it, as does a script the scanner
/// doesn't end in code (then it may have read it wrong); the markup
/// (`<pre>`, `<textarea>`) between them is never touched.
fn without_indent(index: &str) -> String {
    let trim = |l: &str| l.trim_start_matches([' ', '\t']).to_owned();
    let mut out = String::with_capacity(index.len());
    let mut at = 0;
    if let Some((start, end)) = style_span(index) {
        let start = start + "<style>".len();
        out.push_str(&index[..start]);
        let mut cont = false;
        for l in index[start..end].split_inclusive('\n') {
            // A line after one ending in `\` may be a string's rest.
            out.push_str(&if cont { l.to_owned() } else { trim(l) });
            cont = l.trim_end_matches(['\n', '\r']).ends_with('\\');
        }
        at = end;
    }
    for (open, close) in scripts(index) {
        if open < at {
            continue;
        }
        out.push_str(&index[at..open]);
        let script = &index[open..close];
        let (states, closed) = line_states(script);
        if closed {
            for (l, st) in script.split_inclusive('\n').zip(states) {
                out.push_str(&if st == S::Code { trim(l) } else { l.to_owned() });
            }
        } else {
            out.push_str(script);
        }
        at = close;
    }
    out.push_str(&index[at..]);
    out
}

/// A script's tokens as a browser reads them: code split at its spaces (which
/// go), everything in a string, template, comment or regular expression as
/// it is. Taking out indentation where it is code leaves them the same.
#[cfg(test)]
fn tokens(script: &str) -> Vec<String> {
    let mut marks = Vec::new();
    scan(script, Some(&mut marks));
    let (mut out, mut cur) = (Vec::new(), String::new());
    for (c, st) in script.chars().zip(marks) {
        // A line comment ends at its newline, which is no part of it.
        if (st == S::Code || st == S::Line && c == '\n') && c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    out.extend((!cur.is_empty()).then_some(cur));
    out
}

/// The lines [`without_js_comment_lines`] would take out that are inside
/// a template literal (text, not a comment): their 1-based numbers.
#[cfg(test)]
fn comment_lines_in_templates(index: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for (open, close) in scripts(index) {
        let first = index[..open].matches('\n').count() + 1;
        let script = &index[open..close];
        let (states, _) = line_states(script);
        for (n, (l, st)) in script.split_inclusive('\n').zip(states).enumerate() {
            if st == S::Template && l.trim_start().starts_with("// ") {
                out.push(first + n);
            }
        }
    }
    out
}

/// A page that only needs its words and the language step spliced in
/// (kiri-win's setup page).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn words(page: &str, lang: Option<&str>) -> String {
    page.replacen("/*LANGSTEP*/", LANGSTEP_JS, 1).replacen("/*LANG*/", &super::lang::boot_js(lang), 1).replacen("/*STRINGS*/{}", &super::lang::strings_js(super::lang::resolve(lang)), 1)
}

#[cfg(test)]
mod tests {
    use super::super::lang;

    #[test]
    fn the_script_loses_its_comment_lines_only() {
        let p = super::without_js_comment_lines("<style>\n  // css stays\n</style><script>\n  // why\n  a(); // kept\n  b(\"https://x\");\n  //not a comment line\n</script>");
        assert_eq!(p, "<style>\n  // css stays\n</style><script>\n  a(); // kept\n  b(\"https://x\");\n  //not a comment line\n</script>");
    }

    #[test]
    fn no_comment_line_taken_out_is_text_in_a_template_literal() {
        // A line of a template literal that starts with `// ` is text: taking it out changes the page.
        let page = "<style></style><script>\nconst a = `x\n  // text, not a comment\n${b ? `\n// inner\n` : \"\"}`;\n// a comment\nconst r = /`/; const s = \"`\";\n// another\n</script>";
        assert_eq!(super::comment_lines_in_templates(page), [3, 5]);
        let index = include_str!("index.html");
        assert_eq!(super::comment_lines_in_templates(index), Vec::<usize>::new(), "index.html: a template literal's line starts with `// `; it would be stripped");
    }

    #[test]
    fn indentation_goes_only_where_it_is_code() {
        let page = concat!(
            "<style>\n  a { b: c; }\n    d { content: \"x\\\n  y\"; }\n</style>\n",
            "<pre>\n  kept\n</pre>\n",
            "<script>\n",
            "  const t = `a\n    b ${x ? `\n      in` : `${y}`}\n  c`;\n",
            "    const s = \"// not a comment\", u = '/* nor this */';\n",
            "  const r = /[`'\"]/g, q = /\\//;\n",
            "  if (a) return /`/.test(b);\n",
            "  const z = n / 2 / `\n   still`;\n",
            "</script>\n",
            "<textarea>\n  kept\n</textarea>\n",
        );
        let want = concat!(
            "<style>\na { b: c; }\nd { content: \"x\\\n  y\"; }\n</style>\n",
            "<pre>\n  kept\n</pre>\n",
            "<script>\n",
            "const t = `a\n    b ${x ? `\n      in` : `${y}`}\n  c`;\n",
            "const s = \"// not a comment\", u = '/* nor this */';\n",
            "const r = /[`'\"]/g, q = /\\//;\n",
            "if (a) return /`/.test(b);\n",
            "const z = n / 2 / `\n   still`;\n",
            "</script>\n",
            "<textarea>\n  kept\n</textarea>\n",
        );
        assert_eq!(super::without_indent(page), want);
        // A script the scanner can't close (here a template never ended) stays as it is.
        let open = "<style></style><script>\n  a(`\n  b\n</script>";
        assert_eq!(super::without_indent(open), open);
        // HTML ends a script at `</script>`, in a string or not: the script
        // before it stays (its string never ends), and what follows is markup.
        let cut = "<style></style><script>\n  const w = \"</script>\n  html now\n</script>\n<script>\n  b();\n</script>";
        assert_eq!(super::without_indent(cut), "<style></style><script>\n  const w = \"</script>\n  html now\n</script>\n<script>\nb();\n</script>");
        // index.html's own script is read to its end, and stripped it is the same tokens.
        let index = include_str!("index.html");
        let stripped = super::without_indent(index);
        let (a, b) = (super::scripts(index), super::scripts(&stripped));
        assert_eq!(a.len(), b.len());
        for ((open, close), (o2, c2)) in a.into_iter().zip(b) {
            assert!(super::line_states(&index[open..close]).1, "the script at {open} doesn't end in code");
            let (x, y) = (super::tokens(&index[open..close]), super::tokens(&stripped[o2..c2]));
            let at = x.iter().zip(&y).position(|(a, b)| a != b).unwrap_or(x.len().min(y.len()));
            assert!(x == y, "the script at {open} reads otherwise stripped, from token {at}: {:?} / {:?}", &x[at.saturating_sub(3)..(at + 3).min(x.len())], &y[at.saturating_sub(3)..(at + 3).min(y.len())]);
        }
    }

    /// Each case's scripts read the same tokens stripped as not, and lose
    /// their code lines' indentation.
    fn same_tokens(page: &str, want: &str) {
        let got = super::without_indent(page);
        assert_eq!(got, want);
        for ((o, c), (o2, c2)) in super::scripts(page).into_iter().zip(super::scripts(&got)) {
            assert_eq!(super::tokens(&page[o..c]), super::tokens(&got[o2..c2]), "{page:?}");
        }
    }

    #[test]
    fn the_stripper_reads_scripts_as_a_browser_does() {
        // CRLF, a string going on past a `\` at a line's end.
        same_tokens("<style>\r\n  a{}\r\n</style><script>\r\n  const a = \"x\\\r\n  y\";\r\n  b();\r\n</script>", "<style>\r\na{}\r\n</style><script>\r\nconst a = \"x\\\r\n  y\";\r\nb();\r\n</script>");
        // `</SCRIPT>` and `</script >` end a script as `</script>` does; markup after stays.
        same_tokens("<style></style><SCRIPT>\n  a();\n</SCRIPT >\n<pre>\n  kept\n</pre>\n<script type=\"x\">\n  b();\n</script\n>", "<style></style><SCRIPT>\na();\n</SCRIPT >\n<pre>\n  kept\n</pre>\n<script type=\"x\">\nb();\n</script\n>");
        // A `<style media=…>` before the plain one: no panic, the plain one is the page's.
        let media = "<style media=\"print\">\n  /* p */ x{}\n</style><style>\n  /* why */ a{}\n</style><script>\n  b();\n</script>";
        assert_eq!(super::without_css_comments(media), "<style media=\"print\">\n  /* p */ x{}\n</style><style>\n   a{}\n</style><script>\n  b();\n</script>");
        same_tokens(media, "<style media=\"print\">\n  /* p */ x{}\n</style><style>\n/* why */ a{}\n</style><script>\nb();\n</script>");
        let _ = super::without_js_comment_lines(media);
        // A regular expression with a backtick after `=>`, an `if (…)`, `+`, `<`; a division after `++` or a call's `)`.
        same_tokens(
            "<style></style><script>\n  const f = (s) => /`/.test(s);\n  if (a) /`/.test(b);\n  const g = x + /`/.source;\n  const h = y < /`/.source;\n  const i = n++ / 2, j = f(n) / 2 / `\n  t`;\n  done();\n</script>",
            "<style></style><script>\nconst f = (s) => /`/.test(s);\nif (a) /`/.test(b);\nconst g = x + /`/.source;\nconst h = y < /`/.source;\nconst i = n++ / 2, j = f(n) / 2 / `\n  t`;\ndone();\n</script>",
        );
    }

    #[test]
    fn the_page_tests_strip_the_page_to_the_same_bytes() {
        use std::io::Write;
        // scripts/ui-test.py builds the page its own way: what it strips must be what the window loads.
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/ui-test.py");
        let strip = |page: &str| super::without_indent(&super::without_js_comment_lines(&super::without_css_comments(page)));
        let cases = [
            include_str!("index.html"),
            "<style>\r\n  /* why */ a{}\r\n  b { content: \"x\\\r\n  y\"; }\r\n</style><script>\r\n  // gone\r\n  const a = \"x\\\r\n  y\";\r\n  b();\r\n</script>",
            "<style media=\"print\">\n  /* p */ x{}\n</style><style>\n  /* why */ a{} /*XTERM_CSS*/\n</style><SCRIPT>\n  a();\n</SCRIPT >\n<pre>\n  kept\n</pre>\n<script>\n  const f = (s) => /`/.test(s);\n  if (a) /`/.test(b);\n  const g = x + /`/.source, i = n++ / 2;\n  done();\n</script>",
            // Escapes outside strings, custom property values: tight_css's edge cases.
            "<style>\n  .a\\\" b { c: d; } .a\\{ b { c: d; } /* x */ --t: { y; z }; \n  :root { --a: { b; c }; --d:  x  {y} ; e: f; --s: \"a; {\"; --e: a\\; b; } .a--b { c: calc(1px -- 2px); }\n</style><script>\n  a();\n</script>",
        ];
        for page in cases {
            let Ok(mut child) = std::process::Command::new("python3").arg(script).arg("--strip").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn() else {
                eprintln!("no python3: skipped");
                return;
            };
            let mut stdin = child.stdin.take().unwrap();
            let input = page.to_owned();
            let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
            let out = child.wait_with_output().unwrap();
            writer.join().unwrap().unwrap();
            assert!(out.status.success());
            let (py, rs) = (String::from_utf8(out.stdout).unwrap(), strip(page));
            let at = py.bytes().zip(rs.bytes()).position(|(a, b)| a != b).unwrap_or(py.len().min(rs.len()));
            assert!(py == rs, "ui-test.py strips otherwise at byte {at}: {:?} / {:?}", &py[at.saturating_sub(40).min(py.len())..(at + 40).min(py.len())], &rs[at.saturating_sub(40).min(rs.len())..(at + 40).min(rs.len())]);
        }
    }

    #[test]
    fn the_stylesheet_loses_its_comments_but_not_its_splice_points() {
        let p = super::without_css_comments("<style>/* why */a { b: c; } /*XTERM_CSS*/ /* two\nlines */d{}</style><script>/* kept */</script>");
        assert_eq!(p, "<style>a{b: c;} /*XTERM_CSS*/ d{}</style><script>/* kept */</script>");
        let p = super::without_css_comments("<style>e::after { content: \"x; { y\\\" ; {\"; } f { g: 'h; {'; }</style>");
        assert_eq!(p, "<style>e::after{content: \"x; { y\\\" ; {\";} f{g: 'h; {';}</style>");
        // An escape outside a string never starts one, nor ends a selector.
        assert_eq!(super::tight_css(r#".a\" b { c: d; } .e { f: "g; {"; }"#), r#".a\" b{c: d;} .e{f: "g; {";}"#);
        assert_eq!(super::tight_css(r".a\{ b { c: d; }"), r".a\{ b{c: d;}");
        assert_eq!(super::tight_css(r".a\; b { c: d; }"), r".a\; b{c: d;}");
        // A custom property's value stays as written, `{` and `;` in it too.
        assert_eq!(super::tight_css(":root { --a: { b; c }; --d:  x  {y} ; e: f; }"), ":root{--a: { b; c };--d:  x  {y} ;e: f;}");
        assert_eq!(super::tight_css(".x { --j: [1; 2]; --s: \"a; {\"; --e: a\\; b; k: l; }"), ".x{--j: [1; 2];--s: \"a; {\";--e: a\\; b;k: l;}");
        // A selector or value with `--` in it is no custom property.
        assert_eq!(super::tight_css(".a--b { c: calc(1px -- 2px); }"), ".a--b{c: calc(1px -- 2px);}");
    }

    #[test]
    fn every_placeholder_is_filled_once_and_the_page_stays_a_sane_size() {
        let units = |s: &str| s.encode_utf16().count();
        let page = super::page(include_str!("index.html"), None);
        for p in ["/*ICONS*/", "/*LANGSTEP*/", "/*XTERM_CSS*/", "/*XTERM_JS*/", "/*LANG*/", "/*STRINGS*/"] {
            assert!(!page.contains(p), "{p} left in the page");
        }
        assert!(page.contains(".xterm-viewport"), "xterm.css missing");
        // The main page keeps its own catalog (personal i18n). langstep is
        // spliced into setup, which carries the placeholders.
        assert!(page.contains("const LANGS = ["), "language list missing");
        let setup = super::words(include_str!("../../../kiri-win/src/setup.html"), None);
        for p in ["/*LANGSTEP*/", "/*LANG*/", "/*STRINGS*/"] {
            assert!(!setup.contains(p), "{p} left in the setup page");
        }
        assert!(setup.contains("function langStep("), "langstep.js missing from the setup page");
        assert!(setup.contains("window.KIRI_LANG = {"), "setup language boot missing");
        // No system has a hard limit on it now, so this is a sanity ceiling
        // only. Windows no longer loads the main page with NavigateToString.
        let n = units(&page);
        assert!(n < 8_000_000, "the page is {n} UTF-16 units");
        let mut worst_words = 0;
        for (tag, _) in lang::FILES {
            worst_words = worst_words.max(units(&super::words("/*LANG*/ /*STRINGS*/{}", Some(tag))));
        }
        assert!(worst_words < 200_000, "setup words are {worst_words} UTF-16 units");
    }
}
