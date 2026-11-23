#!/usr/bin/env python3
"""Tests for tests-scope.py: one per rule, and one per "when in doubt, wider".

    python3 -m unittest discover -s scripts/ci -p 'test_tests_scope.py'
"""
import importlib.util
import os
import unittest

_spec = importlib.util.spec_from_file_location("tests_scope", os.path.join(os.path.dirname(os.path.abspath(__file__)), "tests-scope.py"))
ts = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ts)

L, W, M = "linux", "windows", "mac"

WORKSPACE = """
[workspace]
members = ["crates/*"]
"""


def crate(name, deps=""):
    return f'[package]\nname = "{name}"\nversion = "0.0.0"\n\n{deps}\n'


# A small repository shaped like Kiri's: kiri (all three), kiri-win (its
# real code under cfg(windows), a stub elsewhere, #[path] includes from
# kiri), kiri-init (Linux only: the jobs build it on Linux alone) and
# kiri-vmm (a cfg(unix) dependency of kiri).
BASE = {
    "Cargo.toml": WORKSPACE,
    "Cargo.lock": "# lock\n",
    "crates/kiri/Cargo.toml": crate("kiri", '[target.\'cfg(unix)\'.dependencies]\nkiri-vmm = { path = "../kiri-vmm" }\n'),
    "crates/kiri/build.rs": 'include!("key.rs");\nfn main() { println!("cargo:rerun-if-changed=../../brand/kiri.ico"); }\n',
    "crates/kiri/key.rs": "fn key() {}\n",
    "crates/kiri/src/main.rs": """\
mod shared;
#[cfg(target_os = "macos")]
mod apple;
#[cfg(windows)]
mod wsl;
#[cfg(unix)]
mod unixy;
mod ui;
#[cfg(any(target_os = "linux", test))]
mod linux_or_test;

fn main() {
    shared::run();
}
""",
    "crates/kiri/src/shared.rs": """\
pub fn run() {
    let a = 1;
    let b = 2;
}

#[cfg(target_os = "macos")]
fn mac_only() {
    let x = 1;
    let y = "}";
}

#[cfg(windows)]
fn win_only() {
    let x = 1;
}

#[cfg(not(windows))]
fn not_win() {
    let z = 1;
}

#[cfg(all(unix, not(target_os = "macos")))]
fn linux_only() {
    let q = 1;
}

#[cfg(test)]
mod tests {
    #[test]
    fn t() {
        assert_eq!(1, 1);
    }
}

#[cfg(feature = "x")]
fn feature_x() {
    let f = 1;
}

pub fn statements() {
    #[cfg(target_os = "macos")]
    {
        let inner = 1;
    }
    let after = 2;
    if cfg!(windows) {
        let w = 1;
    }
}

macro_rules! m {
    () => {
        #[cfg(windows)]
        fn from_macro() {}
    };
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn with_cfg_attr() {
    let c = 1;
}

// a comment
fn last() {}
""",
    "crates/kiri/src/apple.rs": "pub fn a() {}\nmod container;\n",
    "crates/kiri/src/apple/container.rs": "pub fn c() {}\n",
    "crates/kiri/src/wsl.rs": "pub fn w() {}\n",
    "crates/kiri/src/unixy.rs": "pub fn u() {}\n",
    "crates/kiri/src/linux_or_test.rs": "pub fn lt() {}\n",
    "crates/kiri/src/ui/mod.rs": '#[cfg(target_os = "macos")]\nmod mac;\nmod lang;\nconst PAGE: &str = include_str!("index.html");\n',
    "crates/kiri/src/ui/mac.rs": "pub fn m() {}\n",
    "crates/kiri/src/ui/lang.rs": 'pub fn l() {}\n#[cfg(windows)]\nconst ICON: &[u8] = include_bytes!("icon.png");\n',
    "crates/kiri/src/ui/index.html": "<html>\n",
    "crates/kiri/src/ui/icon.png": "png\n",
    "crates/kiri/src/orphan.rs": "fn nobody_declares_me() {}\n",
    "crates/kiri-win/Cargo.toml": crate("kiri-win"),
    "crates/kiri-win/src/main.rs": """\
#[cfg(windows)]
mod app;
#[cfg(windows)]
#[path = "../../kiri/src/ui/lang.rs"]
mod lang;
#[cfg(any(windows, test))]
mod page;

#[cfg(windows)]
fn main() {}

#[cfg(not(windows))]
fn main() {
    eprintln!("kiri-win runs on Windows");
}
""",
    "crates/kiri-win/src/app.rs": "pub fn app() {}\n",
    "crates/kiri-win/src/page.rs": "pub fn page() {}\n",
    "crates/kiri-init/Cargo.toml": crate("kiri-init"),
    "crates/kiri-init/src/main.rs": '#[cfg(target_os = "linux")]\nmod linux;\n#[cfg(not(target_os = "linux"))]\nfn main() {}\n',
    "crates/kiri-init/src/linux/mod.rs": "pub fn main() {}\n",
    "crates/kiri-vmm/Cargo.toml": crate("kiri-vmm"),
    "crates/kiri-vmm/src/lib.rs": "pub fn vmm() {}\n",
    "brand/kiri.ico": "ico\n",
    "README.md": "# Kiri\n",
    "guest/kernel/kiri.config": "CONFIG_X=y\n",
    "contrib/windows/kiri.ps1": "Write-Host hi\n",
    "tests/e2e.sh": "#!/bin/bash\nt hello\n",
    "tests/regressions.tsv": "hello\t#1\tbroke\n",
    "tests/ui/test.py": "print(1)\n",
}


