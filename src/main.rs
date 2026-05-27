use std::{
    fs::{File, OpenOptions},
    os::{
        fd::{AsRawFd, AsFd},
        unix::{io::OwnedFd, fs::OpenOptionsExt}
    },
    path::{Path, PathBuf},
    collections::HashMap,
    cmp::min,
    panic::{self, AssertUnwindSafe},
};
use cairo::{ImageSurface, Format, Context, Surface, Rectangle, FontSlant, FontWeight, Antialias};
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
mod config;

use backlight::BacklightManager;
use display::{DrmBackend, DrmRedrawHandle};
use pixel_shift::{PixelShiftManager, PIXEL_SHIFT_WIDTH_PX};
use config::{ButtonConfig, Config};
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
    background: bool
}

fn load_image(icon_name: &str, mode: Option<String>, path: &str) -> Result<ButtonImage> {
    if path != "use_default" {
        return Err(anyhow!("Custom path defined, using that"));
    }
    let theme = ConfigManager::new().load_theme();
    let icon_theme = match mode {
        Some(mode_val) => {
            if mode_val == "App" {theme.app_icon_theme} else {theme.media_icon_theme}
        }
        None => {
            panic!("No mode specified")
        }
    };
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
    let icon_loader;
    match loader.load_icon(icon_name) {
        Some(icon) => {
            icon_loader = icon;
        }
        None => {
            match loader.load_icon(format!("{}.svg", icon_name)) {
                Some(icon) => {
                    icon_loader = icon;
                }
                None => {
                    match loader.load_icon(format!("{}.png", icon_name)) {
                        Some(icon) => {
                            icon_loader = icon;
                        }
                        None => {
                            return Err(anyhow!("Icon not found: {}, trying /usr/share/pixmaps", icon_name));
                        }
                    }
                }
            }
        }
    };
    let icon = icon_loader.file_for_size(256);
    match icon.icon_type() {
        IconFileType::SVG => {
            let handle = Loader::new().read_path(icon.path())?;
            Ok(ButtonImage::Svg(handle))
        }
        IconFileType::PNG => {
            let mut file = File::open(icon.path())?;
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
        IconFileType::XPM => {
            panic!("Legacy XPM icons are not supported")
        }
    }
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
        if let Some(text) = cfg.text {
            if let Some(bg) = cfg.background {
                background = bg;
            } else {
                background = true;
            }
            Button::new_text(text, cfg.action, background)
        } else if let Some(icon) = cfg.icon {
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
            Button::new_icon(&icon, cfg.action, cfg.mode, &path, background)
        } else if let Some(mode) = cfg.mode {
            if let Some(bg) = cfg.background {
                background = bg;
            } else {
                background = false;
            }
            if mode.to_lowercase() == "blank" {
                Button::new_blank(cfg.action, background)
            } else if mode.to_lowercase() == "time" {
                let format = match cfg.format {
                    Some(f) => f,
                    None => "24hr".to_string()
                };
                let locale = match cfg.locale {
                    Some(l) => l,
                    None => "POSIX".to_string()
                };
                Button::new_time(cfg.action, format, locale, background)
            } else {
                panic!("Invalid config, a button must have either Text, Icon or be Blank")
            }
        } else {
            panic!("Invalid config, a button must have either Text, Icon or be Blank")
        }
    }
    fn new_text(text: String, action: Key, background: bool) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Text(text),
            background
        }
    }
    fn new_icon(icon_name: &str, action: Key, mode: Option<String>, path: &str, background: bool) -> Button {
        let image = load_image(icon_name, mode, path)
            .or_else(|_| try_load_svg_path(icon_name, path))
            .or_else(|_| try_load_png_path(icon_name, path))
            .unwrap_or_else(|_| ButtonImage::Text(icon_name.to_string()));
        Button {
            action, image,
            active: false,
            changed: false,
            background
        }
    }
    fn new_time(action: Key, format: String, locale: String, background: bool) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Time(format, locale),
            background
        }
    }
    fn new_blank(action: Key, background: bool) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Blank,
            background
        }
    }
    fn render(&self, c: &Context, height: i32, button_left_edge: f64, button_width: u64, y_shift: f64) {
        match &self.image {
            ButtonImage::Text(text) => {
                let extents = c.text_extents(text).unwrap();
                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round()
                );
                c.show_text(text).unwrap();
            },
            ButtonImage::Svg(svg) => {
                let renderer = CairoRenderer::new(&svg);
                let x = button_left_edge + (button_width as f64 / 2.0 - (ICON_SIZE / 2) as f64).round();
                let y = y_shift + ((height as f64 - ICON_SIZE as f64) / 2.0).round();

                renderer.render_document(c,
                    &Rectangle::new(x, y, ICON_SIZE as f64, ICON_SIZE as f64)
                ).unwrap();
            }
            ButtonImage::Bitmap(surf) => {
                let x = button_left_edge + (button_width as f64 / 2.0 - (ICON_SIZE / 2) as f64).round();
                let y = y_shift + ((height as f64 - ICON_SIZE as f64) / 2.0).round();
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
                let time_extents = c.text_extents(&formatted_time).unwrap();
                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - time_extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + time_extents.height() / 2.0).round()
                );
                c.show_text(&formatted_time).unwrap();
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

    /// Send a single key press+release (tap). Avoids kernel key-repeat if TouchUp is late.
    fn tap_key<F>(&self, uinput: &mut UInputHandle<F>) where F: AsRawFd {
        toggle_key(uinput, self.action, 1);
        toggle_key(uinput, self.action, 0);
    }

    fn set_active<F>(&mut self, uinput: &mut UInputHandle<F>, active: bool) where F: AsRawFd {
        if self.active != active {
            self.highlight(active);
            toggle_key(uinput, self.action, active as i32);
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
        if config.font_renderer.to_lowercase() == "cairo" {
            c.select_font_face(&config.font_style_cairo, if config.italic_cairo {FontSlant::Italic} else {FontSlant::Normal}, if config.bold_cairo {FontWeight::Bold} else {FontWeight::Normal});
        } else if config.font_renderer.to_lowercase() == "freetype" {
            c.set_font_face(&config.font_face);
        } else { panic!("Invalid font renderer chosen. Choose between \"Cairo\" and \"FreeType\""); }
        c.set_font_size(32.0);
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
            if (button.action != Key::Unknown &&
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
                button.render(&c, height, left_edge, button_width.ceil() as u64 * 3, pixel_shift_y);
            } else {
                button.render(&c, height, left_edge, button_width.ceil() as u64, pixel_shift_y);
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

fn release_all_held_keys(
    touches: &mut HashMap<u32, (usize, u32)>,
    layers: &mut [FunctionLayer],
) {
    for (_, (layer, btn)) in touches.drain() {
        layers[layer].buttons[btn as usize].highlight(false);
    }
}

/// Clear visual highlights that outlived their touch (no matching slot in `touches`).
fn clear_orphan_highlights(layers: &mut [FunctionLayer]) {
    for layer in layers {
        for button in &mut layer.buttons {
            if button.active {
                button.highlight(false);
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

fn main() {
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
    let (mut cfg, mut layers) = cfg_mgr.load_config(width);

    // Open sysfs backlights as root before privdrop (nodes are root-only on many systems).
    let mut backlight = BacklightManager::new();
    let mut uinput = UInputHandle::new(OpenOptions::new().write(true).open("/dev/uinput").unwrap());
    uinput.set_evbit(EventKind::Key).unwrap();
    for layer in &layers {
        for button in &layer.buttons {
            uinput.set_keybit(button.action).unwrap();
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

    PrivDrop::default()
        .user("nobody")
        .group_list(&["input", "video"])
        .apply()
        .unwrap_or_else(|e| panic!("Failed to drop privileges: {}", e));

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
    let mut touches: HashMap<u32, (usize, u32)> = HashMap::new();
    loop {
        if cfg_mgr.update_config(&mut cfg, &mut layers, width) {
            release_all_held_keys(&mut touches, &mut layers);
            active_layer = default_layer;
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
                Event::Keyboard(KeyboardEvent::Key(key)) => {
                    if key.key() == Key::Fn as u32 {
                        let new_layer = match key.key_state() {
                            KeyState::Pressed => fn_layer,
                            KeyState::Released => default_layer,
                        };
                        if active_layer != new_layer {
                            release_all_held_keys(&mut touches, &mut layers);
                            active_layer = new_layer;
                            needs_complete_redraw = true;
                        }
                    } else if key.key() == Key::Macro1 as u32 && key.key_state() == KeyState::Pressed {
                        release_all_held_keys(&mut touches, &mut layers);
                        active_layer = if cfg.media_layer_default { default_layer } else { fn_layer };
                        needs_complete_redraw = true;
                    } else if key.key() == Key::Macro2 as u32 && key.key_state() == KeyState::Pressed {
                        release_all_held_keys(&mut touches, &mut layers);
                        active_layer = 2;
                        needs_complete_redraw = true;
                    } else if key.key() == Key::Macro3 as u32 && key.key_state() == KeyState::Pressed {
                        release_all_held_keys(&mut touches, &mut layers);
                        active_layer = 3;
                        needs_complete_redraw = true;
                    }
                },
                Event::Touch(te) => {
                    if Some(te.device()) != digitizer {
                        continue;
                    }
                    match te {
                        TouchEvent::Down(dn) => {
                            let x = dn.x_transformed(width as u32);
                            let num = layers[active_layer].buttons.len() as u32;
                            let Some(btn) = button_at_x(num, width, x) else {
                                continue;
                            };
                            let button = &mut layers[active_layer].buttons[btn as usize];
                            if button.action == Key::Unknown || button.action == Key::Time {
                                continue;
                            }
                            touches.insert(dn.seat_slot(), (active_layer, btn));
                            // Same path as volume/play: uinput KEY_BRIGHTNESS* so GNOME/UPower
                            // handles brightness and shows the on-screen notification.
                            button.tap_key(&mut uinput);
                            button.highlight(true);
                        },
                        TouchEvent::Up(up) => {
                            let Some((layer, btn)) = touches.remove(&up.seat_slot()) else {
                                continue;
                            };
                            let button = &mut layers[layer].buttons[btn as usize];
                            if button.action == Key::Unknown || button.action == Key::Time {
                                continue;
                            }
                            button.highlight(false);
                        },
                        // Ignore motion: no key or redraw churn from finger jitter.
                        _ => {}
                    }
                },
                _ => {}
            }
            }
        }
        if touches.is_empty() {
            clear_orphan_highlights(&mut layers);
        }
        backlight.update_backlight(&cfg);

        if needs_complete_redraw || layers[active_layer].buttons.iter().any(|b| b.changed) {
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
                needs_complete_redraw,
            );
            let data = surface.data().unwrap();
            redraw.try_push(data.to_vec(), clips);
            needs_complete_redraw = false;
        }
    }
}
