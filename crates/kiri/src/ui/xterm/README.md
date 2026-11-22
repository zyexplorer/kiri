# xterm.js, vendored

The built-in terminal's emulator. These files are the published builds,
unmodified, and assemble.rs splices them into the window's page.

| File | Package | SHA-256 |
|---|---|---|
| xterm.js, xterm.css | `@xterm/xterm@6.0.0` (lib/xterm.js, css/xterm.css) | 14903579ff54664cd72f8e8699e6961a6272c21863ec1c3b118cdc8af5d4a972, 854a7c0fb70e8b1a083c16797ab827299fb18744f5ad34f227b48337e33293c6 |
| addon-fit.js | `@xterm/addon-fit@0.11.0` (lib/addon-fit.js) | ba3ea256ce0620a0992a197d6c9baea64823fc93d8da07a9e366ca9943c18527 |
| addon-search.js | `@xterm/addon-search@0.16.0` (lib/addon-search.js) | 7bc1b8c7b3549411f6f6f779524c4ded6ca621fd80d64b40a70ae7c78aefbf55 |
| addon-web-links.js | `@xterm/addon-web-links@0.12.0` (lib/addon-web-links.js) | b74864125ef3889753b94e61eb4dabb99b2a27f920f0c9a7bd53cbe6d10f032a |

MIT licence: LICENSE here, and in the repository's NOTICE.

To update: download the same paths from https://unpkg.com/<package>@<version>/,
replace the files, update this table, and run `cargo test -p kiri assemble`
(the page's sanity checks) and `python3 scripts/ui-test.py`.
