use crate::fonts::{FontConfig, Pattern};
use crate::{Button, FunctionLayer};
use anyhow::Error;
use cairo::FontFace;
use freetype::Library as FtLibrary;
use input_linux::Key;
use nix::{
    errno::Errno,
    sys::inotify::{AddWatchFlags, InitFlags, Inotify, WatchDescriptor},
};
use serde::Deserialize;
use std::{fs::read_to_string, os::fd::AsFd};

const USER_CFG_PATH: &'static str = "/etc/tiny-dfr/config.toml";

pub struct Config {
    pub media_layer_default: bool,
    pub show_button_outlines: bool,
    pub enable_pixel_shift: bool,
    pub font_renderer: String,
    pub font_style_cairo: String,
    pub bold_cairo: bool,
    pub italic_cairo: bool,
    pub font_face: FontFace,
    pub emoji_font_face: Option<FontFace>,
    pub adaptive_brightness: bool,
    pub active_brightness: u32,
    pub dim_timeout_ms: u64,
    pub off_timeout_ms: u64,
    pub dimmed_brightness: u32,
    pub fn_mode: FnMode,
    pub fn_toggle_press_ms: u64,
    pub layer_shortcuts: Vec<LayerShortcut>,
    pub redraw_shortcut: KeyboardShortcut,
    pub allow_root_commands: bool,
}

