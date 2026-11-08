#!/usr/bin/env python3
"""Merge /tmp/kiri-i18n/{ja,ko,fr,de,es,pt,ru}.json into crates/kiri/src/ui/index.html."""
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HTML = ROOT / "crates/kiri/src/ui/index.html"
SRC = Path("/tmp/kiri-i18n")
LANGS = [
    ("ja", "JA", "日本語"),
    ("ko", "KO", "한국어"),
    ("fr", "FR", "Français"),
    ("de", "DE", "Deutsch"),
    ("es", "ES", "Español"),
    ("pt", "PT", "Português"),
    ("ru", "RU", "Русский"),
]


def js_str(s: str) -> str:
    return json.dumps(s, ensure_ascii=False)


def catalog_js(var: str, mapping: dict[str, str]) -> str:
    lines = [f"const {var} = {{"]
    for k, v in mapping.items():
        lines.append(f"  {js_str(k)}: {js_str(v)},")
    lines.append("};")
    return "\n".join(lines)


def main() -> int:
    keys = json.loads((SRC / "en_keys.json").read_text())
    catalogs: dict[str, dict[str, str]] = {}
    for code, var, _native in LANGS:
        path = SRC / f"{code}.json"
        if not path.exists():
            print(f"missing {path}", file=sys.stderr)
            return 1
        data = json.loads(path.read_text())
        missing = [k for k in keys if k not in data]
        extra = [k for k in data if k not in set(keys)]
        if missing or extra or len(data) != len(keys):
            print(f"{code}: keys={len(data)} expected={len(keys)} missing={len(missing)} extra={len(extra)}", file=sys.stderr)
            if missing[:5]:
                print("  missing sample:", missing[:5], file=sys.stderr)
            return 1
        # Ensure language names exist as identity where useful.
        for _c, _v, native in LANGS:
            data.setdefault(native, native)
        data.setdefault("English", "English")
        data.setdefault("简体中文", "简体中文")
        data.setdefault("繁體中文", "繁體中文")
        # Rebuild in key order for stable diffs.
        catalogs[code] = {k: data[k] for k in keys}
        # Append any identity extras that weren't in keys
        for native in [n for _, _, n in LANGS] + ["English", "简体中文", "繁體中文"]:
            if native not in catalogs[code]:
                catalogs[code][native] = data.get(native, native)
        print(f"ok {code}: {len(catalogs[code])} entries")

    html = HTML.read_text()
    # Remove previously injected NEWLANG blocks if re-run.
    html = re.sub(r"\n/\*I18N_NEWLANGS\*/.*?/\*I18N_NEWLANGS_END\*/\n", "\n", html, flags=re.S)

    blocks = ["/*I18N_NEWLANGS*/"]
    for code, var, _native in LANGS:
        blocks.append(catalog_js(var, catalogs[code]))
    blocks.append("/*I18N_NEWLANGS_END*/")
    insert = "\n".join(blocks) + "\n"

    marker = "const LANGS ="
    idx = html.find(marker)
    if idx < 0:
        print("LANGS marker missing", file=sys.stderr)
        return 1
    html = html[:idx] + insert + html[idx:]

    # Replace LANGS / lang() / I18N wiring.
    langs_list = [["en", "English"], ["zh", "简体中文"], ["zh-TW", "繁體中文"]] + [[c, n] for c, _, n in LANGS]
    langs_js = "const LANGS = [" + ", ".join(f'["{c}", "{n}"]' for c, n in langs_list) + "];"

    html = re.sub(r"const LANGS = \[.*?\];", lambda _m: langs_js, html, count=1)

    lang_fn = '''function lang() {
  const pick = prefs.lang || "system";
  if (LANGS.some(([code]) => code === pick)) return pick;
  const sys = (navigator.languages && navigator.languages[0]) || navigator.language || "en";
  if (/^zh[-_]?(TW|HK|MO)\\b/i.test(sys)) return "zh-TW";
  if (/^zh\\b/i.test(sys)) return "zh";
  if (/^ja\\b/i.test(sys)) return "ja";
  if (/^ko\\b/i.test(sys)) return "ko";
  if (/^fr\\b/i.test(sys)) return "fr";
  if (/^de\\b/i.test(sys)) return "de";
  if (/^es\\b/i.test(sys)) return "es";
  if (/^pt\\b/i.test(sys)) return "pt";
  if (/^ru\\b/i.test(sys)) return "ru";
  return "en";
}'''
    html = re.sub(r"function lang\(\) \{.*?\n\}", lambda _m: lang_fn, html, count=1, flags=re.S)

    i18n_js = 'const I18N = { zh: ZH, "zh-TW": ZH_TW, ja: JA, ko: KO, fr: FR, de: DE, es: ES, pt: PT, ru: RU };'
    html = re.sub(r"const I18N = \{[^}]*\};", i18n_js, html, count=1)

    # document.documentElement.lang mapping for new codes stays as code itself (paintChrome).
    HTML.write_text(html)
    print("updated", HTML)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
