use std::{
    env,
    fs::{File, OpenOptions},
    io::ErrorKind,
    os::{
        fd::{AsRawFd, AsFd},
        unix::{io::OwnedFd, fs::{OpenOptionsExt, PermissionsExt}, net::UnixDatagram}
    },
    path::{Path, PathBuf},
    collections::HashMap,
    cmp::min,
    panic::{self, AssertUnwindSafe},
    process::{self, Command},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};
use cairo::{ImageSurface, Format, Context, Surface, Rectangle, Antialias};
use rsvg::{Loader, CairoRenderer, SvgHandle};
use drm::control::ClipRect;
use anyhow::{Result, anyhow};
use input::{
    Libinput, LibinputInterface, Device as InputDevice,
    event::{
        Event, device::DeviceEvent, EventTrait,
        touch::{TouchEvent, TouchEventPosition, TouchEventSlot},
        keyboard::{KeyboardEvent, KeyboardEventTrait, KeyState}
    }
};
use libc::{O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY, c_char};
use input_linux::{uinput::UInputHandle, EventKind, Key, SynchronizeKind};
use input_linux_sys::{uinput_setup, input_id, timeval, input_event};
use nix::{
    sys::{
        signal::{Signal, SigSet},
        epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags}
    }, 
    errno::Errno
};
use privdrop::PrivDrop;
use icon_loader::{IconFileType, IconLoader};
use chrono::{Local, Locale, Timelike};

mod backlight;
mod display;
mod pixel_shift;
mod fonts;
mod text_render;
mod config;

use backlight::BacklightManager;
use display::{DrmBackend, DrmRedrawHandle};
use pixel_shift::{PixelShiftManager, PIXEL_SHIFT_WIDTH_PX};
use config::{ButtonConfig, Config, FnMode, KeyboardModifiers};
use text_render::{LabelFonts, show_label_centered};
use crate::config::ConfigManager;

const BUTTON_SPACING_PX: i32 = 16;
const BUTTON_COLOR_INACTIVE: f64 = 0.200;
const BUTTON_COLOR_ACTIVE: f64 = 0.400;
const ICON_SIZE: i32 = 48;
const TIMEOUT_MS: i32 = 10 * 1000;
/// Poll interval while a touch is active or the display needs updating.
const INTERACTIVE_POLL_MS: i32 = 16;
enum ButtonImage {
    Text(String),
    Svg(SvgHandle),
    Bitmap(ImageSurface),
    Time(String, String),
    Blank
}

struct Button {
    image: ButtonImage,
    changed: bool,
    active: bool,
    action: Key,
    command: Option<String>,
    switch_to_layer: Option<usize>,
    background: bool
}

fn icon_name_candidates(icon_name: &str) -> Vec<String> {
    let mut names = vec![icon_name.to_string()];
    if !icon_name.ends_with("-symbolic") {
        names.push(format!("{icon_name}-symbolic"));
    }
    let mut expanded = Vec::new();
    for name in names {
        expanded.push(name.clone());
        if !name.ends_with(".svg") {
            expanded.push(format!("{name}.svg"));
        }
        if !name.ends_with(".png") {
            expanded.push(format!("{name}.png"));
        }
    }
    expanded
}

fn theme_icon_subdirs(theme: &str) -> Vec<Vec<String>> {
    vec![
        vec![],
        vec![theme.into()],
        vec![theme.into(), "symbolic".into()],
        vec![theme.into(), "symbolic".into(), "status".into()],
        vec![theme.into(), "symbolic".into(), "actions".into()],
        vec![theme.into(), "symbolic".into(), "apps".into()],
        vec!["hicolor".into(), "symbolic".into(), "status".into()],
        vec!["hicolor".into(), "symbolic".into(), "actions".into()],
        vec!["hicolor".into(), "symbolic".into(), "apps".into()],
    ]
}

