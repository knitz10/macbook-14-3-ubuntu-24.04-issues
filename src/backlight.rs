use crate::config::Config;
use anyhow::{anyhow, Result};
use input::event::{
    switch::{Switch, SwitchEvent, SwitchState},
    Event,
};
use input_linux::Key;
use std::{
    cmp::min,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_DISPLAY_BRIGHTNESS: u32 = 509;
const MAX_TOUCH_BAR_BRIGHTNESS: u32 = 255;
/// After any brightness/illum adjustment, don't touch HID-backed touchbar backlight.
const BRIGHTNESS_PAUSE_MS: u64 = 3000;
/// Touchbar illumination sysfs writes each trigger slow HID; throttle them.
const ILLUM_WRITE_MIN_INTERVAL_MS: u64 = 300;

fn read_attr(path: &Path, attr: &str) -> u32 {
    fs::read_to_string(path.join(attr))
        .expect(&format!("Failed to read {attr}"))
        .trim()
        .parse::<u32>()
        .expect(&format!("Failed to parse {attr}"))
}

fn find_backlight() -> Result<PathBuf> {
    for entry in fs::read_dir("/sys/class/backlight/")? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        if ["display-pipe", "appletb_backlight"]
            .iter()
            .any(|s| name.contains(s))
        {
            return Ok(entry.path());
        }
    }
    Err(anyhow!("No Touch Bar backlight device found"))
}

fn find_display_backlight() -> Result<PathBuf> {
    for entry in fs::read_dir("/sys/class/backlight/")? {
        let entry = entry?;
        if [
            "apple-panel-bl",
            "gmux_backlight",
            "intel_backlight",
            "acpi_video0",
        ]
        .iter()
        .any(|s| entry.file_name().to_string_lossy().contains(s))
        {
            return Ok(entry.path());
        }
    }
    Err(anyhow!("No Built-in Retina Display backlight device found"))
}

fn set_backlight(mut file: &File, value: u32) {
    file.write(format!("{}\n", value).as_bytes()).unwrap();
}

pub struct BacklightManager {
    last_active: Instant,
    max_bl: u32,
    current_bl: u32,
    lid_state: SwitchState,
    bl_file: File,
    display_bl_path: PathBuf,
    display_bl_file: File,
    display_bl_max: u32,
    /// Skip sysfs/HID touchbar backlight updates (expensive on T1) after brightness keys.
    adaptive_paused_until: Instant,
    illum_write_allowed_at: Instant,
}

impl BacklightManager {
    pub fn new() -> BacklightManager {
        let bl_path = find_backlight().unwrap();
        let display_bl_path = find_display_backlight().unwrap();
        let bl_file = OpenOptions::new()
            .write(true)
            .open(bl_path.join("brightness"))
            .unwrap();
        let display_bl_max = read_attr(&display_bl_path, "max_brightness");
        let display_bl_file = OpenOptions::new()
            .write(true)
            .open(display_bl_path.join("brightness"))
            .expect("display backlight brightness");
        BacklightManager {
            bl_file,
            lid_state: SwitchState::Off,
            max_bl: read_attr(&bl_path, "max_brightness"),
            current_bl: read_attr(&bl_path, "brightness"),
            last_active: Instant::now(),
            display_bl_path,
            display_bl_file,
            display_bl_max,
            adaptive_paused_until: Instant::now(),
            illum_write_allowed_at: Instant::now(),
        }
    }

    pub fn is_brightness_key(action: Key) -> bool {
        matches!(
            action,
            Key::BrightnessUp | Key::BrightnessDown | Key::IllumUp | Key::IllumDown
        )
    }

    /// Display brightness keys: sysfs only (no uinput, no HID).
    pub fn is_display_brightness_key(action: Key) -> bool {
        matches!(action, Key::BrightnessUp | Key::BrightnessDown)
    }

    /// Touchbar illumination keys: sysfs write triggers kernel HID — avoid concurrent DRM.
    pub fn is_touchbar_illum_key(action: Key) -> bool {
        matches!(action, Key::IllumUp | Key::IllumDown)
    }

    /// After display-brightness keys, pause adaptive sync so we don't hammer HID iface 6.
    pub fn pause_adaptive_sync(&mut self, ms: u64) {
        self.adaptive_paused_until = Instant::now() + Duration::from_millis(ms);
    }

    /// Adjust brightness without sending uinput keys (avoids desktop + HID side effects).
    pub fn handle_brightness_key(&mut self, action: Key) {
        self.pause_adaptive_sync(BRIGHTNESS_PAUSE_MS);
        self.last_active = Instant::now();
        match action {
            Key::BrightnessUp => self.step_display_brightness(1),
            Key::BrightnessDown => self.step_display_brightness(-1),
            _ => {}
        }
    }

    fn step_display_brightness(&mut self, direction: i32) {
        let cur = read_attr(&self.display_bl_path, "brightness");
        let step = (self.display_bl_max / 32).max(1);
        let new = if direction > 0 {
            min(self.display_bl_max, cur + step)
        } else {
            cur.saturating_sub(step)
        };
        set_backlight(&mut self.display_bl_file, new);
    }

    fn step_touchbar_illumination(&mut self, direction: i32) {
        if Instant::now() < self.illum_write_allowed_at {
            return;
        }
        self.illum_write_allowed_at =
            Instant::now() + Duration::from_millis(ILLUM_WRITE_MIN_INTERVAL_MS);
        let step = (self.max_bl / 32).max(1);
        let new = if direction > 0 {
            min(self.max_bl, self.current_bl + step)
        } else {
            self.current_bl.saturating_sub(step)
        };
        self.current_bl = new;
        set_backlight(&mut self.bl_file, new);
    }
    fn display_to_touchbar(display: u32, active_brightness: u32) -> u32 {
        let normalized = display as f64 / MAX_DISPLAY_BRIGHTNESS as f64;
        // Add one so that the touch bar does not turn off
        let adjusted = (normalized.powf(0.5) * active_brightness as f64) as u32 + 1;
        adjusted.min(MAX_TOUCH_BAR_BRIGHTNESS) // Clamp the value to the maximum allowed brightness
    }
    pub fn process_event(&mut self, event: &Event) {
        match event {
            Event::Keyboard(_) | Event::Pointer(_) | Event::Gesture(_) | Event::Touch(_) => {
                self.last_active = Instant::now();
            }
            Event::Switch(SwitchEvent::Toggle(toggle)) => match toggle.switch() {
                Some(Switch::Lid) => {
                    self.lid_state = toggle.switch_state();
                    println!("Lid Switch event: {:?}", self.lid_state);
                    if toggle.switch_state() == SwitchState::Off {
                        self.last_active = Instant::now();
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    pub fn update_backlight(&mut self, cfg: &Config) {
        if Instant::now() < self.adaptive_paused_until {
            return;
        }
        let since_last_active = (Instant::now() - self.last_active).as_millis() as u64;
        let new_bl = min(
            self.max_bl,
            if self.lid_state == SwitchState::On {
                0
            } else if since_last_active < cfg.dim_timeout_ms {
                // Never mirror display brightness onto the touchbar here: each sysfs write
                // triggers slow HID traffic on T1 and blocks other touchbar I/O.
                let _ = cfg.adaptive_brightness;
                cfg.active_brightness
            } else if since_last_active < cfg.off_timeout_ms {
                cfg.dimmed_brightness
            } else {
                0
            },
        );
        if self.current_bl != new_bl {
            self.current_bl = new_bl;
            set_backlight(&self.bl_file, self.current_bl);
        }
    }
    pub fn current_bl(&self) -> u32 {
        self.current_bl
    }
}
