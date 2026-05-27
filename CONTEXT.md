# Touch Bar on Linux — context, architecture, and resources

This document explains **what** the Touch Bar stack is on a 2016–2017 MacBook Pro (T1), **how** the pieces in `tiny-dfr-fix` fit together, and **where** to read more.

## Hardware overview

The Touch Bar on pre-T2 MacBook Pros is part of the **iBridge** USB composite device:

- **USB ID:** `05ac:8600`
- **Two configurations:**
  - **Config 1:** HID / keyboard / touch sensor (default after boot)
  - **Config 2:** Adds **DRM bulk endpoints** for the OLED strip (`appletbdrm` driver)

Without config 2, the kernel never exposes a `/dev/dri/card*` for the bar, and `tiny-dfr` cannot draw.

A separate **HID interface (interface 6)** controls Touch Bar **backlight** (nits) on T1. That path is slow (USB HID) and must not fight the DRM framebuffer path on the same device.

The **touch sensor** appears as an input device (e.g. `Apple Inc. iBridge Touchpad`). It must be on **`seat-touchbar`**, not `seat0`, or GNOME will treat touches as main-screen input.

## Software stack (bottom to top)

```
┌─────────────────────────────────────────────────────────────┐
│  GNOME / compositor (seat0)                                  │
│  - Brightness OSD via KEY_BRIGHTNESSUP from uinput            │
│  - Volume, media keys from tiny-dfr virtual keyboard          │
└─────────────────────────────────────────────────────────────┘
                              ▲
                              │ uinput (virtual keyboard)
┌─────────────────────────────────────────────────────────────┐
│  tiny-dfr (Rust)                                             │
│  - libinput seat-touchbar → touch → tap_key / highlights      │
│  - libinput seat0 → Fn key → layer switch                     │
│  - DRM atomic commit → framebuffer → appletbdrm               │
│  - Cairo + librsvg → button icons                            │
└─────────────────────────────────────────────────────────────┘
                              ▲
                              │ DRM + dirty framebuffer
┌─────────────────────────────────────────────────────────────┐
│  appletbdrm (kernel module, DKMS)                            │
│  - T1: synchronous GINF probe, portrait mode 60×2170         │
│  - Bulk IN/OUT framedata + UPDATE_COMPLETE (timestamp quirk) │
│  - backlight device: appletb_backlight                         │
└─────────────────────────────────────────────────────────────┘
                              ▲
                              │ USB config 2
┌─────────────────────────────────────────────────────────────┐
│  Apple iBridge 05ac:8600                                     │
└─────────────────────────────────────────────────────────────┘
```

## T1 vs T2 (important)

| | T1 (2017 MBP, this guide) | T2+ |
|---|---------------------------|-----|
| USB PID | `8600` | `8302` etc. |
| Probe | Synchronous USB + GINF/STATS | Often async |
| DRM mode from GINF | 2170×60 pixels; DRM mode **60×2170** | Different layout |
| Brightness | HID reports (iface 6) | Often bulk SBTN |
| Touch device name | `Apple Inc. iBridge Touchpad` | Often `… Touch Bar` |

Upstream `tiny-dfr` and early `appletbdrm` targeted T2; T1 needs the patches in this bundle (GINF drain, `begin_y`, timestamp `U64_MAX`, connector modes prefill, etc.).

## tiny-dfr configuration model

Config merges:

1. `/usr/share/tiny-dfr/config.toml` (template)
2. `/etc/tiny-dfr/config.toml` (overrides)

Key options:

- **`MediaLayerDefault`** — `true` → default layer is `MediaLayerKeys`; hold **Fn** for `PrimaryLayerKeys` (F1–F12).
- **`MediaLayerKeys` / `PrimaryLayerKeys`** — arrays of `{ Icon=…, Action=…, Mode=Media }` or `{ Text="F1", Action="F1" }`.
- **`AdaptiveBrightness`** — if true, daemon may write `appletb_backlight` from display brightness (HID-heavy on T1). **Recommend `false`.**

Actions map to Linux input keys (`input-linux` `Key` enum). Brightness should use **`BrightnessUp` / `BrightnessDown`** via uinput so **GNOME shows OSD** and applies policy — not raw sysfs from tiny-dfr.

## Boot sequence (this bundle)

1. **`systemd-udevd`** — `99-appletb-usb-config.rules` sets config 2 when device is on config 1.
2. **`macbook-quirks.service`** — `modprobe -r appletbdrm`; `echo 2 > …/bConfigurationValue`; `modprobe appletbdrm`.
3. **`tiny-dfr.service`** — starts before GDM; opens DRM card; draws Touch Bar.
4. **GDM / user session** — main GPU + `seat0`; Touch Bar input on `seat-touchbar`.

## Suspend behavior on MacBookPro14,3

For this model, the most workable suspend setup in practice is often:

- kernel cmdline: `mem_sleep_default=s2idle`
- `/etc/systemd/sleep.conf`:
  - `[Sleep]`
  - `SuspendState=freeze`

This improves chances of suspend/resume, but is known to be slow and can still break subsystems.

### Known resume failure classes

