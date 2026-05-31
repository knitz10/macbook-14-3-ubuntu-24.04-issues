# tiny-dfr-fix

A self-contained bundle for running **tiny-dfr** on a MacBook Pro Touch Bar (T1, `05ac:8600`) under Linux — including patched kernel driver, systemd/udev setup, documentation, and a button layout editor.

## Disclaimer

This repository is mostly AI-generated, all the scripts, etc.

I mostly just did research and put it into the chatbot, as I am not experienced with Linux n stuff enough and don't know Rust or C at all, and I didn't have time to try to figure everything out because I needed the laptop for school. As shameful as it is for me to have such a repository here publicly on my profile, I want to help those that might have such issues, so they don't have to go through what I did. I'll try to do better next time, sorry. °^°

Also, if any creators of modified programs want me to take their stuff down, please reach out to me, you can reach out by [creating an issue](https://github.com/knitz10/macbook-14-3-ubuntu-24.04-issues/issues/new), or you can reach out to me via [e-mail](mailto:antoni@knycz.net).

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

## How to edit the layout using tiny-dfr

You can now edit the layout using "tiny-dfr edit". You need to have python3 installed, and it will tell you the install commands for its dependencies. It's not the most beautiful editor but I didn't wanna touch it because I have 0 experience with tkinter and 0 time to learn it. You can add custom buttons with commands, additional bars, it's great, really. Strongly recommend. It looks like it's a prototype, and that's because it is, I don't have the time to try to properly finish it - even using ai, but it mostly works, it's just ugly.


## Docs

- **[FIX.md](FIX.md)** — How to install, verify, and recover when something breaks.
- **[CONTEXT.md](CONTEXT.md)** — Deep background: T1 vs T2, USB configs, DRM, input, resources.



<sub><sup>Also, uhh, I'm not responsible for any damage done to your hardware, okay? Although generally it should work just fine, if anything bad happens and isn't mentioned here, sorry, you're on your own. Just, you know, be careful with what you do in general...</sub></sup>
