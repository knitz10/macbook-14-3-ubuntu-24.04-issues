# tiny-dfr-fix

A self-contained bundle for running **tiny-dfr** on a MacBook Pro Touch Bar (T1, `05ac:8600`) under Linux — including patched kernel driver, systemd/udev setup, documentation, and a button layout editor.

## Disclaimer

This repository is mostly AI-generated.

"i mostly just did research and put it into the chatbot, as i am not experienced with linux enough and don't know rust at all, and i didn't have time to try to figure everything out because i needed the laptop for school."

## Quick start

1. Read [FIX.md](FIX.md) for install and troubleshooting.
2. Run `./install.sh` (review it first — it copies files system-wide).
3. Reboot once after first install.

## Suspend/Resume note (MacBookPro14,3)

On this model, suspend is most reliable with:

- Kernel cmdline: `mem_sleep_default=s2idle`
- `/etc/systemd/sleep.conf`:
  - `[Sleep]`
  - `SuspendState=freeze`

This can make suspend/resume **slow**, and it still **often breaks some devices** (especially WiFi or Touch Bar state) depending on kernel/firmware timing.
Use the included sleep hooks and PCIe power rule from this repo to improve recovery after resume.

## Layout editor (drag to reorder buttons)

```bash
cd tools && ./run-layout-editor.sh
```

- Preview matches the real Touch Bar (black strip, rounded tiles, SVG icons from `tiny-dfr-icons`)
- Loads merged `/etc` + `/usr/share` config like tiny-dfr
- Edit **Media**, **Fn/F-keys**, or **App** layers (if `SpecialExtendedMode` is on)
- Drag buttons left/right, then **Save & restart tiny-dfr**
- Uses `pkexec` if `/etc` is not writable

## Contents

| Path | Description |
|------|-------------|
| `bin/tiny-dfr` | Patched userspace daemon (Rust) |
| `src/` | Rust source matching this binary |
| `appletbdrm/` | Patched `appletbdrm` kernel module (DKMS) |
| `systemd/` | Services + suspend/resume hooks (`systemd/sleep/*`) |
| `udev/` | USB/touchbar rules + PCIe D3cold rule for BCM43602 |
| `libinput/` | Optional libinput quirks |
| `tools/` | Touch Bar layout editor |
| [FIX.md](FIX.md) | Step-by-step fix guide |
| [CONTEXT.md](CONTEXT.md) | Architecture, history, links |

## Docs

- **[FIX.md](FIX.md)** — How to install, verify, and recover when something breaks.
- **[CONTEXT.md](CONTEXT.md)** — Deep background: T1 vs T2, USB configs, DRM, input, resources.