def run(changes, base=None):
    base = dict(BASE if base is None else base)
    head = dict(base)
    for path, text in changes.items():
        if text is None:
            head.pop(path, None)
        else:
            head[path] = text
    out, notes = ts.scope(ts.dict_snapshot(base), ts.dict_snapshot(head))
    return out, ts.outputs(out)


def systems(changes, base=None):
    out, res = run(changes, base)
    return {s for s in ts.SYSTEMS if res[s]}


def edit(path, old, new):
    text = BASE[path]
    assert old in text, (path, old)
    return {path: text.replace(old, new, 1)}


class Cfg(unittest.TestCase):
    def ev(self, text):
        return set(ts.cfg_from_text(text))

    def test_targets(self):
        self.assertEqual(self.ev('cfg(target_os = "linux")'), {L})
        self.assertEqual(self.ev('cfg(target_os = "macos")'), {M})
        self.assertEqual(self.ev("cfg(windows)"), {W})
        self.assertEqual(self.ev("cfg(unix)"), {L, M})
        self.assertEqual(self.ev('cfg(target_family = "windows")'), {W})
        # Only the Mac's vendor is known: elsewhere it may be either.
        self.assertEqual(self.ev('cfg(target_vendor = "apple")'), {L, M, W})
        self.assertEqual(self.ev('cfg(not(target_vendor = "apple"))'), {L, W})

    def test_combinators(self):
        self.assertEqual(self.ev("cfg(not(windows))"), {L, M})
        self.assertEqual(self.ev('cfg(all(unix, not(target_os = "macos")))'), {L})
        self.assertEqual(self.ev('cfg(any(target_os = "macos", windows))'), {M, W})
        self.assertEqual(self.ev('cfg(target_os = "ios")'), set())
        self.assertEqual(self.ev("cfg(any())"), set())
        self.assertEqual(self.ev("cfg(all())"), {L, M, W})

    def test_unknowns_are_either(self):
        self.assertEqual(self.ev("cfg(test)"), {L, M, W})
        self.assertEqual(self.ev("cfg(not(test))"), {L, M, W})
        self.assertEqual(self.ev("cfg(all(windows, test))"), {W})
        self.assertEqual(self.ev("cfg(any(windows, test))"), {L, M, W})
        self.assertEqual(self.ev('cfg(all(feature = "x", target_os = "linux"))'), {L})
        self.assertEqual(self.ev('cfg(not(target_arch = "x86_64"))'), {L, M, W})
        self.assertEqual(self.ev("cfg(debug_assertions)"), {L, M, W})