fn resolve_icon_path(icon_name: &str, icon_theme: &str) -> Option<PathBuf> {
    let roots = [
        "/etc/tiny-dfr/icons",
        "/usr/share/tiny-dfr/icons",
        "/usr/share/icons",
        "/usr/share/pixmaps",
    ];
    for name in icon_name_candidates(icon_name) {
        for root in roots {
            for subdir in theme_icon_subdirs(icon_theme) {
                let mut path = PathBuf::from(root);
                for part in &subdir {
                    path.push(part);
                }
                path.push(&name);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn button_image_from_path(path: &Path) -> Result<ButtonImage> {
    let path_str = path.to_string_lossy();
    if path_str.ends_with(".svg") {
        let handle = Loader::new().read_path(path)?;
        return Ok(ButtonImage::Svg(handle));
    }
    if path_str.ends_with(".png") {
        let mut file = File::open(path)?;
        let surf = ImageSurface::create_from_png(&mut file)?;
        if surf.height() == ICON_SIZE && surf.width() == ICON_SIZE {
            return Ok(ButtonImage::Bitmap(surf));
        }
        let resized = ImageSurface::create(Format::ARgb32, ICON_SIZE, ICON_SIZE).unwrap();
        let c = Context::new(&resized).unwrap();
        c.scale(ICON_SIZE as f64 / surf.width() as f64, ICON_SIZE as f64 / surf.height() as f64);
        c.set_source_surface(surf, 0.0, 0.0).unwrap();
        c.set_antialias(Antialias::Best);
        c.paint().unwrap();
        return Ok(ButtonImage::Bitmap(resized));
    }
    Err(anyhow!("Unsupported icon format: {}", path.display()))
}

fn load_image_via_icon_loader(icon_name: &str, icon_theme: &str) -> Result<ButtonImage> {
    let mut search_paths: Vec<PathBuf> = vec![
        PathBuf::from("/etc/tiny-dfr/icons"),
        PathBuf::from("/usr/share/tiny-dfr/icons/"),
        PathBuf::from("/usr/share/icons/"),
    ];
    let mut loader = IconLoader::new();
    search_paths.extend(loader.search_paths().into_owned());
    loader.set_search_paths(search_paths);
    loader.set_theme_name_provider(icon_theme);
    loader.update_theme_name().unwrap();
    for name in icon_name_candidates(icon_name) {
        if let Some(icon_loader) = loader.load_icon(&name) {
            let icon = icon_loader.file_for_size(256);
            return match icon.icon_type() {
                IconFileType::SVG => {
                    let handle = Loader::new().read_path(icon.path())?;
                    Ok(ButtonImage::Svg(handle))
                }
                IconFileType::PNG => button_image_from_path(icon.path()),
                IconFileType::XPM => Err(anyhow!("Legacy XPM icons are not supported")),
            };
        }
    }
    Err(anyhow!("Icon not found in theme `{icon_theme}`: {icon_name}"))
}

fn load_image(icon_name: &str, mode: &Option<String>, path: &str) -> Result<ButtonImage> {
    if path != "use_default" {
        return Err(anyhow!("Custom path defined, using that"));
    }
    let theme = ConfigManager::new().load_theme();
    let icon_theme = match mode {
        Some(mode_val) => {
            if mode_val == "App" {
                theme.app_icon_theme
            } else {
                theme.media_icon_theme
            }
        }
        None => {
            panic!("No mode specified")
        }
    };
    load_image_via_icon_loader(icon_name, &icon_theme).or_else(|loader_err| {
        resolve_icon_path(icon_name, &icon_theme)
            .ok_or(loader_err)
            .and_then(|p| button_image_from_path(&p))
    })
}

fn try_load_svg_path(icon_name: &str, path: &str) -> Result<ButtonImage> {
    let handle = Loader::new().read_path(format!("{}", path)).or_else(|_| {
        Loader::new().read_path(format!("/usr/share/pixmaps/{}.svg", icon_name))
    })?;
    Ok(ButtonImage::Svg(handle))
}

fn try_load_png_path(icon_name: &str, path: &str) -> Result<ButtonImage> {
    let mut file = File::open(format!("{}", path)).or_else(|_| {
        File::open(format!("/usr/share/pixmaps/{}.png", icon_name))
    })?;
    let surf = ImageSurface::create_from_png(&mut file)?;
    if surf.height() == ICON_SIZE && surf.width() == ICON_SIZE {
        return Ok(ButtonImage::Bitmap(surf));
    }
    let resized = ImageSurface::create(Format::ARgb32, ICON_SIZE, ICON_SIZE).unwrap();
    let c = Context::new(&resized).unwrap();
    c.scale(ICON_SIZE as f64 / surf.width() as f64, ICON_SIZE as f64 / surf.height() as f64);
    c.set_source_surface(surf, 0.0, 0.0).unwrap();
    c.set_antialias(Antialias::Best);
    c.paint().unwrap();
    return Ok(ButtonImage::Bitmap(resized));
}

impl Button {
    fn with_config(cfg: ButtonConfig) -> Button {
        let background;
        let action = cfg.resolved_action();
        let command = cfg.command;
        let switch_to_layer = cfg.switch_to_layer;
        if let Some(icon) = cfg.icon {
            let path = match cfg.path {
                Some(p) => p,
                None => "use_default".to_string()
            };
            if let Some(bg) = cfg.background {
                background = bg;
            } else {
                if let Some(ref mode) = cfg.mode {
                    if mode.to_lowercase() == "app" {
                        background = false;
                    } else {
                        background = true;
                    }
                } else {
                    panic!("Invalid config, a button must have either Text, Icon or be Blank")
                }
            }
            Button::new_icon(&icon, action, cfg.mode, &path, background, command, switch_to_layer)
        } else if let Some(text) = cfg.text {
            if let Some(bg) = cfg.background {
                background = bg;
            } else {
                background = true;
            }
            Button::new_text(text, action, background, command, switch_to_layer)
        } else if let Some(mode) = cfg.mode {
            if let Some(bg) = cfg.background {
                background = bg;
            } else {
                background = false;
            }
            if mode.to_lowercase() == "blank" {
                Button::new_blank(action, background)
            } else if mode.to_lowercase() == "time" {
                let format = match cfg.format {
                    Some(f) => f,
                    None => "24hr".to_string()
                };
                let locale = match cfg.locale {
                    Some(l) => l,
                    None => "POSIX".to_string()
                };
                Button::new_time(action, format, locale, background)
            } else {
                panic!("Invalid config, a button must have either Text, Icon or be Blank")
            }
        } else {
            panic!("Invalid config, a button must have either Text, Icon or be Blank")
        }
    }
    fn new_text(text: String, action: Key, background: bool, command: Option<String>, switch_to_layer: Option<usize>) -> Button {
        Button {
            action,
            command,
            switch_to_layer,
            active: false,
            changed: false,
            image: ButtonImage::Text(text),
            background
        }
    }
    fn new_icon(icon_name: &str, action: Key, mode: Option<String>, path: &str, background: bool, command: Option<String>, switch_to_layer: Option<usize>) -> Button {
        let image = load_image(icon_name, &mode, path)
            .or_else(|_| try_load_svg_path(icon_name, path))
            .or_else(|_| try_load_png_path(icon_name, path))
            .unwrap_or_else(|err| {
                eprintln!("tiny-dfr: failed to load icon `{icon_name}`: {err}");
                ButtonImage::Text(icon_name.to_string())
            });
        Button {
            action, image,
            command,
            switch_to_layer,
            active: false,
            changed: false,
            background
        }
    }
    fn new_time(action: Key, format: String, locale: String, background: bool) -> Button {
        Button {
            action,
            command: None,
            switch_to_layer: None,
            active: false,
            changed: false,
            image: ButtonImage::Time(format, locale),
            background
        }
    }
    fn new_blank(action: Key, background: bool) -> Button {
        Button {
            action,
            command: None,
            switch_to_layer: None,
            active: false,
            changed: false,
            image: ButtonImage::Blank,
            background
        }
    }
    fn render(
        &self,
        c: &Context,
        button_left_edge: f64,
        button_width: u64,
        label_top: f64,
        label_bottom: f64,
        y_shift: f64,
        fonts: &LabelFonts<'_>,
        font_size: f64,
    ) {
        match &self.image {
            ButtonImage::Text(text) => {
                show_label_centered(
                    c,
                    text,
                    button_left_edge + button_width as f64 / 2.0,
                    label_top + y_shift,
                    label_bottom + y_shift,
                    font_size,
                    fonts,
                );
            },
            ButtonImage::Svg(svg) => {
                let renderer = CairoRenderer::new(&svg);
                let x = button_left_edge + (button_width as f64 / 2.0 - (ICON_SIZE / 2) as f64).round();
                let y = y_shift + ((label_top + label_bottom - ICON_SIZE as f64) / 2.0).round();

                renderer.render_document(c,
                    &Rectangle::new(x, y, ICON_SIZE as f64, ICON_SIZE as f64)
                ).unwrap();
            }
            ButtonImage::Bitmap(surf) => {
                let x = button_left_edge + (button_width as f64 / 2.0 - (ICON_SIZE / 2) as f64).round();
                let y = y_shift + ((label_top + label_bottom - ICON_SIZE as f64) / 2.0).round();
                c.set_source_surface(surf, x, y).unwrap();
                c.rectangle(x, y, ICON_SIZE as f64, ICON_SIZE as f64);
                c.fill().unwrap();
            }
            ButtonImage::Time(format, locale) => {
                let current_time = Local::now();
                let current_locale = Locale::try_from(locale.as_str()).unwrap_or(Locale::POSIX);
                let formatted_time;
                if format == "24hr" {
                    formatted_time = format!(
                    "{}:{}    {} {} {}",
                     current_time.format_localized("%H", current_locale),
                     current_time.format_localized("%M", current_locale),
                     current_time.format_localized("%a", current_locale),
                     current_time.format_localized("%-e", current_locale),
                     current_time.format_localized("%b", current_locale)
                );
                } else {
                    formatted_time = format!(
                    "{}:{} {}    {} {} {}",
                    current_time.format_localized("%-l", current_locale),
                    current_time.format_localized("%M", current_locale),
                    current_time.format_localized("%p", current_locale),
                    current_time.format_localized("%a", current_locale),
                    current_time.format_localized("%-e", current_locale),
                    current_time.format_localized("%b", current_locale)
                );
                }
                show_label_centered(
                    c,
                    &formatted_time,
                    button_left_edge + button_width as f64 / 2.0,
                    label_top + y_shift,
                    label_bottom + y_shift,
                    font_size,
                    fonts,
                );
            }
            _ => {
            }
        }
    }
    fn highlight(&mut self, active: bool) {
        if self.active != active {
            self.active = active;
            self.changed = true;
        }
    }

    fn emits_key(&self) -> bool {
        self.action != Key::Unknown && self.action != Key::Time
    }

    fn is_interactive(&self) -> bool {
        self.emits_key() || self.command.is_some() || self.switch_to_layer.is_some()
    }

    fn set_active<F>(&mut self, uinput: &mut UInputHandle<F>, active: bool) where F: AsRawFd {
        if self.active != active {
            self.highlight(active);
            if self.emits_key() {
                toggle_key(uinput, self.action, active as i32);
            }
        }
    }
}

#[derive(Default)]
pub struct FunctionLayer {
    buttons: Vec<Button>
}

impl FunctionLayer {
    fn with_config(cfg: Vec<ButtonConfig>) -> FunctionLayer {
        if cfg.is_empty() {
            panic!("Invalid configuration, layer has 0 buttons");
        }
        FunctionLayer {
            buttons: cfg.into_iter().map(Button::with_config).collect()
        }
    }
    fn draw(&mut self, config: &Config, width: i32, height: i32, surface: &Surface, pixel_shift: (f64, f64), complete_redraw: bool) -> Vec<ClipRect> {
        let c = Context::new(&surface).unwrap();
        let mut modified_regions = if complete_redraw {
            vec![ClipRect::new(0, 0, height as u16, width as u16)]
        } else {
            Vec::new()
        };
        c.translate(height as f64, 0.0);
        c.rotate((90.0f64).to_radians());
        let pixel_shift_width = if config.enable_pixel_shift { PIXEL_SHIFT_WIDTH_PX } else { 0 };
        let button_width = ((width - pixel_shift_width as i32) - (BUTTON_SPACING_PX * (self.buttons.len() - 1) as i32)) as f64 / self.buttons.len() as f64;
        let radius = 8.0f64;
        let bot = (height as f64) * 0.15;
        let top = (height as f64) * 0.85;
        let (pixel_shift_x, pixel_shift_y) = pixel_shift;

        if complete_redraw {
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.paint().unwrap();
        }
        const FONT_SIZE: f64 = 32.0;
        let label_fonts = LabelFonts {
            primary: &config.font_face,
            emoji: config.emoji_font_face.as_ref(),
        };
        for (i, button) in self.buttons.iter_mut().enumerate() {
            if !button.changed && !complete_redraw {
                continue;
            };

            let left_edge = (i as f64 * (button_width + BUTTON_SPACING_PX as f64)).floor() + pixel_shift_x + (pixel_shift_width / 2) as f64;
            let color = if button.active {
                BUTTON_COLOR_ACTIVE
            } else if config.show_button_outlines {
                BUTTON_COLOR_INACTIVE
            } else {
                0.0
            };
            if !complete_redraw {
                c.set_source_rgb(0.0, 0.0, 0.0);
                if button.action == Key::Time {
                    c.rectangle(left_edge, bot - radius, button_width * 3.0, top - bot + radius * 2.0);
                } else {
                    c.rectangle(left_edge, bot - radius, button_width, top - bot + radius * 2.0);
                }
                c.fill().unwrap();
            }
            if (!(button.action == Key::Unknown && !button.is_interactive()) &&
               button.action != Key::Time &&
               button.action != Key::Macro1 &&
               button.action != Key::Macro2 &&
               button.action != Key::Macro3 &&
               button.action != Key::Macro4) &&
               ((button.background) ||
                button.active) {
            c.set_source_rgb(color, color, color);
            // draw box with rounded corners
            c.new_sub_path();
            let left = left_edge + radius;
            let right = (left_edge + button_width.ceil()) - radius;
            c.arc(
                right,
                bot,
                radius,
                (-90.0f64).to_radians(),
                (0.0f64).to_radians(),
            );
            c.arc(
                right,
                top,
                radius,
                (0.0f64).to_radians(),
                (90.0f64).to_radians(),
            );
            c.arc(
                left,
                top,
                radius,
                (90.0f64).to_radians(),
                (180.0f64).to_radians(),
            );
            c.arc(
                left,
                bot,
                radius,
                (180.0f64).to_radians(),
                (270.0f64).to_radians(),
            );
            c.close_path();

            c.fill().unwrap();
            }
            c.set_source_rgb(1.0, 1.0, 1.0);
            if button.action == Key::Time {
                button.render(
                    &c,
                    left_edge,
                    button_width.ceil() as u64 * 3,
                    bot,
                    top,
                    pixel_shift_y,
                    &label_fonts,
                    FONT_SIZE,
                );
            } else {
                button.render(
                    &c,
                    left_edge,
                    button_width.ceil() as u64,
                    bot,
                    top,
                    pixel_shift_y,
                    &label_fonts,
                    FONT_SIZE,
                );
            }

            button.changed = false;

            if !complete_redraw {
                if button.action == Key::Time {
                    modified_regions.push(ClipRect::new(
                        height as u16 - top as u16 - radius as u16,
                        left_edge as u16,
                        height as u16 - bot as u16 + radius as u16,
                        left_edge as u16 + button_width as u16 * 3
                    ));
                } else {
                    modified_regions.push(ClipRect::new(
                        height as u16 - top as u16 - radius as u16,
                        left_edge as u16,
                        height as u16 - bot as u16 + radius as u16,
                        left_edge as u16 + button_width as u16
                    ));
                }
            }
        }

        modified_regions
    }
}

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let mode = flags & O_ACCMODE;

        OpenOptions::new()
            .custom_flags(flags)
            .read(mode == O_RDONLY || mode == O_RDWR)
            .write(mode == O_WRONLY || mode == O_RDWR)
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        _ = File::from(fd);
    }
}


fn button_width_px(num: u32, width: u16) -> f64 {
    (width as i32 - (BUTTON_SPACING_PX * (num.saturating_sub(1)) as i32)) as f64 / num as f64
}

/// Which button (if any) is at logical x, accounting for spacing between buttons.
fn button_at_x(num: u32, width: u16, x: f64) -> Option<u32> {
    let button_width = button_width_px(num, width);
    for i in 0..num {
        let left_edge = i as f64 * (button_width + BUTTON_SPACING_PX as f64);
        if x >= left_edge && x <= left_edge + button_width {
            return Some(i);
        }
    }
    None
}

fn touchbar_device_name(name: &str) -> bool {
    name.contains(" Touch Bar") || name.contains("iBridge")
}

struct ActiveTouch {
    layer: usize,
    button: u32,
    pending_command: Option<String>,
}

fn release_button<F>(
    uinput: &mut UInputHandle<F>,
    layers: &mut [FunctionLayer],
    layer: usize,
    button: u32,
) where F: AsRawFd {
    if let Some(button) = layers
        .get_mut(layer)
        .and_then(|layer| layer.buttons.get_mut(button as usize))
    {
        button.set_active(uinput, false);
    }
}

fn release_all_held_keys<F>(
    uinput: &mut UInputHandle<F>,
    touches: &mut HashMap<u32, ActiveTouch>,
    layers: &mut [FunctionLayer],
) where F: AsRawFd {
    for (_, touch) in touches.drain() {
        release_button(uinput, layers, touch.layer, touch.button);
    }
}

/// Clear visual highlights that outlived their touch (no matching slot in `touches`).
fn clear_orphan_highlights<F>(
    uinput: &mut UInputHandle<F>,
    touches: &HashMap<u32, ActiveTouch>,
    layers: &mut [FunctionLayer],
) where F: AsRawFd {
    if !touches.is_empty() {
        return;
    }
    for layer in layers {
        for button in &mut layer.buttons {
            if button.active {
                button.set_active(uinput, false);
            }
        }
    }
}

fn emit<F>(uinput: &mut UInputHandle<F>, ty: EventKind, code: u16, value: i32) where F: AsRawFd {
    uinput.write(&[input_event {
        value: value,
        type_: ty as u16,
        code: code,
        time: timeval {
            tv_sec: 0,
            tv_usec: 0
        }
    }]).unwrap();
}

fn toggle_key<F>(uinput: &mut UInputHandle<F>, code: Key, value: i32) where F: AsRawFd {
    emit(uinput, EventKind::Key, code as u16, value);
    emit(uinput, EventKind::Synchronize, SynchronizeKind::Report as u16, 0);
}

fn run_editor() -> i32 {
    const EDITOR_SCRIPT: &str = include_str!("../touchbar-layout-editor.py");
    const RENDER_HELPER: &str = include_str!("../touchbar_render.py");

    let editor_dir = env::temp_dir().join(format!("tiny-dfr-editor-{}", process::id()));
    if let Err(err) = std::fs::create_dir_all(&editor_dir) {
        eprintln!("tiny-dfr edit: failed to create {}: {err}", editor_dir.display());
        return 1;
    }
    let editor_path = editor_dir.join("touchbar-layout-editor.py");
    let helper_path = editor_dir.join("touchbar_render.py");
    if let Err(err) = std::fs::write(&editor_path, EDITOR_SCRIPT)
        .and_then(|_| std::fs::write(&helper_path, RENDER_HELPER))
    {
        eprintln!("tiny-dfr edit: failed to write embedded editor: {err}");
        return 1;
    }

    let status = Command::new("python3")
        .arg(&editor_path)
        .env("TINY_DFR_EDITOR_EMBEDDED", "1")
        .status()
        .or_else(|_| {
            Command::new("python")
                .arg(&editor_path)
                .env("TINY_DFR_EDITOR_EMBEDDED", "1")
                .status()
        });

    match status {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("tiny-dfr edit: failed to start Python editor: {err}");
            1
        }
    }
}

fn spawn_command_runner() -> Sender<String> {
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || run_command_worker(rx));
    tx
}