1. **WiFi (BCM43602) can fail from PCIe D3cold**
   - Typical logs: `Unable to change power state from D3cold to D0`, `probe after resume failed, err=-19`
   - Mitigation in this repo:
     - `udev/99-pcie-no-d3cold.rules`
     - `systemd/sleep/wifi-resume`

2. **Touch Bar can resume dark even if tiny-dfr restarted**
   - tiny-dfr may be running but `appletb_backlight` can remain at `0`
   - Mitigation in this repo:
     - `systemd/sleep/touchbar-resume` restores backlight and nudges `appletbdrm` if needed

## Patches in this `appletbdrm` (summary)

Why they exist (T1-specific):

1. **`drm_registered` guard** — avoid oops on failed probe teardown.
2. **`appletbdrm_probe_t1_sync`** — STATS drain, CLRD, GINF retries, `usb_clear_halt`.
3. **Connector `connected` + pre-filled modes** — tiny-dfr probes before DRM master.
4. **DRM mode `hdisplay=60`, `vdisplay=2170`** — matches drawing code (rotate 90°).
5. **`begin_y = height - damage.x2`** — correct damage coords for portrait mode.
6. **Framebuffer timestamp** — accept `U64_MAX` as well as echoed timestamp.
7. **`connector_detect` → connected** — userspace sees connected state.

## Patches in this `tiny-dfr` (summary)

1. **`looks_like_touchbar`** — accept portrait or landscape aspect ratio.
2. **Digitizer** — match `iBridge` device name (T1).
3. **`button_at_x`** — spacing-aware hit test; no bogus Y threshold (T1 reports `y≈0`).
4. **`tap_key`** — press+release on touch down; avoids stuck keys.
5. **DRM redraw on worker thread** — input never blocks on USB framebuffer flush.
6. **No adaptive HID sync** on hot path — optional pause after brightness keys.
7. **Brightness via uinput** — GNOME notifications and standard brightness stack.

## Display vs Touch Bar backlight

| Control | sysfs / path | Who should change it |
|---------|----------------|----------------------|
| **Main LCD** | `/sys/class/backlight/gmux_backlight/brightness` | GNOME / UPower (via key events) |
| **Touch Bar OLED backlight** | `/sys/class/backlight/appletb_backlight/brightness` | `appletbdrm` / tiny-dfr dim logic |

Writing gmux from tiny-dfr works but **bypasses GNOME** (no OSD). Prefer uinput brightness keys.

## Input: seats and libinput

- **`seat-touchbar`** — udev sets `ID_SEAT=seat-touchbar` on `Apple Inc. iBridge Touchpad`.
- **tiny-dfr** uses `input_tb.udev_assign_seat("seat-touchbar")` for touches; `input_main` on `seat0` for Fn.
- **`98-touchbar-fake.rules`** — marks device as touchscreen for libinput touch protocol.

## Resources

### Upstream / drivers

- [tiny-dfr](https://github.com/WhatAmISupposedToPutHere/tiny-dfr) — userspace daemon (Rust).
- [appletbdrm](https://github.com/linux-appletb/appletbdrm) — kernel DRM driver (T2-oriented upstream).
- [linux-appletb](https://github.com/linux-appletb) — org / docs.
- [marvinrobot78/MacBookPro-14-3-Ubuntu](https://github.com/marvinrobot78/MacBookPro-14-3-Ubuntu) — related MacBook Pro Linux notes.

### Kernel / USB

- [applespi](https://github.com/torvalds/linux/blob/master/drivers/input/keyboard/applespi.c) — internal keyboard; `KEY_FN` for Fn layer.
- Linux `drivers/gpu/drm/tiny/appletbdrm.c` — in-tree minimal driver; DKMS fork usually better for T1.

### Desktop integration

- [input-linux Key enum](https://docs.rs/input-linux/latest/input_linux/enum.Key.html) — valid `Action = "…"` names in config.
- GNOME brightness OSD — triggered by `KEY_BRIGHTNESSUP` / `KEY_BRIGHTNESSDOWN` from any input device (including uinput).

### Tools in this repo

- `tools/touchbar-layout-editor.py` — drag-and-drop reorder with Touch Bar preview (SVG icons, spacing, outlines).
- [FIX.md](FIX.md) — install and troubleshooting.

## Debugging commands

```bash
# USB
lsusb -d 05ac:8600 -v | grep -E 'Configuration|bInterface'
cat /sys/bus/usb/devices/1-3/bConfigurationValue

# Kernel
journalctl -k -b | grep appletbdrm

# Userspace
journalctl -u tiny-dfr -b -f

# DRM
ls -la /dev/dri/
modetest -c 2>/dev/null | head -40

# Input
libinput list-devices | grep -A6 -i ibridge
udevadm info /sys/class/input/input*/device | grep -E 'NAME|ID_SEAT'

# Test brightness (main display)
cat /sys/class/backlight/gmux_backlight/{brightness,max_brightness}
```

## License notes

- **tiny-dfr** — MIT / Apache-2.0 (see upstream).
- **appletbdrm** — GPL-2.0 (kernel module).
- This bundle’s docs and tools — use alongside those licenses.