class Scanner(unittest.TestCase):
    def test_braces_in_strings_chars_comments_and_raw_strings(self):
        src = 'fn a() { let s = "}"; let c = \'}\'; let r = r#"}"#; /* } /* } */ */ // }\n let l: &\'static str = "x"; }\n'
        toks = ts.tokenize(src)
        ts.match_brackets(toks)  # balanced
        self.assertIn("life", [t.kind for t in toks])

    def test_unbalanced_is_an_error(self):
        self.assertIsNotNone(ts.Parsed("fn a() {\n").error)
        self.assertIsNotNone(ts.Parsed('fn a() { let s = "x; }\n').error)

    def test_doc_comments_are_code_and_comments_are_not(self):
        p = ts.Parsed("/// doc\n// plain\nfn a() {}\n")
        self.assertIn(1, p.line_ctx)
        self.assertNotIn(2, p.line_ctx)


class FileLevel(unittest.TestCase):
    def test_shared_file(self):
        self.assertEqual(systems(edit("crates/kiri/src/shared.rs", "let a = 1;", "let a = 10;")), {L, M, W})

    def test_cfg_module(self):
        self.assertEqual(systems({"crates/kiri/src/apple.rs": "pub fn a() { 1; }\nmod container;\n"}), {M})
        self.assertEqual(systems({"crates/kiri/src/wsl.rs": "pub fn w() { 1; }\n"}), {W})
        self.assertEqual(systems({"crates/kiri/src/unixy.rs": "pub fn u() { 1; }\n"}), {L, M})

    def test_ancestor_cfg_applies_to_children(self):
        self.assertEqual(systems({"crates/kiri/src/apple/container.rs": "pub fn c() { 2; }\n"}), {M})
        self.assertEqual(systems({"crates/kiri/src/ui/mac.rs": "pub fn m() { 2; }\n"}), {M})

    def test_unknown_cfg_atom_is_union(self):
        self.assertEqual(systems({"crates/kiri/src/linux_or_test.rs": "pub fn lt() { 2; }\n"}), {L, M, W})

    def test_new_apple_module_starts_only_the_mac_job(self):
        # The owner's example: no list to edit, the cfg on `mod` decides.
        main = BASE["crates/kiri/src/main.rs"].replace("mod ui;\n", 'mod ui;\n#[cfg(target_os = "macos")]\nmod apple_container;\n')
        self.assertEqual(systems({"crates/kiri/src/main.rs": main, "crates/kiri/src/apple_container.rs": "pub fn run() {}\n"}), {M})

    def test_path_include_makes_a_file_windows_too(self):
        # lang.rs is kiri's (all three) and kiri-win's under cfg(windows).
        self.assertEqual(systems({"crates/kiri/src/ui/lang.rs": BASE["crates/kiri/src/ui/lang.rs"].replace("l() {}", "l() { 1; }")}), {L, M, W})
        base = dict(BASE)
        base["crates/kiri/src/ui/mod.rs"] = '#[cfg(target_os = "macos")]\nmod mac;\n#[cfg(target_os = "macos")]\nmod lang;\n'
        self.assertEqual(systems({"crates/kiri/src/ui/lang.rs": BASE["crates/kiri/src/ui/lang.rs"].replace("l() {}", "l() { 1; }")}, base), {M, W})

    def test_kiri_win_stub_and_test_modules(self):
        self.assertEqual(systems({"crates/kiri-win/src/app.rs": "pub fn app() { 1; }\n"}), {W})
        self.assertEqual(systems({"crates/kiri-win/src/page.rs": "pub fn page() { 1; }\n"}), {L, M, W})
        self.assertEqual(systems(edit("crates/kiri-win/src/main.rs", 'eprintln!("kiri-win runs on Windows");', 'eprintln!("Windows only");')), {L, M})

    def test_crate_built_for_one_system(self):
        self.assertEqual(systems({"crates/kiri-init/src/linux/mod.rs": "pub fn main() { 1; }\n"}), {L})
        # its non-Linux stub is built by no job (the Mac job excludes kiri-init)
        self.assertEqual(systems(edit("crates/kiri-init/src/main.rs", "fn main() {}", "fn main() { 1; }")), set())

    def test_cfg_dependency(self):
        # kiri-vmm is a cfg(unix) dependency of kiri: the Windows check never builds it.
        self.assertEqual(systems({"crates/kiri-vmm/src/lib.rs": "pub fn vmm() { 1; }\n"}), {L, M})


