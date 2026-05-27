# How to fix tiny-dfr on MacBook Pro Touch Bar (T1 / 2016–2017)

This guide matches the setup in this repository. It assumes Ubuntu/Debian with a **t2/tuxonice** or similar kernel and an Apple **iBridge** USB device `05ac:8600`.

## Symptoms

| Symptom | Likely cause |
|---------|----------------|
| Touch Bar blank, no `/dev/dri/card*` for Touch Bar | USB stuck on config 1, or `appletbdrm` not loaded |
| `tiny-dfr` exits: “No connected touchbar-like connectors found” | Wrong DRM mode, empty connector modes, or USB config |
| Touch moves the main screen cursor | Touch sensor on `seat0`; needs `seat-touchbar` udev rule |
| Buttons visible but no key action | uinput / digitizer / stuck key state |
| Brightness keys change screen but no GNOME popup | Must use uinput `BrightnessUp`/`Down`, not sysfs-only |
| After reload, `Invalid framebuffer response` | T1 timestamp quirk; need patched `appletbdrm` |
| Module stuck “Unloading” | USB deadlock; reboot |

## One-time install

### 1. Dependencies

```bash
sudo apt install dkms build-essential linux-headers-$(uname -r) \
  libinput-tools python3-tk
```

### 2. Kernel module (`appletbdrm`)

```bash
sudo mkdir -p /usr/src/appletbdrm-1.2
sudo cp appletbdrm/appletbdrm.c appletbdrm/dkms.conf /usr/src/appletbdrm-1.2/
sudo dkms add -m appletbdrm -v 1.2
sudo dkms build appletbdrm/1.2
sudo dkms install appletbdrm/1.2 --force
```

### 3. Userspace (`tiny-dfr`)

```bash
sudo cp bin/tiny-dfr /usr/bin/tiny-dfr
sudo chmod +x /usr/bin/tiny-dfr
```

Or build from `src/`:

```bash
cargo build --release
sudo cp target/release/tiny-dfr /usr/bin/tiny-dfr
```

### 4. udev + systemd

```bash
sudo cp udev/*.rules /etc/udev/rules.d/
sudo cp systemd/*.service /etc/systemd/system/
sudo udevadm control --reload-rules
sudo systemctl daemon-reload
sudo systemctl enable macbook-quirks.service tiny-dfr.service
```

### 5. Config

```bash
sudo mkdir -p /etc/tiny-dfr
sudo cp /usr/share/tiny-dfr/config.toml /etc/tiny-dfr/config.toml   # if missing
# Edit: MediaLayerDefault = true for media keys by default
```

Recommended in `/etc/tiny-dfr/config.toml`:

```toml
MediaLayerDefault = true
AdaptiveBrightness = false
```

### 6. Touch input (optional)

```bash
sudo mkdir -p /etc/libinput
sudo cp libinput/local-overrides.quirks /etc/libinput/ 2>/dev/null || true
sudo cp udev/99-touchbar-seat.rules /etc/udev/rules.d/
```

Log out and back in after first install so Wayland picks up `seat-touchbar`.

### 7. Reboot

```bash
sudo reboot
```

## Suspend settings for MacBookPro14,3

These settings improve suspend reliability on this hardware, but resume can still be slow and sometimes breaks parts of the system:

- add `mem_sleep_default=s2idle` to kernel cmdline
- set `SuspendState=freeze` in `/etc/systemd/sleep.conf`

Example `/etc/systemd/sleep.conf`:

```ini
[Sleep]
SuspendState=freeze
```

### Update kernel cmdline (`mem_sleep_default=s2idle`)

If you use GRUB:

```bash
sudo sed -i 's/^GRUB_CMDLINE_LINUX_DEFAULT="/&mem_sleep_default=s2idle /' /etc/default/grub
sudo update-grub
```

Reboot after changing boot parameters.

## Install resume recovery scripts (Touch Bar + WiFi)

This repo includes:

- `systemd/sleep/touchbar-resume`
- `systemd/sleep/wifi-resume`
- `udev/99-pcie-no-d3cold.rules` (prevents BCM43602 D3cold resume failures)

Install:

```bash
sudo mkdir -p /etc/systemd/system-sleep
sudo install -m 755 systemd/sleep/touchbar-resume /etc/systemd/system-sleep/touchbar-resume
sudo install -m 755 systemd/sleep/wifi-resume     /etc/systemd/system-sleep/wifi-resume

sudo cp udev/99-pcie-no-d3cold.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules

sudo bash -c 'echo 0 > /sys/bus/pci/devices/0000:03:00.0/d3cold_allowed'
sudo bash -c 'echo 0 > /sys/bus/pci/devices/0000:00:1c.0/d3cold_allowed'
```

## After reboot — verify

```bash
# USB config must be 2
cat /sys/bus/usb/devices/1-3/bConfigurationValue

# Driver loaded
lsmod | grep appletbdrm
journalctl -k -b | grep -E 'appletbdrm|GINF' | tail -10

# DRM node
ls -la /dev/dri/card*

# Services
systemctl status macbook-quirks tiny-dfr

# Touchbar seat
udevadm info /sys/class/input/input*/device 2>/dev/null | grep -E 'ID_SEAT|iBridge' | head -5
```

Expected kernel log (T1):

- `GINF: pixel 2170x60 ...`
- `Touch Bar T1 initialized`
- DRM mode **60×2170** (portrait; userspace rotates for drawing)

## If something breaks without reboot

```bash
sudo systemctl restart macbook-quirks.service
sleep 3
sudo systemctl restart tiny-dfr.service
journalctl -u tiny-dfr -n 20 --no-pager
```

If USB hangs (commands stick on `tee` to sysfs):

- **Reboot** — do not loop USB reset scripts.

## Update only the binary

```bash
cd ~/tiny-dfr   # or use src/ from this bundle
cargo build --release
sudo systemctl stop tiny-dfr
sudo cp target/release/tiny-dfr /usr/bin/tiny-dfr
sudo systemctl start tiny-dfr
```

## Reorder buttons (GUI)

```bash
cd tools && ./run-layout-editor.sh
# Or: sudo apt install python3-tomlkit && python3 touchbar-layout-editor.py
```

## Kernel upgrade

After a new kernel:

```bash
sudo dkms install appletbdrm/1.2 --force
```

If build fails, reinstall headers and rebuild.

## Hard rules (this setup)

- Do **not** stop GDM or switch to TTY-only boot for tiny-dfr.
- Touch Bar DRM is **non-desktop**; tiny-dfr holds the DRM node early via systemd order.
- Do **not** use `appletbdrm` from initramfs alone without DKMS — `macbook-quirks` reloads the DKMS module after setting USB config 2.

## File locations on a running system

| File | Role |
|------|------|
| `/etc/tiny-dfr/config.toml` | Button layout and options |
| `/usr/bin/tiny-dfr` | Daemon |
| `/usr/src/appletbdrm-1.2/` | DKMS module source |
| `/etc/systemd/system/macbook-quirks.service` | USB config 2 + modprobe |
| `/etc/systemd/system/tiny-dfr.service` | Daemon unit |
| `/etc/udev/rules.d/99-appletb-usb-config.rules` | Auto config 2 on plug |
| `/etc/udev/rules.d/99-touchbar-seat.rules` | Touch → `seat-touchbar` |
