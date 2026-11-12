# kiri-win setup: page ⇄ host protocol

The first-run setup page (`setup.html`, Windows only) talks to kiri-win the
way the main page does: it sends JSON strings with
`window.webkit.messageHandlers.kiri.postMessage(JSON.stringify({cmd, ...}))`
(kiri-win's shim forwards that to WebView2) and receives state with
`window.kiri.on(event)`.

## Page → host

| cmd | fields | when |
|---|---|---|
| `setup-ready` | | the page has loaded; the host answers with the current `setup` state |
| `setup-install-wsl` | | Welcome → "Install WSL" |
| `setup-restart` | | Restart → "Restart now" |
| `setup-later` | | Restart → "Restart later" (hides the window to the tray) |
| `setup-choose` | `value`: `"kiri"` or `"ubuntu"` | Choose a Linux → the primary button |
| `setup-switch` | `value`: `"kiri"` | while Ubuntu downloads → "Use Kiri Linux instead" |
| `setup-cancel` | | any progress screen → "Cancel" (back to the step's start) |
| `setup-retry` | | error → "Try again" |
| `setup-use-local` | `which`: `"wsl"` or `"ubuntu"` (optional; inferred from the current step) | Welcome → "I already have the installer…"; download error → "Use a local file…" (stages the pinned MSI / `.wsl` into downloads after SHA-256 check, then continues the step) |
| `setup-default` | `value`: bool | Docker tools → "Make Kiri the default for docker" checkbox |
| `setup-build` | `value`: `"docker"` or `"kiri"` | Docker tools → Builds (shown only with another engine and Kiri not made docker's default): "Keep building with Docker" / "Build with Kiri" |
| `setup-autostart` | `value`: bool | Ready → "Start Kiri when I sign in" |
| `setup-open` | | Ready → "Open Kiri" (the main window replaces setup) |
| `setup-quit` | | Welcome → "Quit" |
| `copy` | `value`: text | Ready → "Copy" |
| `open` | `name`: https URL | "What is WSL?", the pinned WSL MSI / Ubuntu `.wsl` download (only those URLs) |

## Host → page

One event type, sent whenever anything changes (at most ~10 a second):

```json
{
  "type": "setup",
  "step": "welcome" | "wsl" | "restart" | "choose" | "linux" | "engine" | "tools" | "ready",
  "linux": "kiri" | "ubuntu" | null,
  "sizes": {"wsl": 367575040, "ubuntu": 418495746, "kiri": 9000000},
  "urls": {"wsl": "https://github.com/microsoft/WSL/releases/download/3.0.3/wsl.3.0.3.0.x64.msi", "ubuntu": "https://releases.ubuntu.com/…"},
  "existingUbuntu": false,
  "otherDocker": false,
  "otherEngine": null | "Docker Desktop",
  "makeDefault": true,
  "buildKiri": true,
  "autostart": true,
  "progress": {"pct": 58, "done": 224395264, "total": 367575040, "rate": 19084083, "eta": 9, "label": "Downloading WSL 3.0.3 from Microsoft"} | null,
  "tasks": [{"label": "Download WSL", "state": "done" | "active" | "todo" | "error", "detail": "214 / 368 MB"}],
  "log": ["kiri daemon: ready in 33 ms"],
  "error": null | "text"
}
```

- `step` picks the screen (variant A of the design canvas): welcome = A1,
  wsl = A2, restart = A3, choose = A4, linux = A5 (kiri) / A5b (ubuntu),
  engine = A6, tools = A7, ready = A8.
- `progress.done`/`total` are bytes when downloading (show "214 of 368 MB",
  rate as MB/s, eta in seconds); `total` null means an unsized step (show
  the percentage only, or an indeterminate bar when `pct` is null).
- `error`: shown on the current step's screen with "Try again" (`setup-retry`).
- The left rail derives from `step`: steps before it done, it current, after it to do.