class LineLevel(unittest.TestCase):
    F = "crates/kiri/src/shared.rs"

    def test_inside_cfg_fn(self):
        self.assertEqual(systems(edit(self.F, "let x = 1;\n    let y", "let x = 2;\n    let y")), {M})
        self.assertEqual(systems(edit(self.F, "fn win_only() {\n    let x = 1;", "fn win_only() {\n    let x = 3;")), {W})
        self.assertEqual(systems(edit(self.F, "let z = 1;", "let z = 2;")), {L, M})
        self.assertEqual(systems(edit(self.F, "let q = 1;", "let q = 2;")), {L})

    def test_brace_in_a_string_does_not_end_the_item(self):
        self.assertEqual(systems(edit(self.F, 'let y = "}";', 'let y = "}}";')), {M})

    def test_cfg_test_and_features_are_union(self):
        self.assertEqual(systems(edit(self.F, "assert_eq!(1, 1);", "assert_eq!(2, 2);")), {L, M, W})
        self.assertEqual(systems(edit(self.F, "let f = 1;", "let f = 2;")), {L, M, W})

    def test_cfg_block_statement(self):
        self.assertEqual(systems(edit(self.F, "let inner = 1;", "let inner = 2;")), {M})
        self.assertEqual(systems(edit(self.F, "let after = 2;", "let after = 3;")), {L, M, W})

    def test_cfg_macro_is_runtime_and_shared(self):
        self.assertEqual(systems(edit(self.F, "let w = 1;", "let w = 2;")), {L, M, W})

    def test_comment_only(self):
        out, res = run(edit(self.F, "// a comment", "// another comment"))
        self.assertEqual({s for s in ts.SYSTEMS if res[s]}, set())
        self.assertIn("comments or blank lines only", " ".join(out[0].reasons))

    def test_doc_comment_counts(self):
        self.assertEqual(systems(edit(self.F, "// a comment\nfn last", "/// a doc comment\nfn last")), {L, M, W})

    def test_reason_names_the_item(self):
        out, _ = run(edit(self.F, "let x = 1;\n    let y", "let x = 2;\n    let y"))
        self.assertIn('inside #[cfg(target_os = "macos")] fn mac_only', " ".join(out[0].reasons))

    def test_lines_in_two_items_take_the_union(self):
        both = edit(self.F, "let x = 1;\n    let y", "let x = 2;\n    let y")[self.F]
        both = both.replace("fn win_only() {\n    let x = 1;", "fn win_only() {\n    let x = 3;")
        self.assertEqual(systems({self.F: both}), {M, W})

    def test_adding_a_whole_cfg_item(self):
        self.assertEqual(systems(edit(self.F, "// a comment\n", '#[cfg(windows)]\nfn new_win() {\n    let n = 1;\n}\n// a comment\n')), {W})
        self.assertEqual(systems(edit(self.F, "#[cfg(windows)]\nfn win_only() {\n    let x = 1;\n}\n", "")), {W})