fn wayland_session_user() -> Option<(String, u32)> {
    let entries = std::fs::read_dir("/run/user").ok()?;
    for entry in entries.flatten() {
        let uid: u32 = entry.file_name().to_string_lossy().parse().ok()?;
        if uid < 1000 {
            continue;
        }
        let runtime = entry.path();
        if !runtime.join("wayland-0").exists() {
            continue;
        }
        let output = Command::new("id")
            .args(["-nu", &uid.to_string()])
            .output()
            .ok()?;
        let user = String::from_utf8(output.stdout).ok()?;
        let user = user.trim().to_string();
        if user.is_empty() {
            continue;
        }
        return Some((user, uid));
    }
    None
}

fn run_shell_command(command: &str) {
    if let Some((user, uid)) = wayland_session_user() {
        let runtime = format!("/run/user/{uid}");
        let mut cmd = Command::new("runuser");
        cmd.args(["-u", &user, "--", "env"]);
        cmd.arg(format!("XDG_RUNTIME_DIR={runtime}"));
        cmd.arg("WAYLAND_DISPLAY=wayland-0");
        cmd.arg("sh").arg("-c").arg(command);
        match cmd.status() {
            Ok(status) if !status.success() => {
                eprintln!(
                    "tiny-dfr: command button `{}` exited with {}",
                    command,
                    status
                );
            }
            Err(err) => {
                eprintln!("tiny-dfr: command button failed to start `{}`: {}", command, err);
            }
            _ => {}
        }
        return;
    }
    if let Err(err) = Command::new("sh").arg("-c").arg(command).status() {
        eprintln!("tiny-dfr: command button failed to start `{}`: {}", command, err);
    }
}