pub struct Theme {
    pub media_icon_theme: String,
    pub app_icon_theme: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FnMode {
    Hold,
    Toggle,
    Smart,
}

impl FnMode {
    fn from_config(value: Option<String>) -> FnMode {
        match value
            .unwrap_or_else(|| "hold".to_string())
            .to_lowercase()
            .as_str()
        {
            "toggle" => FnMode::Toggle,
            "smart" => FnMode::Smart,
            _ => FnMode::Hold,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConfigProxy {
    media_layer_default: Option<bool>,
    special_extended_mode: Option<bool>,
    show_button_outlines: Option<bool>,
    enable_pixel_shift: Option<bool>,
    font_renderer: Option<String>,
    font_style: Option<String>,
    bold: Option<bool>,
    italic: Option<bool>,
    font_template: Option<String>,
    emoji_font_template: Option<String>,
    media_icon_theme: Option<String>,
    app_icon_theme: Option<String>,
    adaptive_brightness: Option<bool>,
    active_brightness: Option<u32>,
    dim_timeout_ms: Option<u64>,
    off_timeout_ms: Option<u64>,
    dimmed_brightness: Option<u32>,
    fn_mode: Option<String>,
    fn_toggle_press_ms: Option<u64>,
    allow_root_commands: Option<bool>,
    primary_layer_keys: Option<Vec<ButtonConfig>>,
    media_layer_keys: Option<Vec<ButtonConfig>>,
    app_layer_keys1: Option<Vec<ButtonConfig>>,
    app_layer_keys2: Option<Vec<ButtonConfig>>,
    app_layer_keys3: Option<Vec<ButtonConfig>>,
    custom_bars: Option<Vec<Vec<ButtonConfig>>>,
    layer_shortcuts: Option<Vec<LayerShortcut>>,
    redraw_shortcut: Option<KeyboardShortcut>,
    /// Editor-only preset buttons; ignored by the daemon at runtime.
    button_library: Option<Vec<ButtonConfig>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ButtonConfig {
    #[serde(alias = "Svg")]
    pub icon: Option<String>,
    pub path: Option<String>,
    pub mode: Option<String>,
    pub text: Option<String>,
    pub background: Option<bool>,
    pub format: Option<String>,
    pub locale: Option<String>,
    pub command: Option<String>,
    pub switch_to_layer: Option<usize>,
    pub action: Option<String>,
}

/// Map config / editor key names to input-linux `Key` values.
pub fn parse_key_name(name: &str) -> Option<Key> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let canonical = match trimmed {
        "." => "Dot",
        "," => "Comma",
        "<" => "Comma",
        "/" => "Slash",
        "\\" => "Backslash",
        "-" => "Minus",
        "=" => "Equal",
        ";" => "Semicolon",
        "'" => "Apostrophe",
        "[" => "Leftbrace",
        "]" => "Rightbrace",
        "`" => "Grave",
        other => other,
    };
    #[derive(Deserialize)]
    struct KeyWrap {
        action: Key,
    }
    let table = format!("action = \"{canonical}\"");
    match toml::from_str::<KeyWrap>(&table) {
        Ok(wrap) => Some(wrap.action),
        Err(_) => {
            eprintln!("tiny-dfr: unknown Action/Key name `{name}` (ignored)");
            None
        }
    }
}

impl ButtonConfig {
    pub fn resolved_action(&self) -> Key {
        self.action
            .as_deref()
            .and_then(parse_key_name)
            .unwrap_or(Key::Unknown)
    }
}

#[derive(Clone, Copy, Default)]
pub struct KeyboardModifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
}

impl KeyboardModifiers {
    pub fn update(&mut self, key: u32, pressed: bool) {
        let apply = |slot: &mut bool| {
            if pressed {
                *slot = true;
            } else {
                *slot = false;
            }
        };
        match key {
            k if k == Key::LeftCtrl as u32 || k == Key::RightCtrl as u32 => apply(&mut self.ctrl),
            k if k == Key::LeftAlt as u32 || k == Key::RightAlt as u32 => apply(&mut self.alt),
            k if k == Key::LeftShift as u32 || k == Key::RightShift as u32 => apply(&mut self.shift),
            k if k == Key::LeftMeta as u32 || k == Key::RightMeta as u32 => {
                apply(&mut self.super_key)
            }
            _ => {}
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct KeyboardShortcut {
    pub key: Option<String>,
    pub code: Option<u32>,
    #[serde(alias = "Fn")]
    pub require_fn: Option<bool>,
    pub ctrl: Option<bool>,
    pub alt: Option<bool>,
    pub shift: Option<bool>,
    #[serde(alias = "Super")]
    pub super_key: Option<bool>,
}

impl KeyboardShortcut {
    fn modifier_ok(required: Option<bool>, active: bool) -> bool {
        match required {
            Some(true) => active,
            Some(false) => !active,
            None => true,
        }
    }

    pub fn matches(
        &self,
        key: u32,
        fn_pressed: bool,
        modifiers: &KeyboardModifiers,
        default_require_fn: bool,
    ) -> bool {
        match self.require_fn {
            Some(true) if !fn_pressed => return false,
            Some(false) if fn_pressed => return false,
            None if default_require_fn && !fn_pressed => return false,
            _ => {}
        }
        if !Self::modifier_ok(self.ctrl, modifiers.ctrl) {
            return false;
        }
        if !Self::modifier_ok(self.alt, modifiers.alt) {
            return false;
        }
        if !Self::modifier_ok(self.shift, modifiers.shift) {
            return false;
        }
        if !Self::modifier_ok(self.super_key, modifiers.super_key) {
            return false;
        }
        if let Some(code) = self.code {
            return code == key;
        }
        self.key
            .as_deref()
            .and_then(parse_key_name)
            .map(|k| k as u32 == key)
            .unwrap_or(false)
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LayerShortcut {
    pub key: Option<String>,
    pub code: Option<u32>,
    pub layer: usize,
    #[serde(alias = "Fn")]
    pub require_fn: Option<bool>,
    pub ctrl: Option<bool>,
    pub alt: Option<bool>,
    pub shift: Option<bool>,
    #[serde(alias = "Super")]
    pub super_key: Option<bool>,
}

impl LayerShortcut {
    fn as_keyboard_shortcut(&self) -> KeyboardShortcut {
        KeyboardShortcut {
            key: self.key.clone(),
            code: self.code,
            require_fn: self.require_fn,
            ctrl: self.ctrl,
            alt: self.alt,
            shift: self.shift,
            super_key: self.super_key,
        }
    }

    pub fn match_priority(&self) -> u32 {
        let mut score = self.layer as u32;
        if self.require_fn.is_some() {
            score += 64;
        }
        for modifier in [self.ctrl, self.alt, self.shift, self.super_key] {
            if modifier.is_some() {
                score += 16;
            }
        }
        if self.key.is_some() {
            score += 8;
        }
        score
    }

    pub fn matches(
        &self,
        key: u32,
        fn_pressed: bool,
        modifiers: &KeyboardModifiers,
    ) -> bool {
        self.as_keyboard_shortcut()
            .matches(key, fn_pressed, modifiers, true)
    }
}

pub fn default_redraw_shortcut() -> KeyboardShortcut {
    KeyboardShortcut {
        key: None,
        code: Some(51),
        require_fn: Some(true),
        ctrl: None,
        alt: None,
        shift: Some(true),
        super_key: None,
    }
}

fn load_font_face(name: &str) -> FontFace {
    let fontconfig = FontConfig::new();
    let mut pattern = Pattern::new(name);
    fontconfig.perform_substitutions(&mut pattern);
    let pat_match = match fontconfig.match_pattern(&pattern) {
        Ok(pat) => pat,
        Err(_) => panic!(
            "Unable to find font matching `{name}`. Install a font or fix FontTemplate / EmojiFontTemplate in config."
        ),
    };
    let file_name = pat_match.get_file_name();
    let file_idx = pat_match.get_font_index();
    let ft_library = FtLibrary::init().unwrap();
    let face = ft_library.new_face(file_name, file_idx).unwrap();
    FontFace::create_from_ft(&face).unwrap()
}

fn load_emoji_font_face(user_template: Option<&str>) -> Option<FontFace> {
    let mut patterns: Vec<String> = Vec::new();
    if let Some(template) = user_template {
        if !template.trim().is_empty() {
            patterns.push(template.trim().to_string());
        }
    }
    patterns.push("emoji".to_string());
    patterns.push(":family=Apple Color Emoji".to_string());
    patterns.push(":family=Noto Color Emoji".to_string());

    for pattern in patterns {
        let fontconfig = FontConfig::new();
        let mut fc_pattern = Pattern::new(&pattern);
        fontconfig.perform_substitutions(&mut fc_pattern);
        if let Ok(pat_match) = fontconfig.match_pattern(&fc_pattern) {
            let file_name = pat_match.get_file_name();
            let file_idx = pat_match.get_font_index();
            if let Ok(ft_library) = FtLibrary::init() {
                if let Ok(face) = ft_library.new_face(file_name, file_idx) {
                    if let Ok(cairo_face) = FontFace::create_from_ft(&face) {
                        eprintln!(
                            "tiny-dfr: emoji font `{pattern}` -> {file_name} (index {file_idx})"
                        );
                        return Some(cairo_face);
                    }
                }
            }
        }
    }
    eprintln!(
        "tiny-dfr: no emoji font found (install Noto Color Emoji / Apple Color Emoji, or set EmojiFontTemplate)"
    );
    None
}

fn load_theme() -> Theme {
    let mut base =
        toml::from_str::<ConfigProxy>(&read_to_string("/usr/share/tiny-dfr/config.toml").unwrap())
            .unwrap();
    let user = read_to_string("/etc/tiny-dfr/config.toml")
        .map_err::<Error, _>(|e| e.into())
        .and_then(|r| Ok(toml::from_str::<ConfigProxy>(&r)?));
    if let Ok(user) = user {
        base.media_icon_theme = user.media_icon_theme.or(base.media_icon_theme);
        base.app_icon_theme = user.app_icon_theme.or(base.app_icon_theme);
    };
    Theme {
        media_icon_theme: base.media_icon_theme.unwrap(),
        app_icon_theme: base.app_icon_theme.unwrap(),
    }
}

fn merge_user_into_base(base: &mut ConfigProxy, mut user: ConfigProxy) {
    base.media_layer_default = user
        .media_layer_default
        .or_else(|| base.media_layer_default.take());
    base.special_extended_mode = user
        .special_extended_mode
        .or_else(|| base.special_extended_mode.take());
    base.show_button_outlines = user
        .show_button_outlines
        .or_else(|| base.show_button_outlines.take());
    base.enable_pixel_shift = user
        .enable_pixel_shift
        .or_else(|| base.enable_pixel_shift.take());
    base.font_renderer = user
        .font_renderer
        .or_else(|| base.font_renderer.take());
    base.font_style = user.font_style.or_else(|| base.font_style.take());
    base.bold = user.bold.or_else(|| base.bold.take());
    base.italic = user.italic.or_else(|| base.italic.take());
    base.font_template = user
        .font_template
        .or_else(|| base.font_template.take());
    base.emoji_font_template = user
        .emoji_font_template
        .or_else(|| base.emoji_font_template.take());
    base.adaptive_brightness = user
        .adaptive_brightness
        .or_else(|| base.adaptive_brightness.take());
    base.dim_timeout_ms = user
        .dim_timeout_ms
        .or_else(|| base.dim_timeout_ms.take());
    base.off_timeout_ms = user
        .off_timeout_ms
        .or_else(|| base.off_timeout_ms.take());
    base.dimmed_brightness = user
        .dimmed_brightness
        .or_else(|| base.dimmed_brightness.take());
    base.fn_mode = user.fn_mode.or_else(|| base.fn_mode.take());
    base.fn_toggle_press_ms = user
        .fn_toggle_press_ms
        .or_else(|| base.fn_toggle_press_ms.take());
    base.allow_root_commands = user
        .allow_root_commands
        .or_else(|| base.allow_root_commands.take());
    base.media_layer_keys = user
        .media_layer_keys
        .or_else(|| base.media_layer_keys.take());
    base.primary_layer_keys = user
        .primary_layer_keys
        .or_else(|| base.primary_layer_keys.take());
    base.app_layer_keys1 = user
        .app_layer_keys1
        .or_else(|| base.app_layer_keys1.take());
    base.app_layer_keys2 = user
        .app_layer_keys2
        .or_else(|| base.app_layer_keys2.take());
    base.app_layer_keys3 = user
        .app_layer_keys3
        .or_else(|| base.app_layer_keys3.take());
    base.custom_bars = user.custom_bars.or_else(|| base.custom_bars.take());
    base.layer_shortcuts = user
        .layer_shortcuts
        .or_else(|| base.layer_shortcuts.take());
    base.redraw_shortcut = user
        .redraw_shortcut
        .or_else(|| base.redraw_shortcut.take());
    base.active_brightness = user
        .active_brightness
        .or_else(|| base.active_brightness.take());
}

fn load_config(width: u16) -> Result<(Config, Vec<FunctionLayer>), Error> {
    let mut base = toml::from_str::<ConfigProxy>(&read_to_string(
        "/usr/share/tiny-dfr/config.toml",
    )?)?;
    match read_to_string(USER_CFG_PATH) {
        Ok(raw) => match toml::from_str::<ConfigProxy>(&raw) {
            Ok(user) => merge_user_into_base(&mut base, user),
            Err(err) => {
                eprintln!(
                    "tiny-dfr: failed to parse {USER_CFG_PATH}: {err}\n\
                     tiny-dfr: ignoring user overrides until the file is fixed"
                );
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            eprintln!("tiny-dfr: could not read {USER_CFG_PATH}: {err}");
        }
    }
    let media_layer = FunctionLayer::with_config(base.media_layer_keys.unwrap());
    let fkey_layer = FunctionLayer::with_config(base.primary_layer_keys.unwrap());
    let app_layer1 = FunctionLayer::with_config(base.app_layer_keys1.unwrap());
    let app_layer2 = FunctionLayer::with_config(base.app_layer_keys2.unwrap());
    let app_layer3 = FunctionLayer::with_config(base.app_layer_keys3.unwrap());
    let mut layers = if base.media_layer_default.unwrap() {
        if base.special_extended_mode.unwrap() {
            vec![app_layer1, fkey_layer, app_layer2, app_layer3]
        } else {
            vec![media_layer, fkey_layer]
        }
    } else {
        if base.special_extended_mode.unwrap() {
            vec![fkey_layer, app_layer1, app_layer2, app_layer3]
        } else {
            vec![fkey_layer, media_layer]
        }
    };
    if let Some(custom_bars) = base.custom_bars {
        layers.extend(custom_bars.into_iter().map(FunctionLayer::with_config));
    }
    if width >= 2170 {
        for layer in &mut layers {
            layer.buttons.insert(
                0,
                Button::new_text("esc".to_string(), Key::Esc, true, None, None),
            );
        }
    }
    let cfg = Config {
        media_layer_default: base.media_layer_default.unwrap(),
        show_button_outlines: base.show_button_outlines.unwrap(),
        enable_pixel_shift: base.enable_pixel_shift.unwrap(),
        adaptive_brightness: base.adaptive_brightness.unwrap(),
        font_renderer: base.font_renderer.unwrap(),
        font_style_cairo: base.font_style.unwrap(),
        bold_cairo: base.bold.unwrap(),
        italic_cairo: base.italic.unwrap(),
        font_face: load_font_face(&base.font_template.unwrap()),
        emoji_font_face: load_emoji_font_face(base.emoji_font_template.as_deref()),
        active_brightness: base.active_brightness.unwrap(),
        dim_timeout_ms: base.dim_timeout_ms.unwrap_or(30_000),
        off_timeout_ms: base.off_timeout_ms.unwrap_or(60_000),
        dimmed_brightness: base.dimmed_brightness.unwrap_or(1),
        fn_mode: FnMode::from_config(base.fn_mode),
        fn_toggle_press_ms: base.fn_toggle_press_ms.unwrap_or(120),
        layer_shortcuts: base.layer_shortcuts.unwrap_or_else(default_layer_shortcuts),
        redraw_shortcut: base
            .redraw_shortcut
            .unwrap_or_else(default_redraw_shortcut),
        allow_root_commands: base.allow_root_commands.unwrap_or(false),
    };
    Ok((cfg, layers))
}

fn default_layer_shortcuts() -> Vec<LayerShortcut> {
    // Linux input key codes: KEY_1..KEY_9 are 2..10, KEY_0 is 11.
    let mut shortcuts = (0..=9)
        .map(|layer| LayerShortcut {
            key: None,
            code: Some(if layer == 0 { 11 } else { layer as u32 + 1 }),
            layer,
            require_fn: Some(true),
            ctrl: None,
            alt: None,
            shift: None,
            super_key: None,
        })
        .collect::<Vec<_>>();
    shortcuts.sort_by_key(|shortcut| shortcut.layer);
    shortcuts
}

pub struct ConfigManager {
    inotify_fd: Inotify,
    watch_desc: Option<WatchDescriptor>,
}

fn arm_inotify(inotify_fd: &Inotify) -> Option<WatchDescriptor> {
    let flags = AddWatchFlags::IN_MOVED_TO | AddWatchFlags::IN_CLOSE | AddWatchFlags::IN_ONESHOT;
    match inotify_fd.add_watch(USER_CFG_PATH, flags) {
        Ok(wd) => Some(wd),
        Err(Errno::ENOENT) => None,
        e => Some(e.unwrap()),
    }
}

impl ConfigManager {
    pub fn new() -> ConfigManager {
        let inotify_fd = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
        let watch_desc = arm_inotify(&inotify_fd);
        ConfigManager {
            inotify_fd,
            watch_desc,
        }
    }
    pub fn load_config(&self, width: u16) -> Result<(Config, Vec<FunctionLayer>), Error> {
        load_config(width)
    }
    pub fn load_theme(&self) -> Theme {
        load_theme()
    }
    pub fn update_config(
        &mut self,
        cfg: &mut Config,
        layers: &mut Vec<FunctionLayer>,
        width: u16,
    ) -> bool {
        if self.watch_desc.is_none() {
            self.watch_desc = arm_inotify(&self.inotify_fd);
            return false;
        }
        let evts = match self.inotify_fd.read_events() {
            Ok(e) => e,
            Err(Errno::EAGAIN) => Vec::new(),
            r => r.unwrap(),
        };
        let mut ret = false;
        for evt in evts {
            if evt.wd != self.watch_desc.unwrap() {
                continue;
            }
            match load_config(width) {
                Ok(parts) => {
                    *cfg = parts.0;
                    *layers = parts.1;
                    ret = true;
                }
                Err(err) => {
                    eprintln!("tiny-dfr: config reload failed: {err}");
                }
            }
            self.watch_desc = arm_inotify(&self.inotify_fd);
        }
        ret
    }
    pub fn fd(&self) -> &impl AsFd {
        &self.inotify_fd
    }
}