class WhenInDoubt(unittest.TestCase):
    F = "crates/kiri/src/shared.rs"

    def test_changing_a_cfg_attribute(self):
        self.assertEqual(systems(edit(self.F, '#[cfg(target_os = "macos")]\nfn mac_only', "#[cfg(unix)]\nfn mac_only")), {L, M, W})

    def test_moving_code_out_of_a_cfg_item(self):
        # The closing brace moves: win_only now swallows the code after it.
        text = BASE[self.F].replace("    let x = 1;\n}\n\n#[cfg(not(windows))]", "    let x = 1;\n\n#[cfg(not(windows))]", 1)
        text = text.replace("fn linux_only", "}\nfn linux_only", 1)
        self.assertEqual(systems({self.F: text}), {L, M, W})

    def test_inserting_between_an_attribute_and_its_item(self):
        self.assertEqual(systems(edit(self.F, "#[cfg(windows)]\nfn win_only", "#[cfg(windows)]\nfn another() {}\nfn win_only")), {L, M, W})

    def test_opening_a_block_comment(self):
        # Both changed lines are Windows code, but the comment between them
        # swallows shared() for every system.
        base = dict(BASE)
        base[self.F] = "#[cfg(windows)]\nfn a() {\n    let x = 1;\n}\nfn shared() {\n    let s = 1;\n}\n#[cfg(windows)]\nfn b() {\n    let y = 1;\n}\n"
        head = base[self.F].replace("let x = 1;", "let x = 1; /*").replace("    let y = 1;", "    */ let y = 1;")
        self.assertEqual(systems({self.F: head}, base), {L, M, W})

    def test_cfg_attr(self):
        self.assertEqual(systems(edit(self.F, '#[cfg_attr(target_os = "macos", allow(dead_code))]', '#[cfg_attr(windows, allow(dead_code))]')), {L, M, W})

    def test_macro_rules_body(self):
        self.assertEqual(systems(edit(self.F, "fn from_macro() {}", "fn from_macro() { 1; }")), {L, M, W})

    def test_unreadable_file(self):
        self.assertEqual(systems(edit(self.F, "let x = 1;\n    let y", "let x = {;\n    let y")), {L, M, W})

    def test_build_script_and_rust_include(self):
        self.assertEqual(systems({"crates/kiri/build.rs": BASE["crates/kiri/build.rs"] + "// x\n"}), {L, M, W})
        self.assertEqual(systems({"crates/kiri/key.rs": "fn key() { 1; }\n"}), {L, M, W})

    def test_cargo_files(self):
        self.assertEqual(systems({"Cargo.lock": "# lock 2\n"}), {L, M, W})
        self.assertEqual(systems({"crates/kiri-init/Cargo.toml": crate("kiri-init") + "# x\n"}), {L, M, W})

    def test_deleted_and_renamed_files(self):
        self.assertEqual(systems({"crates/kiri/src/wsl.rs": None}), {W})
        self.assertEqual(systems({"crates/kiri/src/shared.rs": None}), {L, M, W})
        # a rename is a delete and an add: each keeps its file's systems
        main = BASE["crates/kiri/src/main.rs"].replace("mod wsl;", "mod wsl2;")
        self.assertEqual(systems({"crates/kiri/src/wsl.rs": None, "crates/kiri/src/wsl2.rs": BASE["crates/kiri/src/wsl.rs"],
                                  "crates/kiri/src/main.rs": main}), {W})

    def test_file_no_crate_declares(self):
        self.assertEqual(systems({"crates/kiri/src/orphan.rs": "fn x() {}\n"}), {L, M, W})

    def test_unknown_file(self):
        self.assertEqual(systems({"deny.toml": "x\n"}), {L, M, W})


class NotRust(unittest.TestCase):
    def test_words_and_kernel(self):
        self.assertEqual(systems({"README.md": "# K\n", "guest/kernel/kiri.config": "CONFIG_Y=y\n"}), set())

    def test_packaging(self):
        self.assertEqual(systems({"contrib/windows/kiri.ps1": "x\n"}), {W})

    def test_included_files_count_for_the_code_that_includes_them(self):
        # The page is the UI workflow's, but kiri's unit tests read it too.
        self.assertEqual(systems({"crates/kiri/src/ui/index.html": "<html><body>\n"}), {L, M, W})
        self.assertEqual(systems({"crates/kiri/src/ui/icon.png": "png2\n"}), {W})

    def test_build_script_inputs(self):
        self.assertEqual(systems({"brand/kiri.ico": "ico2\n"}), {L, M, W})

    def test_suites_start_the_scripts_job_only(self):
        out, res = run({"tests/e2e.sh": "#!/bin/bash\nt hello2\n"})
        self.assertEqual({s for s in ts.SYSTEMS if res[s]}, set())
        self.assertTrue(res["scripts"])
        self.assertTrue(res["suites_changed"])
        out, res = run({"tests/regressions.tsv": "x\n"})
        self.assertTrue(res["scripts"] and res["suites_changed"])
        out, res = run({"tests/ui/test.py": "print(2)\n"})
        self.assertTrue(res["scripts"])
        self.assertFalse(res["suites_changed"])
        out, res = run({"README.md": "x\n"})
        self.assertFalse(res["scripts"] or res["suites_changed"])


if __name__ == "__main__":
    unittest.main()