fn run_command_worker(rx: Receiver<String>) {
    while let Ok(command) = rx.recv() {
        let command = command.trim();
        if command.is_empty() {
            continue;
        }
        let command = command.to_string();
        thread::spawn(move || run_shell_command(&command));
    }
}

fn init_control_socket() -> Option<UnixDatagram> {
    let dir = Path::new("/run/tiny-dfr");
    let path = dir.join("control.sock");
    if let Err(err) = std::fs::create_dir_all(dir) {
        eprintln!("tiny-dfr: control socket disabled, cannot create {}: {err}", dir.display());
        return None;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            eprintln!("tiny-dfr: control socket disabled, cannot replace {}: {err}", path.display());
            return None;
        }
    }
    let socket = match UnixDatagram::bind(&path) {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("tiny-dfr: control socket disabled, bind failed: {err}");
            return None;
        }
    };
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666));
    if let Err(err) = socket.set_nonblocking(true) {
        eprintln!("tiny-dfr: control socket disabled, nonblocking failed: {err}");
        return None;
    }
    Some(socket)
}

fn requested_control_layer(socket: &UnixDatagram) -> Option<usize> {
    let mut buf = [0u8; 128];
    let mut requested = None;
    loop {
        match socket.recv(&mut buf) {
            Ok(len) => {
                let msg = String::from_utf8_lossy(&buf[..len]);
                let mut parts = msg.split_whitespace();
                let Some(cmd) = parts.next() else { continue; };
                if cmd.eq_ignore_ascii_case("layer") || cmd.eq_ignore_ascii_case("bar") {
                    if let Some(layer) = parts.next().and_then(|part| part.parse::<usize>().ok()) {
                        requested = Some(layer);
                    }
                } else if cmd.eq_ignore_ascii_case("default") {
                    requested = Some(0);
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => break,
            Err(err) => {
                eprintln!("tiny-dfr: control socket read failed: {err}");
                break;
            }
        }
    }
    requested
}

fn mark_layer_dirty(layers: &mut [FunctionLayer], layer: usize) {
    if let Some(layer) = layers.get_mut(layer) {
        for button in &mut layer.buttons {
            button.changed = true;
        }
    }
}

fn mark_all_layers_dirty(layers: &mut [FunctionLayer]) {
    for layer in layers.iter_mut() {
        for button in &mut layer.buttons {
            button.changed = true;
        }
    }
}

fn resolve_layer_index(requested_layer: usize, layer_count: usize) -> Option<usize> {
    if requested_layer < layer_count {
        Some(requested_layer)
    } else {
        eprintln!(
            "tiny-dfr: SwitchToLayer {requested_layer} is invalid (layers 0..{})",
            layer_count.saturating_sub(1)
        );
        None
    }
}

fn set_active_layer<F>(
    uinput: &mut UInputHandle<F>,
    touches: &mut HashMap<u32, ActiveTouch>,
    layers: &mut [FunctionLayer],
    active_layer: &mut usize,
    requested_layer: usize,
    needs_complete_redraw: &mut bool,
    release_keys: bool,
) where F: AsRawFd {
    let Some(new_layer) = resolve_layer_index(requested_layer, layers.len()) else {
        return;
    };
    if *active_layer != new_layer {
        if release_keys {
            release_all_held_keys(uinput, touches, layers);
        }
        *active_layer = new_layer;
    }
    mark_layer_dirty(layers, new_layer);
    *needs_complete_redraw = true;
}

/// App/custom bars (layer index >= 2). Layers 0/1 are the default ↔ Fn pair only.
fn apply_layer_selection<F>(
    uinput: &mut UInputHandle<F>,
    touches: &mut HashMap<u32, ActiveTouch>,
    layers: &mut [FunctionLayer],
    active_layer: &mut usize,
    overlay_layer: &mut Option<usize>,
    fn_toggle_active: &mut bool,
    fn_layer: usize,
    requested_layer: usize,
    needs_complete_redraw: &mut bool,
    release_keys: bool,
) where F: AsRawFd {
    let Some(new_layer) = resolve_layer_index(requested_layer, layers.len()) else {
        return;
    };
    if new_layer <= 1 {
        *overlay_layer = None;
        *fn_toggle_active = new_layer == fn_layer;
    } else {
        *overlay_layer = Some(new_layer);
    }
    set_active_layer(
        uinput,
        touches,
        layers,
        active_layer,
        new_layer,
        needs_complete_redraw,
        release_keys,
    );
}

fn baseline_layer(
    overlay_layer: Option<usize>,
    fn_toggle_active: bool,
    default_layer: usize,
    fn_layer: usize,
) -> usize {
    if let Some(layer) = overlay_layer {
        return layer;
    }
    fn_toggle_target(fn_toggle_active, default_layer, fn_layer)
}

fn fn_toggle_target(fn_toggle_active: bool, default_layer: usize, fn_layer: usize) -> usize {
    if fn_toggle_active {
        fn_layer
    } else {
        default_layer
    }
}

fn layer_for_shortcut(cfg: &Config, key: u32, fn_pressed: bool, modifiers: &KeyboardModifiers) -> Option<usize> {
    cfg.layer_shortcuts
        .iter()
        .filter(|shortcut| shortcut.matches(key, fn_pressed, modifiers))
        .max_by_key(|shortcut| shortcut.match_priority())
        .map(|shortcut| shortcut.layer)
}

fn main() {
    if env::args().skip(1).any(|arg| arg == "edit" || arg == "--edit") {
        process::exit(run_editor());
    }

    let drm = match DrmBackend::open_card() {
        Ok(drm) => drm,
        Err(err) => {
            eprintln!("tiny-dfr: failed to acquire touchbar DRM device: {:#}", err);
            return;
        }
    };
    let (height, width) = drm.mode().size();
    let (db_width, db_height) = drm.fb_info().unwrap().size();
    let redraw = DrmRedrawHandle::spawn(drm);
    let _ = panic::catch_unwind(AssertUnwindSafe(|| {
        real_main(&redraw, width, height, db_width, db_height)
    }));
    if let Ok(mut drm) = DrmBackend::open_card() {
        let crash_bitmap = include_bytes!("crash_bitmap.raw");
        let mut map = drm.map().unwrap();
        let data = map.as_mut();
        let mut wptr = 0;
        for byte in crash_bitmap {
            for i in 0..8 {
                let bit = ((byte >> i) & 0x1) == 0;
                let color = if bit { 0xFF } else { 0x0 };
                data[wptr] = color;
                data[wptr + 1] = color;
                data[wptr + 2] = color;
                data[wptr + 3] = color;
                wptr += 4;
            }
        }
        drop(map);
        let _ = drm.dirty(&[ClipRect::new(0, 0, height as u16, width as u16)]);
    }
    let mut sigset = SigSet::empty();
    sigset.add(Signal::SIGTERM);
    sigset.wait().unwrap();
}

fn real_main(
    redraw: &DrmRedrawHandle,
    width: u16,
    height: u16,
    db_width: u32,
    db_height: u32,
) {
    let mut cfg_mgr = ConfigManager::new();
    let (mut cfg, mut layers) = cfg_mgr.load_config(width).unwrap_or_else(|err| {
        eprintln!("tiny-dfr: failed to load configuration: {err}");
        process::exit(1);
    });

    // Open sysfs backlights as root before privdrop (nodes are root-only on many systems).
    let mut backlight = BacklightManager::new();
    let mut uinput = UInputHandle::new(OpenOptions::new().write(true).open("/dev/uinput").unwrap());
    uinput.set_evbit(EventKind::Key).unwrap();
    for layer in &layers {
        for button in &layer.buttons {
            if button.emits_key() {
                uinput.set_keybit(button.action).unwrap();
            }
        }
    }
    let mut dev_name_c = [0 as c_char; 80];
    let dev_name = "Dynamic Function Row Virtual Input Device".as_bytes();
    for i in 0..dev_name.len() {
        dev_name_c[i] = dev_name[i] as c_char;
    }
    uinput.dev_setup(&uinput_setup {
        id: input_id {
            bustype: 0x19,
            vendor: 0x1209,
            product: 0x316E,
            version: 1
        },
        ff_effects_max: 0,
        name: dev_name_c
    }).unwrap();
    uinput.dev_create().unwrap();

    let command_tx = if cfg.allow_root_commands {
        eprintln!(
            "tiny-dfr: AllowRootCommands=true; command buttons execute as root. Only use trusted config."
        );
        Some(spawn_command_runner())
    } else {
        None
    };
    let control_socket = init_control_socket();

    if !cfg.allow_root_commands {
        PrivDrop::default()
            .user("nobody")
            .group_list(&["input", "video"])
            .apply()
            .unwrap_or_else(|e| panic!("Failed to drop privileges: {}", e));
    }

    let mut last_redraw_minute = Local::now().minute();
    let mut pixel_shift = PixelShiftManager::new();
    let mut surface = ImageSurface::create(Format::ARgb32, db_width as i32, db_height as i32).unwrap();
    let mut active_layer = 0;
    let mut needs_complete_redraw = true;

    let mut input_tb = Libinput::new_with_udev(Interface);
    let mut input_main = Libinput::new_with_udev(Interface);
    input_tb.udev_assign_seat("seat-touchbar").unwrap();
    input_main.udev_assign_seat("seat0").unwrap();
    let epoll = Epoll::new(EpollCreateFlags::empty()).unwrap();
    epoll.add(input_main.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 0)).unwrap();
    epoll.add(input_tb.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 1)).unwrap();
    epoll.add(cfg_mgr.fd(), EpollEvent::new(EpollFlags::EPOLLIN, 2)).unwrap();

    // Pick up devices that were already present before we started listening.
    input_tb.dispatch().unwrap();
    input_main.dispatch().unwrap();
    let mut digitizer: Option<InputDevice> = None;
    for event in input_tb.clone().chain(input_main.clone()) {
        if let Event::Device(DeviceEvent::Added(evt)) = event {
            let dev = evt.device();
            if touchbar_device_name(dev.name()) {
                digitizer = Some(dev);
            }
        }
    }

    let default_layer: usize = 0;
    let fn_layer: usize = 1;
    let mut touches: HashMap<u32, ActiveTouch> = HashMap::new();
    let mut fn_pressed = false;
    let mut fn_pressed_at: Option<Instant> = None;
    let mut fn_toggle_active = false;
    let mut fn_shortcut_used = false;
    let mut keyboard_modifiers = KeyboardModifiers::default();
    let mut overlay_layer: Option<usize> = None;
    let mut last_drawn_layer = usize::MAX;
    loop {
        if cfg_mgr.update_config(&mut cfg, &mut layers, width) {
            release_all_held_keys(&mut uinput, &mut touches, &mut layers);
            active_layer = default_layer;
            fn_pressed = false;
            fn_pressed_at = None;
            fn_toggle_active = false;
            fn_shortcut_used = false;
            overlay_layer = None;
            last_drawn_layer = usize::MAX;
            needs_complete_redraw = true;
        }

        if active_layer != last_drawn_layer {
            needs_complete_redraw = true;
        }

        let mut idle_timeout_ms = TIMEOUT_MS;
        if cfg.enable_pixel_shift {
            let (pixel_shift_needs_redraw, pixel_shift_next_timeout_ms) = pixel_shift.update();
            if pixel_shift_needs_redraw {
                needs_complete_redraw = true;
            }
            idle_timeout_ms = min(idle_timeout_ms, pixel_shift_next_timeout_ms);
        }

        let current_minute = Local::now().minute();
        for button in &mut layers[active_layer].buttons {
            if (button.action == Key::Time) && (current_minute != last_redraw_minute) {
                needs_complete_redraw = true;
                last_redraw_minute = current_minute;
            }
        }

        let poll_timeout_ms = if !touches.is_empty()
            || needs_complete_redraw
            || layers[active_layer].buttons.iter().any(|b| b.changed)
        {
            INTERACTIVE_POLL_MS
        } else {
            idle_timeout_ms
        };

        // Process input first. drm.dirty() can block on USB for hundreds of ms; if we
        // redraw before reading events, TouchUp is delayed and keys stay held (volume
        // repeats, buttons feel stuck).
        match epoll.wait(
            &mut [EpollEvent::new(EpollFlags::EPOLLIN, 0)],
            poll_timeout_ms as isize,
        ) {
            Err(Errno::EINTR) | Ok(_) => {}
            Err(e) => panic!("epoll wait failed: {e}"),
        }

        // Drain all pending libinput events before any blocking DRM work.
        for _ in 0..32 {
            input_tb.dispatch().unwrap();
            input_main.dispatch().unwrap();
            let events: Vec<_> = input_tb.clone().chain(input_main.clone()).collect();
            if events.is_empty() {
                break;
            }
            for event in events {
            backlight.process_event(&event);
            match event {
                Event::Device(DeviceEvent::Added(evt)) => {
                    let dev = evt.device();
                    if touchbar_device_name(dev.name()) {
                        digitizer = Some(dev);
                    }
                },
                Event::Device(DeviceEvent::Removed(evt)) => {
                    if Some(evt.device()) == digitizer {
                        release_all_held_keys(&mut uinput, &mut touches, &mut layers);
                        digitizer = None;
                        needs_complete_redraw = true;
                    }
                },
                Event::Keyboard(KeyboardEvent::Key(key)) => {
                    let key_code = key.key();
                    if key_code != Key::Fn as u32 {
                        keyboard_modifiers.update(key_code, key.key_state() == KeyState::Pressed);
                    }
                    if key_code == Key::Fn as u32 {
                        match key.key_state() {
                            KeyState::Pressed => {
                                fn_pressed = true;
                                fn_pressed_at = Some(Instant::now());
                                fn_shortcut_used = false;
                                match cfg.fn_mode {
                                    FnMode::Hold | FnMode::Smart => set_active_layer(
                                        &mut uinput,
                                        &mut touches,
                                        &mut layers,
                                        &mut active_layer,
                                        fn_layer,
                                        &mut needs_complete_redraw,
                                        true,
                                    ),
                                    FnMode::Toggle => {
                                        fn_toggle_active = !fn_toggle_active;
                                        set_active_layer(
                                            &mut uinput,
                                            &mut touches,
                                            &mut layers,
                                            &mut active_layer,
                                            fn_toggle_target(
                                                fn_toggle_active,
                                                default_layer,
                                                fn_layer,
                                            ),
                                            &mut needs_complete_redraw,
                                            true,
                                        );
                                    }
                                }
                            }
                            KeyState::Released => {
                                fn_pressed = false;
                                if !fn_shortcut_used {
                                    match cfg.fn_mode {
                                        FnMode::Hold => set_active_layer(
                                            &mut uinput,
                                            &mut touches,
                                            &mut layers,
                                            &mut active_layer,
                                            baseline_layer(
                                                overlay_layer,
                                                fn_toggle_active,
                                                default_layer,
                                                fn_layer,
                                            ),
                                            &mut needs_complete_redraw,
                                            true,
                                        ),
                                        FnMode::Smart => {
                                            let elapsed = fn_pressed_at
                                                .map(|instant| instant.elapsed())
                                                .unwrap_or(Duration::from_millis(u64::MAX));
                                            let quick_tap = elapsed
                                                <= Duration::from_millis(cfg.fn_toggle_press_ms);
                                            if quick_tap {
                                                fn_toggle_active = !fn_toggle_active;
                                                set_active_layer(
                                                    &mut uinput,
                                                    &mut touches,
                                                    &mut layers,
                                                    &mut active_layer,
                                                    fn_toggle_target(
                                                        fn_toggle_active,
                                                        default_layer,
                                                        fn_layer,
                                                    ),
                                                    &mut needs_complete_redraw,
                                                    true,
                                                );
                                            } else {
                                                set_active_layer(
                                                    &mut uinput,
                                                    &mut touches,
                                                    &mut layers,
                                                    &mut active_layer,
                                                    baseline_layer(
                                                        overlay_layer,
                                                        fn_toggle_active,
                                                        default_layer,
                                                        fn_layer,
                                                    ),
                                                    &mut needs_complete_redraw,
                                                    true,
                                                );
                                            }
                                        }
                                        FnMode::Toggle => {}
                                    }
                                }
                                fn_pressed_at = None;
                                fn_shortcut_used = false;
                            }
                        }
                    } else if key.key_state() == KeyState::Pressed {
                        if cfg.redraw_shortcut.matches(
                            key_code,
                            fn_pressed,
                            &keyboard_modifiers,
                            false,
                        ) {
                            mark_all_layers_dirty(&mut layers);
                            needs_complete_redraw = true;
                        } else if let Some(layer) = layer_for_shortcut(
                            &cfg,
                            key_code,
                            fn_pressed,
                            &keyboard_modifiers,
                        ) {
                            fn_shortcut_used = true;
                            apply_layer_selection(
                                &mut uinput,
                                &mut touches,
                                &mut layers,
                                &mut active_layer,
                                &mut overlay_layer,
                                &mut fn_toggle_active,
                                fn_layer,
                                layer,
                                &mut needs_complete_redraw,
                                true,
                            );
                        } else if key_code == Key::Macro1 as u32 {
                            apply_layer_selection(
                                &mut uinput,
                                &mut touches,
                                &mut layers,
                                &mut active_layer,
                                &mut overlay_layer,
                                &mut fn_toggle_active,
                                fn_layer,
                                if cfg.media_layer_default {
                                    default_layer
                                } else {
                                    fn_layer
                                },
                                &mut needs_complete_redraw,
                                true,
                            );
                        } else if key_code == Key::Macro2 as u32 {
                            apply_layer_selection(
                                &mut uinput,
                                &mut touches,
                                &mut layers,
                                &mut active_layer,
                                &mut overlay_layer,
                                &mut fn_toggle_active,
                                fn_layer,
                                2,
                                &mut needs_complete_redraw,
                                true,
                            );
                        } else if key_code == Key::Macro3 as u32 {
                            apply_layer_selection(
                                &mut uinput,
                                &mut touches,
                                &mut layers,
                                &mut active_layer,
                                &mut overlay_layer,
                                &mut fn_toggle_active,
                                fn_layer,
                                3,
                                &mut needs_complete_redraw,
                                true,
                            );
                        }
                    }
                },
                Event::Touch(te) => {
                    if Some(te.device()) != digitizer {
                        continue;
                    }
                    match te {
                        TouchEvent::Down(dn) => {
                            let slot = dn.seat_slot();
                            if let Some(previous) = touches.remove(&slot) {
                                release_button(&mut uinput, &mut layers, previous.layer, previous.button);
                            }
                            let x = dn.x_transformed(width as u32);
                            let num = layers[active_layer].buttons.len() as u32;
                            let Some(btn) = button_at_x(num, width, x) else {
                                continue;
                            };
                            let button = &mut layers[active_layer].buttons[btn as usize];
                            if !button.is_interactive() {
                                continue;
                            }
                            let command = button.command.clone();
                            let switch_to_layer = button.switch_to_layer;
                            let touch_layer = active_layer;
                            button.set_active(&mut uinput, true);
                            if let Some(layer) = switch_to_layer {
                                apply_layer_selection(
                                    &mut uinput,
                                    &mut touches,
                                    &mut layers,
                                    &mut active_layer,
                                    &mut overlay_layer,
                                    &mut fn_toggle_active,
                                    fn_layer,
                                    layer,
                                    &mut needs_complete_redraw,
                                    false,
                                );
                            }
                            touches.insert(slot, ActiveTouch {
                                layer: touch_layer,
                                button: btn,
                                pending_command: command,
                            });
                        },
                        TouchEvent::Up(up) => {
                            let Some(touch) = touches.remove(&up.seat_slot()) else {
                                continue;
                            };
                            release_button(&mut uinput, &mut layers, touch.layer, touch.button);
                            if let Some(command) = touch.pending_command {
                                if let Some(tx) = &command_tx {
                                    let _ = tx.send(command);
                                } else {
                                    eprintln!(
                                        "tiny-dfr: command button ignored because AllowRootCommands=false"
                                    );
                                }
                            }
                        },
                        TouchEvent::Cancel(_) => {
                            release_all_held_keys(&mut uinput, &mut touches, &mut layers);
                        },
                        // Ignore motion: no key or redraw churn from finger jitter.
                        _ => {}
                    }
                },
                _ => {}
            }
            }
        }
        if let Some(socket) = &control_socket {
            if let Some(layer) = requested_control_layer(socket) {
                apply_layer_selection(
                    &mut uinput,
                    &mut touches,
                    &mut layers,
                    &mut active_layer,
                    &mut overlay_layer,
                    &mut fn_toggle_active,
                    fn_layer,
                    layer,
                    &mut needs_complete_redraw,
                    true,
                );
            }
        }
        clear_orphan_highlights(&mut uinput, &touches, &mut layers);
        backlight.update_backlight(&cfg);

        let layer_changed = active_layer != last_drawn_layer;
        let complete_redraw = needs_complete_redraw || layer_changed;
        if complete_redraw || layers[active_layer].buttons.iter().any(|b| b.changed) {
            let shift = if cfg.enable_pixel_shift {
                pixel_shift.get()
            } else {
                (0.0, 0.0)
            };
            let clips = layers[active_layer].draw(
                &cfg,
                width as i32,
                height as i32,
                &surface,
                shift,
                complete_redraw,
            );
            let data = surface.data().unwrap();
            redraw.try_push(data.to_vec(), clips);
            last_drawn_layer = active_layer;
            needs_complete_redraw = false;
        }
    }
}
