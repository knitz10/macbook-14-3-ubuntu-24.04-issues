#!/usr/bin/env python3
from __future__ import annotations

import copy
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

try:
    import tkinter as tk
    from tkinter import filedialog, messagebox, ttk
except ImportError:
    print("tkinter is required: sudo apt install python3-tk", file=sys.stderr)
    sys.exit(1)

try:
    import tomlkit
except ImportError:
    print("tomlkit is required: sudo apt install python3-tomlkit", file=sys.stderr)
    sys.exit(1)

from touchbar_render import icon_photoimage, list_theme_icons, read_preview_settings


CONFIG_PATH = Path(os.environ.get("TINY_DFR_CONFIG", "/etc/tiny-dfr/config.toml"))
TEMPLATE_PATH = Path(os.environ.get("TINY_DFR_TEMPLATE", "/usr/share/tiny-dfr/config.toml"))
if not TEMPLATE_PATH.exists():
    local_template = Path(__file__).resolve().parent / "share" / "tiny-dfr" / "config.toml"
    if local_template.exists():
        TEMPLATE_PATH = local_template

BACKUP_SUFFIX = ".bak.layout-editor"
TOUCHBAR_WIDTH = int(os.environ.get("TINY_DFR_TOUCHBAR_WIDTH", "2170"))
TOUCHBAR_HEIGHT = 60
BUTTON_SPACING_PX = 16

BG = "#131316"
PANEL = "#1d1d23"
PANEL_2 = "#24242c"
CHIP = "#30303a"
CHIP_HOVER = "#3a3a46"
TEXT = "#f4f4f6"
TEXT_DIM = "#a7a7b0"
ACCENT = "#4aa3ff"
WARN = "#ffcc66"
STRIP = "#000000"
OUTLINE = "#333333"
ACTIVE = "#666666"

LAYER_DEFS = [
    ("Default / media layer", "MediaLayerKeys"),
    ("Fn / function keys", "PrimaryLayerKeys"),
    ("App layer 1", "AppLayerKeys1"),
    ("App layer 2", "AppLayerKeys2"),
    ("App layer 3", "AppLayerKeys3"),
]

MERGE_KEYS = (
    "MediaLayerDefault", "SpecialExtendedMode", "ShowButtonOutlines", "EnablePixelShift",
    "FontRenderer", "FontStyle", "Bold", "Italic", "FontTemplate", "EmojiFontTemplate", "AdaptiveBrightness",
    "ActiveBrightness", "DimTimeoutMs", "OffTimeoutMs", "DimmedBrightness", "FnMode",
    "FnTogglePressMs", "AllowRootCommands", "LayerShortcuts", "RedrawShortcut",
    "ButtonLibrary",
    "MediaLayerKeys", "PrimaryLayerKeys", "AppLayerKeys1", "AppLayerKeys2", "AppLayerKeys3",
    "CustomBars",
)

BUILTIN_ESC = {"Text": "esc", "Action": "Esc", "_builtin": True}

COMMON_ACTIONS = [
    "",
    "Esc",
    "F1",
    "F2",
    "F3",
    "F4",
    "F5",
    "F6",
    "F7",
    "F8",
    "F9",
    "F10",
    "F11",
    "F12",
    "BrightnessUp",
    "BrightnessDown",
    "VolumeUp",
    "VolumeDown",
    "MicMute",
    "Play",
    "Pause",
    "NextSong",
    "PrevSong",
    "Macro1",
    "Macro2",
    "Macro3",
    "Macro4",
    "Time",
    "Unknown",
]


def sanitize_shortcut(sc: dict) -> dict:
    out = button_to_dict(sc)
    key = out.pop("Key", None)
    if key is not None:
        key = normalize_key_name(str(key))
        if key in EVDEV_KEY_BY_NAME:
            out["Code"] = EVDEV_KEY_BY_NAME[key]
        elif key:
            out["Key"] = key
    return out


def default_redraw_shortcut() -> dict:
    return {"Code": 51, "Fn": True, "Shift": True}


def repair_switch_to_layers(doc) -> None:
    """Fix stale SwitchToLayer indices after bars were added or removed."""
    layer_count = len(runtime_layer_order(doc))
    order = runtime_layer_order(doc)
    first_custom = next(
        (i for i, key in enumerate(order) if key.startswith("CustomBars")),
        None,
    )

    def fix(btn) -> dict:
        b = button_to_dict(btn)
        if "SwitchToLayer" not in b:
            return b
        try:
            idx = int(b["SwitchToLayer"])
        except (TypeError, ValueError):
            return b
        if idx < layer_count:
            return b
        if first_custom is not None:
            b["SwitchToLayer"] = first_custom
        else:
            b["SwitchToLayer"] = max(0, layer_count - 1)
        return b

    for key in (
        "MediaLayerKeys",
        "PrimaryLayerKeys",
        "AppLayerKeys1",
        "AppLayerKeys2",
        "AppLayerKeys3",
    ):
        if key in doc:
            doc[key] = [fix(b) for b in layer_items(doc, key)]
    if "CustomBars" in doc:
        doc["CustomBars"] = [[fix(b) for b in bar] for bar in doc["CustomBars"]]
    if "ButtonLibrary" in doc:
        doc["ButtonLibrary"] = [fix(b) for b in doc["ButtonLibrary"]]


def sanitize_button(btn: dict, doc=None) -> dict:
    out = button_to_dict(btn)
    if out.get("Icon"):
        out.pop("Text", None)
    action = out.get("Action")
    if action is not None and not str(action).strip():
        del out["Action"]
    elif action is not None:
        name = normalize_key_name(str(action))
        if name:
            out["Action"] = name
    if doc is not None and "SwitchToLayer" in out:
        layer_count = len(runtime_layer_order(doc))
        try:
            idx = int(out["SwitchToLayer"])
        except (TypeError, ValueError):
            idx = None
        if idx is not None and idx >= layer_count:
            order = runtime_layer_order(doc)
            first_custom = next(
                (i for i, key in enumerate(order) if key.startswith("CustomBars")),
                None,
            )
            if first_custom is not None:
                out["SwitchToLayer"] = first_custom
            else:
                out["SwitchToLayer"] = max(0, layer_count - 1)
    return out


def button_to_dict(btn) -> dict:
    if isinstance(btn, dict) and not hasattr(btn, "unwrap"):
        return dict(btn)
    if hasattr(btn, "unwrap"):
        plain = btn.unwrap()
        return dict(plain) if isinstance(plain, dict) else dict(btn)
    return dict(btn)


def load_config_document():
    if not TEMPLATE_PATH.exists():
        raise FileNotFoundError(f"Template not found: {TEMPLATE_PATH}")
    doc = tomlkit.parse(TEMPLATE_PATH.read_text(encoding="utf-8"))
    if CONFIG_PATH.exists():
        user = tomlkit.parse(CONFIG_PATH.read_text(encoding="utf-8"))
        for key in MERGE_KEYS:
            if key in user and user[key] is not None:
                doc[key] = user[key]
        for key in user:
            if key not in doc:
                doc[key] = user[key]
    repair_switch_to_layers(doc)
    return doc


def inline_table(data: dict):
    t = tomlkit.inline_table()
    for key, value in data.items():
        if not key.startswith("_") and value not in ("", None):
            t[key] = value
    return t


def button_array(items: list[dict]):
    arr = tomlkit.array()
    arr.multiline(True)
    for item in items:
        arr.append(inline_table(item))
    return arr


def runtime_layer_order(doc) -> list[str]:
    media_default = bool(doc.get("MediaLayerDefault", False))
    extended = bool(doc.get("SpecialExtendedMode", False))
    if extended:
        if media_default:
            order = [
                "AppLayerKeys1",
                "PrimaryLayerKeys",
                "AppLayerKeys2",
                "AppLayerKeys3",
            ]
        else:
            order = [
                "PrimaryLayerKeys",
                "AppLayerKeys1",
                "AppLayerKeys2",
                "AppLayerKeys3",
            ]
    elif media_default:
        order = ["MediaLayerKeys", "PrimaryLayerKeys"]
    else:
        order = ["PrimaryLayerKeys", "MediaLayerKeys"]
    for i in range(len(doc.get("CustomBars", []))):
        order.append(f"CustomBars[{i}]")
    return order


def layer_key_to_index(doc, key: str) -> int:
    order = runtime_layer_order(doc)
    try:
        return order.index(key)
    except ValueError:
        return 0


# Linux evdev key names (input-linux / input-event-codes). Code = KEY_* value.
EVDEV_KEY_BY_NAME: dict[str, int] = {
    "0": 11,
    "1": 2,
    "2": 3,
    "3": 4,
    "4": 5,
    "5": 6,
    "6": 7,
    "7": 8,
    "8": 9,
    "9": 10,
    "A": 30,
    "B": 48,
    "C": 46,
    "D": 32,
    "E": 18,
    "F": 33,
    "G": 34,
    "H": 35,
    "I": 23,
    "J": 36,
    "K": 37,
    "L": 38,
    "M": 50,
    "N": 49,
    "O": 24,
    "P": 25,
    "Q": 16,
    "R": 19,
    "S": 31,
    "T": 20,
    "U": 22,
    "V": 47,
    "W": 17,
    "X": 45,
    "Y": 21,
    "Z": 44,
    "Space": 57,
    "Tab": 15,
    "Enter": 28,
    "Escape": 1,
    "Backspace": 14,
    "Delete": 111,
    "Insert": 110,
    "Home": 102,
    "End": 107,
    "Pageup": 104,
    "Pagedown": 109,
    "Left": 105,
    "Right": 106,
    "Up": 103,
    "Down": 108,
    "F1": 59,
    "F2": 60,
    "F3": 61,
    "F4": 62,
    "F5": 63,
    "F6": 64,
    "F7": 65,
    "F8": 66,
    "F9": 67,
    "F10": 68,
    "F11": 87,
    "F12": 88,
    "Minus": 12,
    "Equal": 13,
    "Leftbrace": 26,
    "Rightbrace": 27,
    "Semicolon": 39,
    "Apostrophe": 40,
    "Grave": 41,
    "Backslash": 43,
    "Comma": 51,
    "Dot": 52,
    "Slash": 53,
    "Leftctrl": 29,
    "Rightctrl": 97,
    "Leftalt": 56,
    "Rightalt": 100,
    "Leftshift": 42,
    "Rightshift": 54,
    "Leftmeta": 125,
    "Rightmeta": 126,
}

EVDEV_NAME_BY_CODE = {code: name for name, code in EVDEV_KEY_BY_NAME.items()}


def normalize_key_name(name: str) -> str:
    cleaned = name.strip()
    if not cleaned:
        return ""
    if cleaned == "<":
        return "Comma"
    if cleaned.isdigit():
        return cleaned
    lower = cleaned.lower()
    for candidate, code in EVDEV_KEY_BY_NAME.items():
        if candidate.lower() == lower:
            return candidate
    return cleaned


def shortcut_key_label(shortcut: dict) -> str:
    if shortcut.get("Key"):
        return str(shortcut["Key"])
    code = shortcut.get("Code")
    if code == 51 and shortcut.get("Shift") is True:
        return "<"
    if code is not None:
        return EVDEV_NAME_BY_CODE.get(int(code), f"code {code}")
    return "?"


def format_shortcut(shortcut: dict) -> str:
    parts = []
    for mod, label in (
        ("Ctrl", "Ctrl"),
        ("Alt", "Alt"),
        ("Shift", "Shift"),
        ("Super", "Super"),
    ):
        if shortcut.get(mod) is True:
            parts.append(label)
    if shortcut.get("Fn", shortcut.get("RequireFn")) is True:
        parts.append("Fn")
    parts.append(shortcut_key_label(shortcut))
    return "+".join(parts)


def all_layer_defs(doc):
    defs = list(LAYER_DEFS)
    for i, _bar in enumerate(doc.get("CustomBars", [])):
        defs.append((f"Custom bar {i + 1}", f"CustomBars[{i}]"))
    return defs


def layer_items(doc, key: str) -> list[dict]:
    if key.startswith("CustomBars["):
        idx = int(key.removeprefix("CustomBars[").removesuffix("]"))
        bars = doc.get("CustomBars", [])
        return [button_to_dict(b) for b in bars[idx]] if idx < len(bars) else []
    return [button_to_dict(b) for b in doc.get(key, [])]


def button_label(btn: dict) -> str:
    if btn.get("Text"):
        return str(btn["Text"])
    if btn.get("Icon"):
        return str(btn["Icon"]).replace("-symbolic", "").replace("-", " ")[:18]
    if btn.get("Command"):
        return "cmd"
    if btn.get("SwitchToLayer") is not None:
        return f"bar {btn['SwitchToLayer']}"
    return str(btn.get("Action", "?"))


def button_detail(btn: dict) -> str:
    parts = []
    for key in ("Text", "Icon", "Path", "Action", "Mode", "Command", "SwitchToLayer", "Background"):
        if btn.get(key) not in (None, ""):
            parts.append(f"{key}: {btn[key]}")
    return " | ".join(parts) if parts else "Blank button"


def layer_switch_choices(doc) -> list[tuple[str, str]]:
    choices = [("— none —", "")]
    for i, (name, _key) in enumerate(all_layer_defs(doc)):
        choices.append((f"Layer {i}: {name}", str(i)))
    return choices


def library_entries(doc) -> list[tuple[dict, bool, int | None]]:
    seen = set()
    entries: list[tuple[dict, bool, int | None]] = []
    for key in ("MediaLayerKeys", "PrimaryLayerKeys", "AppLayerKeys1", "AppLayerKeys2", "AppLayerKeys3"):
        for btn in layer_items(doc, key):
            ident = tuple(sorted((k, str(v)) for k, v in btn.items() if not k.startswith("_")))
            if ident not in seen:
                seen.add(ident)
                entries.append((btn, False, None))
    for i, btn in enumerate(doc.get("ButtonLibrary", [])):
        entries.append((button_to_dict(btn), True, i))
    return entries


class ScrollRow(tk.Frame):
    """Single-line row that scrolls horizontally when settings overflow."""

    def __init__(self, master, height=40, **kwargs):
        super().__init__(master, bg=BG, **kwargs)
        self.canvas = tk.Canvas(self, bg=BG, height=height, highlightthickness=0, bd=0)
        self.hbar = ttk.Scrollbar(self, orient=tk.HORIZONTAL, command=self.canvas.xview)
        self.inner = tk.Frame(self.canvas, bg=BG)
        self.inner.bind("<Configure>", self._sync)
        self._win = self.canvas.create_window((0, 0), window=self.inner, anchor="nw")
        self.canvas.configure(xscrollcommand=self.hbar.set)
        self.canvas.pack(side=tk.TOP, fill=tk.X, expand=True)
        self.hbar.pack(side=tk.BOTTOM, fill=tk.X)
        self.canvas.bind("<Configure>", self._on_canvas)

    def _on_canvas(self, event):
        self.canvas.itemconfigure(self._win, height=event.height)
        self._sync()

    def _sync(self, _event=None):
        self.canvas.configure(scrollregion=self.canvas.bbox("all"))


def draw_round(canvas, x1, y1, x2, y2, radius, **kw):
    radius = max(2, min(radius, (x2 - x1) / 2, (y2 - y1) / 2))
    canvas.create_arc(x1, y1, x1 + 2 * radius, y1 + 2 * radius, start=90, extent=90, style=tk.PIESLICE, **kw)
    canvas.create_arc(x2 - 2 * radius, y1, x2, y1 + 2 * radius, start=0, extent=90, style=tk.PIESLICE, **kw)
    canvas.create_arc(x2 - 2 * radius, y2 - 2 * radius, x2, y2, start=270, extent=90, style=tk.PIESLICE, **kw)
    canvas.create_arc(x1, y2 - 2 * radius, x1 + 2 * radius, y2, start=180, extent=90, style=tk.PIESLICE, **kw)
    canvas.create_rectangle(x1 + radius, y1, x2 - radius, y2, **kw)
    canvas.create_rectangle(x1, y1 + radius, x2, y2 - radius, **kw)


class ButtonCanvas(tk.Canvas):
    def __init__(self, master, app):
        super().__init__(master, bg=STRIP, highlightthickness=0, height=96)
        self.app = app
        self.buttons: list[dict] = []
        self.positions: list[tuple[float, float]] = []
        self.photos = []
        self.drag_index: int | None = None
        self.drag_x = 0
        self.drop_remove = False
        self.selected_index: int | None = None
        self.bind("<ButtonPress-1>", self.press)
        self.bind("<B1-Motion>", self.motion)
        self.bind("<ButtonRelease-1>", self.release)
        self.bind("<Configure>", lambda _e: self.redraw())

    def set_buttons(self, buttons: list[dict]):
        self.buttons = [copy.deepcopy(b) for b in buttons]
        if TOUCHBAR_WIDTH >= 2170:
            self.buttons.insert(0, dict(BUILTIN_ESC))
        self.drag_index = None
        self.selected_index = None
        self.redraw()

    def get_buttons(self):
        return [copy.deepcopy(b) for b in self.buttons if not b.get("_builtin")]

    def scale(self):
        return min(1.0, max(0.2, (self.winfo_width() - 24) / TOUCHBAR_WIDTH))

    def slot_width(self):
        count = max(1, len(self.buttons))
        return max(28, ((TOUCHBAR_WIDTH - BUTTON_SPACING_PX * (count - 1)) / count) * self.scale())

    def layout_positions(self):
        count = len(self.buttons)
        width = self.slot_width()
        spacing = BUTTON_SPACING_PX * self.scale()
        total = count * width + max(0, count - 1) * spacing
        x = max(12, (self.winfo_width() - total) / 2)
        return [(x + i * (width + spacing), width) for i in range(count)]

    def index_at(self, x):
        positions = self.layout_positions()
        if not positions:
            return None
        centers = [left + width / 2 for left, width in positions]
        return min(range(len(centers)), key=lambda i: abs(centers[i] - x))

    def redraw(self):
        self.delete("all")
        self.photos = []
        self.positions = self.layout_positions()
        self.create_rectangle(0, 0, self.winfo_width(), 96, fill=STRIP, outline="")
        for i, btn in enumerate(self.buttons):
            left, width = self.positions[i]
            if i == self.drag_index:
                left = self.drag_x - width / 2
            self.draw_button(i, btn, left, width, 18, 60)
        if self.drop_remove:
            self.create_rectangle(2, 2, self.winfo_width() - 2, 94, outline=WARN, width=2)

    def draw_button(self, i, btn, left, width, top, height):
        active = i == self.drag_index
        right = left + width
        wants_bg = btn.get("Background", True) is not False and btn.get("Mode") != "Blank"
        if wants_bg and not btn.get("_builtin"):
            draw_round(self, left, top, right, top + height, 8, fill=ACTIVE if active else OUTLINE, outline="")
        text_x = left + width / 2
        mid_y = top + height / 2
        mode = str(btn.get("Mode", "")) or None
        if btn.get("Icon"):
            photo = icon_photoimage(
                self,
                str(btn["Icon"]),
                mode=mode,
                media_theme=self.app.preview_settings["media_theme"],
                app_theme=self.app.preview_settings["app_theme"],
                custom_path=btn.get("Path"),
                size=28,
            )
            if photo:
                self.photos.append(photo)
                self.create_image(text_x, mid_y, image=photo)
                return
        label = "esc" if btn.get("_builtin") else button_label(btn)
        self.create_text(text_x, mid_y, text=label, fill=TEXT, font=("Sans", 11, "bold"), width=max(24, width - 8))

    def press(self, event):
        idx = self.index_at(event.x)
        if idx is None or self.buttons[idx].get("_builtin"):
            return
        self.drag_index = idx
        self.selected_index = idx
        self.drag_x = event.x
        self.app.select_button(self.buttons[idx])
        self.redraw()

    def motion(self, event):
        if self.drag_index is None:
            return
        self.drag_x = event.x
        self.drop_remove = self.app.library_contains_root(event.x_root, event.y_root)
        idx = self.index_at(event.x)
        if idx is not None and idx != self.drag_index and not self.buttons[idx].get("_builtin"):
            btn = self.buttons.pop(self.drag_index)
            self.buttons.insert(idx, btn)
            self.drag_index = idx
            self.selected_index = idx
        self.redraw()

    def release(self, event):
        if self.drag_index is not None and self.app.library_contains_root(event.x_root, event.y_root):
            del self.buttons[self.drag_index]
            self.selected_index = None
        self.drag_index = None
        self.drop_remove = False
        self.redraw()

    def add_button(self, btn: dict):
        self.buttons.append(copy.deepcopy(btn))
        self.selected_index = len(self.buttons) - 1
        self.redraw()

    def selected_button(self):
        if self.selected_index is None:
            return None
        if 0 <= self.selected_index < len(self.buttons) and not self.buttons[self.selected_index].get("_builtin"):
            return self.buttons[self.selected_index]
        return None

    def replace_selected(self, btn: dict):
        if self.selected_index is not None and 0 <= self.selected_index < len(self.buttons):
            self.buttons[self.selected_index] = copy.deepcopy(btn)
            self.redraw()

    def delete_selected(self):
        if self.selected_index is not None and 0 <= self.selected_index < len(self.buttons):
            if not self.buttons[self.selected_index].get("_builtin"):
                del self.buttons[self.selected_index]
                self.selected_index = None
                self.redraw()


class Library(tk.Frame):
    CHIP_PAD = 6
    CHIP_MIN_W = 108

    def __init__(self, master, app):
        super().__init__(master, bg=PANEL)
        self.app = app
        self.photos = []
        self._chip_entries: list[tuple[dict | None, bool, int | None]] = []
        self._scroll_bound = False
        tk.Label(self, text="Button library", bg=PANEL, fg=TEXT, font=("Sans", 11, "bold")).pack(
            anchor="w", padx=10, pady=(8, 2)
        )
        body = tk.Frame(self, bg=PANEL)
        body.pack(fill=tk.BOTH, expand=True, padx=8, pady=(0, 8))
        self.canvas = tk.Canvas(body, bg=PANEL, highlightthickness=0, bd=0)
        self.scrollbar_y = ttk.Scrollbar(body, orient=tk.VERTICAL, command=self.canvas.yview)
        self.scrollbar_x = ttk.Scrollbar(body, orient=tk.HORIZONTAL, command=self.canvas.xview)
        self.inner = tk.Frame(self.canvas, bg=PANEL)
        self.grid_host = tk.Frame(self.inner, bg=PANEL)
        self.grid_host.pack(fill=tk.BOTH, expand=True)
        self.inner.bind("<Configure>", self._on_inner_configure)
        self.canvas_window = self.canvas.create_window((0, 0), window=self.inner, anchor="n")
        self.canvas.configure(yscrollcommand=self.scrollbar_y.set, xscrollcommand=self.scrollbar_x.set)
        self.canvas.pack(side=tk.LEFT, fill=tk.BOTH, expand=True)
        self.scrollbar_y.pack(side=tk.RIGHT, fill=tk.Y)
        self.scrollbar_x.pack(side=tk.BOTTOM, fill=tk.X)
        self.canvas.bind("<Configure>", self._on_canvas_configure)
        for widget in (self.canvas, self.inner, self.grid_host):
            widget.bind("<Enter>", self._bind_scroll)
            widget.bind("<Leave>", self._unbind_scroll)

    def _on_inner_configure(self, _event=None):
        self.canvas.configure(scrollregion=self.canvas.bbox("all"))

    def _on_canvas_configure(self, event):
        self.canvas.itemconfigure(self.canvas_window, width=event.width)
        self.canvas.coords(self.canvas_window, event.width / 2, 0)
        self._relayout()

    def _bind_scroll(self, _event=None):
        if self._scroll_bound:
            return
        self._scroll_bound = True
        self.canvas.bind_all("<MouseWheel>", self._on_mousewheel, add="+")
        self.canvas.bind_all("<Button-4>", self._on_mousewheel, add="+")
        self.canvas.bind_all("<Button-5>", self._on_mousewheel, add="+")

    def _unbind_scroll(self, _event=None):
        if not self._scroll_bound:
            return
        self._scroll_bound = False
        self.canvas.unbind_all("<MouseWheel>")
        self.canvas.unbind_all("<Button-4>")
        self.canvas.unbind_all("<Button-5>")

    def _on_mousewheel(self, event):
        if event.num == 4 or getattr(event, "delta", 0) > 0:
            self.canvas.yview_scroll(-1, "units")
        elif event.num == 5 or getattr(event, "delta", 0) < 0:
            self.canvas.yview_scroll(1, "units")

    def _relayout(self):
        for child in self.grid_host.winfo_children():
            child.destroy()
        canvas_w = max(self.CHIP_MIN_W, self.canvas.winfo_width() - 16)
        cols = max(1, canvas_w // self.CHIP_MIN_W)
        content_w = cols * (self.CHIP_MIN_W + self.CHIP_PAD * 2)
        for start in range(0, len(self._chip_entries), cols):
            row_entries = self._chip_entries[start : start + cols]
            row_wrap = tk.Frame(self.grid_host, bg=PANEL, width=content_w)
            row_wrap.pack(fill=tk.X, pady=2)
            row = tk.Frame(row_wrap, bg=PANEL)
            row.pack(anchor="center")
            for entry in row_entries:
                btn, deletable, lib_index = entry
                if btn is None:
                    self._make_custom_button(row)
                else:
                    self._make_chip(btn, row, deletable, lib_index)
        self._on_inner_configure()

    def refresh(self):
        self.photos = []
        self._chip_entries = [
            (copy.deepcopy(btn), deletable, lib_index) for btn, deletable, lib_index in library_entries(self.app.doc)
        ]
        self._chip_entries.append((None, False, None))
        self._relayout()

    def _make_custom_button(self, parent: tk.Misc) -> None:
        tk.Button(
            parent,
            text="+ custom",
            command=lambda: self.app.custom_button_dialog(),
            bg=ACCENT,
            fg="#ffffff",
            activebackground="#6cb5ff",
            relief=tk.FLAT,
            padx=10,
            pady=4,
        ).pack(side=tk.LEFT, padx=self.CHIP_PAD, pady=self.CHIP_PAD)

    def _make_chip(self, btn: dict, parent: tk.Misc, deletable: bool, lib_index: int | None) -> tk.Frame:
        frame = tk.Frame(parent, bg=CHIP, cursor="hand2", padx=8, pady=5)
        if btn.get("Icon"):
            photo = icon_photoimage(
                frame,
                str(btn["Icon"]),
                mode=str(btn.get("Mode", "")) or None,
                media_theme=self.app.preview_settings["media_theme"],
                app_theme=self.app.preview_settings["app_theme"],
                custom_path=btn.get("Path"),
                size=18,
            )
            if photo:
                self.photos.append(photo)
                tk.Label(frame, image=photo, bg=CHIP).pack(side=tk.LEFT, padx=(0, 5))
        chip = tk.Label(frame, text=button_label(btn), bg=CHIP, fg=TEXT, cursor="hand2")
        chip.pack(side=tk.LEFT)
        for widget in (frame, chip):
            widget.bind("<Button-1>", lambda _e, b=copy.deepcopy(btn): self.app.bar.add_button(b))
        if deletable and lib_index is not None:
            tk.Button(
                frame,
                text="×",
                command=lambda idx=lib_index: self.app.delete_library_button(idx),
                bg=CHIP,
                fg=WARN,
                activebackground=CHIP_HOVER,
                relief=tk.FLAT,
                padx=2,
                pady=0,
            ).pack(side=tk.RIGHT)
        frame.pack(side=tk.LEFT, padx=self.CHIP_PAD, pady=self.CHIP_PAD)
        return frame


class App(tk.Tk):
    def __init__(self):
        super().__init__()
        self.title("tiny-dfr editor")
        self.configure(bg=BG)
        self.geometry("1220x720")
        self.minsize(980, 620)
        self.style_widgets()

        self.doc = load_config_document()
        self.preview_settings = read_preview_settings(self.doc)
        self.layer_defs = all_layer_defs(self.doc)
        self.layer_name = tk.StringVar(value=self.layer_defs[0][0])
        self.current_key = self.layer_defs[0][1]

        self.dim_ms = tk.IntVar(value=int(self.doc.get("DimTimeoutMs", 30000)))
        self.off_ms = tk.IntVar(value=int(self.doc.get("OffTimeoutMs", 60000)))
        self.dim_brightness = tk.IntVar(value=int(self.doc.get("DimmedBrightness", 1)))
        self.active_brightness = tk.IntVar(value=int(self.doc.get("ActiveBrightness", 128)))
        self.media_layer_default = tk.BooleanVar(value=bool(self.doc.get("MediaLayerDefault", False)))
        self.fn_mode = tk.StringVar(value=str(self.doc.get("FnMode", "hold")))
        self.fn_tap_ms = tk.IntVar(value=int(self.doc.get("FnTogglePressMs", 120)))
        self.shortcut_key = tk.StringVar(value="1")
        self.shortcut_requires_fn = tk.BooleanVar(value=False)
        self.shortcut_ctrl = tk.BooleanVar(value=False)
        self.shortcut_alt = tk.BooleanVar(value=False)
        self.shortcut_shift = tk.BooleanVar(value=False)
        self.shortcut_super = tk.BooleanVar(value=False)
        self.redraw_key = tk.StringVar(value="<")
        self.redraw_requires_fn = tk.BooleanVar(value=True)
        self.redraw_ctrl = tk.BooleanVar(value=False)
        self.redraw_alt = tk.BooleanVar(value=False)
        self.redraw_shift = tk.BooleanVar(value=True)
        self.redraw_super = tk.BooleanVar(value=False)
        self.detail = tk.StringVar(value="Drag buttons to reorder. Drag to the library area to remove.")

        self.build()
        self.load_redraw_shortcut_ui()
        self.load_layer(self.current_key)

    def style_widgets(self):
        style = ttk.Style()
        if "clam" in style.theme_names():
            style.theme_use("clam")
        style.configure(".", background=BG, foreground=TEXT, fieldbackground=PANEL_2, bordercolor=CHIP, lightcolor=CHIP, darkcolor=CHIP)
        style.configure("TButton", background=CHIP, foreground=TEXT, padding=6)
        style.map("TButton", background=[("active", CHIP_HOVER)])
        style.configure("TCombobox", fieldbackground=PANEL_2, background=CHIP, foreground=TEXT, arrowcolor=TEXT)
        style.configure("TSpinbox", fieldbackground=PANEL_2, background=CHIP, foreground=TEXT, arrowcolor=TEXT)
        style.configure("TCheckbutton", background=BG, foreground=TEXT)

    def build(self):
        top = tk.Frame(self, bg=BG, padx=16, pady=12)
        top.pack(fill=tk.X)
        tk.Label(top, text="tiny-dfr editor", bg=BG, fg=TEXT, font=("Sans", 17, "bold")).pack(side=tk.LEFT)
        tk.Label(top, text=str(CONFIG_PATH), bg=BG, fg=TEXT_DIM).pack(side=tk.RIGHT)

        row = tk.Frame(self, bg=BG, padx=16)
        row.pack(fill=tk.X)
        self.layer_combo = ttk.Combobox(row, textvariable=self.layer_name, values=[name for name, _ in self.layer_defs], state="readonly", width=24)
        self.layer_combo.pack(side=tk.LEFT)
        self.layer_combo.bind("<<ComboboxSelected>>", self.layer_changed)
        ttk.Button(row, text="Add custom bar", command=self.add_custom_bar).pack(side=tk.LEFT, padx=6)
        ttk.Button(row, text="Remove custom bar", command=self.remove_custom_bar).pack(side=tk.LEFT, padx=6)
        ttk.Button(row, text="Edit selected", command=self.edit_selected_button).pack(side=tk.LEFT, padx=6)
        ttk.Button(row, text="Remove button", command=lambda: self.bar.delete_selected()).pack(side=tk.LEFT)
        ttk.Button(row, text="Save", command=lambda: self.save(False)).pack(side=tk.RIGHT, padx=4)
        ttk.Button(row, text="Save + restart", command=lambda: self.save(True)).pack(side=tk.RIGHT, padx=4)

        settings_row = ScrollRow(self, height=42)
        settings_row.pack(fill=tk.X, padx=16, pady=8)
        settings = settings_row.inner
        self.spin(settings, "Dim ms", self.dim_ms, 1000, 600000, 1000)
        self.spin(settings, "Black ms", self.off_ms, 1000, 900000, 1000)
        self.spin(settings, "Dim", self.dim_brightness, 0, 255, 1, 5)
        self.spin(settings, "Active", self.active_brightness, 1, 255, 1, 5)
        ttk.Checkbutton(
            settings,
            text="Media keys on bar by default",
            variable=self.media_layer_default,
            command=self.media_default_changed,
        ).pack(side=tk.LEFT, padx=(12, 6))
        tk.Label(settings, text="Fn", bg=BG, fg=TEXT_DIM).pack(side=tk.LEFT, padx=(8, 4))
        ttk.Combobox(settings, textvariable=self.fn_mode, values=("hold", "toggle", "smart"), state="readonly", width=8).pack(side=tk.LEFT)
        self.spin(settings, "Tap ms", self.fn_tap_ms, 40, 1000, 10, 6)
        tk.Label(settings, text="Bar shortcut", bg=BG, fg=TEXT_DIM).pack(side=tk.LEFT, padx=(12, 4))
        key_values = sorted(
            set(EVDEV_KEY_BY_NAME.keys()) | {"<"},
            key=lambda s: (0 if s == "<" else 1, len(s), s.lower()),
        )
        ttk.Combobox(
            settings,
            textvariable=self.shortcut_key,
            values=key_values,
            width=10,
        ).pack(side=tk.LEFT)
        for label, var in (
            ("Ctrl", self.shortcut_ctrl),
            ("Alt", self.shortcut_alt),
            ("Shift", self.shortcut_shift),
            ("Super", self.shortcut_super),
            ("Fn", self.shortcut_requires_fn),
        ):
            ttk.Checkbutton(settings, text=label, variable=var).pack(side=tk.LEFT, padx=4)
        ttk.Button(settings, text="Set shortcut", command=self.set_current_shortcut).pack(side=tk.LEFT, padx=6)
        ttk.Button(settings, text="Clear shortcut", command=self.clear_current_shortcut).pack(side=tk.LEFT, padx=4)

        redraw_row = ScrollRow(self, height=42)
        redraw_row.pack(fill=tk.X, padx=16, pady=(0, 8))
        redraw = redraw_row.inner
        tk.Label(redraw, text="Force redraw", bg=BG, fg=TEXT_DIM).pack(side=tk.LEFT, padx=(0, 4))
        key_values = sorted(
            set(key_values) | {"<"},
            key=lambda s: (0 if s == "<" else 1, len(s), s.lower()),
        )
        ttk.Combobox(
            redraw,
            textvariable=self.redraw_key,
            values=key_values,
            width=10,
        ).pack(side=tk.LEFT)
        for label, var in (
            ("Ctrl", self.redraw_ctrl),
            ("Alt", self.redraw_alt),
            ("Shift", self.redraw_shift),
            ("Super", self.redraw_super),
            ("Fn", self.redraw_requires_fn),
        ):
            ttk.Checkbutton(redraw, text=label, variable=var).pack(side=tk.LEFT, padx=4)
        ttk.Button(redraw, text="Set redraw", command=self.set_redraw_shortcut).pack(side=tk.LEFT, padx=6)
        ttk.Button(redraw, text="Default (Fn+<)", command=self.reset_redraw_shortcut).pack(side=tk.LEFT, padx=4)

        tk.Label(self, textvariable=self.detail, bg=BG, fg=TEXT_DIM, padx=16, anchor="w").pack(fill=tk.X, pady=(0, 6))

        shell = tk.Frame(self, bg=PANEL, padx=12, pady=12)
        shell.pack(fill=tk.BOTH, expand=True, padx=16, pady=(0, 10))
        self.bar = ButtonCanvas(shell, self)
        self.bar.pack(fill=tk.X)
        self.library = Library(shell, self)
        self.library.pack(fill=tk.BOTH, expand=True, pady=(12, 0))
        self.library.refresh()

    def spin(self, parent, label, var, from_, to, inc, width=8):
        tk.Label(parent, text=label, bg=BG, fg=TEXT_DIM).pack(side=tk.LEFT, padx=(10, 4))
        ttk.Spinbox(parent, textvariable=var, from_=from_, to=to, increment=inc, width=width).pack(side=tk.LEFT)

    def layer_changed(self, _event=None):
        self.store_current_layer()
        self.current_key = dict(self.layer_defs)[self.layer_name.get()]
        self.load_layer(self.current_key)

    def media_default_changed(self):
        self.doc["MediaLayerDefault"] = bool(self.media_layer_default.get())
        self.layer_defs = all_layer_defs(self.doc)
        self.layer_combo.configure(values=[name for name, _ in self.layer_defs])
        self.load_layer(self.current_key)

    def load_layer(self, key: str):
        self.preview_settings = read_preview_settings(self.doc)
        self.bar.set_buttons(layer_items(self.doc, key))
        idx = layer_key_to_index(self.doc, key)
        shortcut = next(
            (s for s in self.doc.get("LayerShortcuts", []) if int(s.get("Layer", -1)) == idx),
            None,
        )
        self.shortcut_ctrl.set(False)
        self.shortcut_alt.set(False)
        self.shortcut_shift.set(False)
        self.shortcut_super.set(False)
        self.shortcut_requires_fn.set(False)
        if shortcut:
            sc = button_to_dict(shortcut)
            self.shortcut_key.set(shortcut_key_label(sc))
            self.shortcut_requires_fn.set(bool(sc.get("Fn", sc.get("RequireFn", False))))
            self.shortcut_ctrl.set(sc.get("Ctrl") is True)
            self.shortcut_alt.set(sc.get("Alt") is True)
            self.shortcut_shift.set(sc.get("Shift") is True)
            self.shortcut_super.set(sc.get("Super") is True)
        runtime_idx = layer_key_to_index(self.doc, key)
        shortcut_hint = format_shortcut(button_to_dict(shortcut)) if shortcut else "none"
        if self.media_layer_default.get():
            default_bar, fn_bar = "media", "function keys"
        else:
            default_bar, fn_bar = "function keys", "media"
        self.detail.set(
            f"{self.layer_name.get()} (runtime layer {runtime_idx}): "
            f"{len(self.bar.get_buttons())} buttons, shortcut {shortcut_hint}. "
            f"Fn toggles {default_bar} ↔ {fn_bar}. "
            "Drag to reorder or drag down into the library to remove."
        )

    def store_current_layer(self):
        items = self.bar.get_buttons()
        key = self.current_key
        if key.startswith("CustomBars["):
            idx = int(key.removeprefix("CustomBars[").removesuffix("]"))
            bars = [list(bar) for bar in self.doc.get("CustomBars", [])]
            while len(bars) <= idx:
                bars.append([])
            bars[idx] = items
            self.doc["CustomBars"] = bars
        else:
            self.doc[key] = items

    def select_button(self, btn):
        self.detail.set(button_detail(btn))

    def library_contains_root(self, x_root, y_root):
        x, y = self.library.winfo_rootx(), self.library.winfo_rooty()
        return x <= x_root <= x + self.library.winfo_width() and y <= y_root <= y + self.library.winfo_height()

    def add_custom_bar(self):
        self.store_current_layer()
        bars = [list(bar) for bar in self.doc.get("CustomBars", [])]
        bars.append([{"Text": "Main", "SwitchToLayer": 0}])
        self.doc["CustomBars"] = bars
        self.layer_defs = all_layer_defs(self.doc)
        self.layer_combo.configure(values=[name for name, _ in self.layer_defs])
        self.current_key = f"CustomBars[{len(bars) - 1}]"
        self.layer_name.set(f"Custom bar {len(bars)}")
        self.load_layer(self.current_key)

    def remove_custom_bar(self):
        if not self.current_key.startswith("CustomBars["):
            self.detail.set("Select a custom bar in the layer dropdown, then use Remove custom bar.")
            return
        idx = int(self.current_key.removeprefix("CustomBars[").removesuffix("]"))
        bars = [list(bar) for bar in self.doc.get("CustomBars", [])]
        if idx >= len(bars):
            return
        if not messagebox.askyesno("Remove custom bar", f"Remove custom bar {idx + 1} and its layout?"):
            return
        removed_runtime = layer_key_to_index(self.doc, self.current_key)
        self.store_current_layer()
        bars.pop(idx)
        self.doc["CustomBars"] = bars
        shortcuts = []
        for s in self.doc.get("LayerShortcuts", []):
            sc = button_to_dict(s)
            layer = int(sc.get("Layer", -1))
            if layer == removed_runtime:
                continue
            if layer > removed_runtime:
                sc["Layer"] = layer - 1
            shortcuts.append(sc)
        self.doc["LayerShortcuts"] = shortcuts
        self.layer_defs = all_layer_defs(self.doc)
        self.layer_combo.configure(values=[name for name, _ in self.layer_defs])
        if bars:
            self.current_key = f"CustomBars[{min(idx, len(bars) - 1)}]"
            self.layer_name.set(f"Custom bar {min(idx, len(bars) - 1) + 1}")
        else:
            self.current_key = self.layer_defs[0][1]
            self.layer_name.set(self.layer_defs[0][0])
        self.load_layer(self.current_key)
        self.library.refresh()

    def delete_library_button(self, index: int):
        library = [button_to_dict(b) for b in self.doc.get("ButtonLibrary", [])]
        if 0 <= index < len(library):
            library.pop(index)
            self.doc["ButtonLibrary"] = library
            self.library.refresh()

    def edit_selected_button(self):
        btn = self.bar.selected_button()
        if btn is None:
            self.detail.set("Select a button on the bar first.")
            return
        self.custom_button_dialog(initial=btn, replace=True)

    def custom_button_dialog(self, initial: dict | None = None, replace: bool = False):
        win = tk.Toplevel(self)
        win.title("Edit button" if replace else "Custom button")
        win.configure(bg=BG)
        win.minsize(520, 480)

        text = tk.StringVar(value=str((initial or {}).get("Text", "")))
        icon = tk.StringVar(value=str((initial or {}).get("Icon", "")))
        path = tk.StringVar(value=str((initial or {}).get("Path", "")))
        action = tk.StringVar(value=str((initial or {}).get("Action", "")))
        command = tk.StringVar(value=str((initial or {}).get("Command", "")))
        switch_layer = tk.StringVar(
            value=str((initial or {}).get("SwitchToLayer", ""))
            if (initial or {}).get("SwitchToLayer") is not None
            else ""
        )
        mode = tk.StringVar(value=str((initial or {}).get("Mode", "Media")))
        layer_labels = [label for label, _value in layer_switch_choices(self.doc)]
        layer_values = [_value for _label, _value in layer_switch_choices(self.doc)]
        if switch_layer.get() and switch_layer.get() in layer_values:
            switch_pick = tk.StringVar(value=layer_labels[layer_values.index(switch_layer.get())])
        else:
            switch_pick = tk.StringVar(value=layer_labels[0])

        icon_names = list_theme_icons(
            self.preview_settings["media_theme"],
            self.preview_settings["app_theme"],
            mode=mode.get(),
        )

        help_text = (
            "Text label OR icon (not both). Icon: theme name without .svg (e.g. go-previous; "
            "-symbolic is added automatically). Path: optional .svg/.png that overrides Icon. "
            "Action: key sent while held (F1, VolumeUp, …). Command: shell command on tap. "
            "Switch bar: jump to another layer when you tap the button."
        )
        tk.Label(win, text=help_text, bg=BG, fg=TEXT_DIM, wraplength=480, justify=tk.LEFT).grid(
            row=0, column=0, columnspan=3, sticky="w", padx=12, pady=(10, 8)
        )

        row = 1
        tk.Label(win, text="Text (label)", bg=BG, fg=TEXT_DIM).grid(row=row, column=0, sticky="e", padx=8, pady=5)
        tk.Entry(win, textvariable=text, width=40, bg=PANEL_2, fg=TEXT, insertbackground=TEXT, relief=tk.FLAT).grid(
            row=row, column=1, columnspan=2, sticky="w", padx=8, pady=5
        )
        row += 1

        tk.Label(win, text="Icon name", bg=BG, fg=TEXT_DIM).grid(row=row, column=0, sticky="e", padx=8, pady=5)
        icon_combo = ttk.Combobox(win, textvariable=icon, values=icon_names, width=36)
        icon_combo.grid(row=row, column=1, sticky="w", padx=8, pady=5)

        def refresh_icons():
            names = list_theme_icons(
                self.preview_settings["media_theme"],
                self.preview_settings["app_theme"],
                mode=mode.get(),
            )
            icon_combo.configure(values=names)

        row += 1
        tk.Label(win, text="Mode", bg=BG, fg=TEXT_DIM).grid(row=row, column=0, sticky="e", padx=8, pady=5)
        mode_combo = ttk.Combobox(win, textvariable=mode, values=("Media", "App"), state="readonly", width=12)
        mode_combo.grid(row=row, column=1, sticky="w", padx=8, pady=5)
        mode_combo.bind("<<ComboboxSelected>>", lambda _e: refresh_icons())
        row += 1

        tk.Label(win, text="Path (override)", bg=BG, fg=TEXT_DIM).grid(row=row, column=0, sticky="e", padx=8, pady=5)
        tk.Entry(win, textvariable=path, width=40, bg=PANEL_2, fg=TEXT, insertbackground=TEXT, relief=tk.FLAT).grid(
            row=row, column=1, sticky="w", padx=8, pady=5
        )
        tk.Button(
            win,
            text="Browse",
            command=lambda: path.set(filedialog.askopenfilename(parent=win) or path.get()),
            bg=CHIP,
            fg=TEXT,
            relief=tk.FLAT,
        ).grid(row=row, column=2, padx=6)
        row += 1

        tk.Label(win, text="Action (key)", bg=BG, fg=TEXT_DIM).grid(row=row, column=0, sticky="e", padx=8, pady=5)
        ttk.Combobox(win, textvariable=action, values=COMMON_ACTIONS, width=36).grid(
            row=row, column=1, columnspan=2, sticky="w", padx=8, pady=5
        )
        row += 1

        tk.Label(win, text="Command", bg=BG, fg=TEXT_DIM).grid(row=row, column=0, sticky="e", padx=8, pady=5)
        tk.Entry(win, textvariable=command, width=40, bg=PANEL_2, fg=TEXT, insertbackground=TEXT, relief=tk.FLAT).grid(
            row=row, column=1, columnspan=2, sticky="w", padx=8, pady=5
        )
        row += 1

        tk.Label(win, text="Switch to bar", bg=BG, fg=TEXT_DIM).grid(row=row, column=0, sticky="e", padx=8, pady=5)
        switch_combo = ttk.Combobox(win, textvariable=switch_pick, values=layer_labels, state="readonly", width=36)
        switch_combo.grid(row=row, column=1, columnspan=2, sticky="w", padx=8, pady=5)
        row += 1

        tk.Label(
            win,
            text="Command buttons need AllowRootCommands=true; they run as your login user (for wl-copy etc.).",
            bg=BG,
            fg=WARN,
            wraplength=480,
            justify=tk.LEFT,
        ).grid(row=row, column=0, columnspan=3, padx=12, pady=8)
        row += 1

        def build_button() -> dict:
            btn: dict = {}
            if icon.get().strip():
                btn["Icon"] = icon.get().strip()
                btn["Mode"] = mode.get()
            elif text.get().strip():
                btn["Text"] = text.get().strip()
            if path.get().strip():
                btn["Path"] = path.get().strip()
            if action.get().strip():
                btn["Action"] = action.get().strip()
            if command.get().strip():
                btn["Command"] = command.get().strip()
            pick = switch_pick.get()
            if pick and pick != layer_labels[0]:
                idx = layer_labels.index(pick)
                layer_val = layer_values[idx]
                if layer_val:
                    btn["SwitchToLayer"] = int(layer_val)
            if not any(k in btn for k in ("Text", "Icon")):
                btn["Text"] = btn.get("Action") or btn.get("Command") or "Button"
            return btn

        def apply():
            btn = build_button()
            if btn.get("Command"):
                self.doc["AllowRootCommands"] = True
            if not replace:
                library = [button_to_dict(b) for b in self.doc.get("ButtonLibrary", [])]
                library.append(btn)
                self.doc["ButtonLibrary"] = library
            self.library.refresh()
            if replace:
                self.bar.replace_selected(btn)
            else:
                self.bar.add_button(btn)
            win.destroy()

        buttons = tk.Frame(win, bg=BG)
        buttons.grid(row=row, column=0, columnspan=3, sticky="e", padx=12, pady=12)
        tk.Button(buttons, text="Cancel", command=win.destroy, bg=CHIP, fg=TEXT, relief=tk.FLAT, padx=10).pack(
            side=tk.RIGHT
        )
        tk.Button(
            buttons,
            text="Apply" if replace else "Add",
            command=apply,
            bg=ACCENT,
            fg="#ffffff",
            relief=tk.FLAT,
            padx=10,
        ).pack(side=tk.RIGHT, padx=8)

    def shortcut_entry_from_ui(self) -> dict | None:
        key_name = normalize_key_name(self.shortcut_key.get())
        if not key_name:
            return None
        entry: dict = {"Layer": layer_key_to_index(self.doc, self.current_key)}
        if key_name in EVDEV_KEY_BY_NAME:
            entry["Code"] = EVDEV_KEY_BY_NAME[key_name]
        elif key_name.isdigit():
            entry["Code"] = int(key_name)
        else:
            entry["Key"] = key_name
        if self.shortcut_requires_fn.get():
            entry["Fn"] = True
        else:
            entry["Fn"] = False
        for field, var in (
            ("Ctrl", self.shortcut_ctrl),
            ("Alt", self.shortcut_alt),
            ("Shift", self.shortcut_shift),
            ("Super", self.shortcut_super),
        ):
            if var.get():
                entry[field] = True
        return entry

    def set_current_shortcut(self):
        self.store_current_layer()
        entry = self.shortcut_entry_from_ui()
        if entry is None:
            self.detail.set("Enter a key name (e.g. F1, 1, Space) for the shortcut.")
            return
        target = entry["Layer"]
        shortcuts = [
            button_to_dict(s)
            for s in self.doc.get("LayerShortcuts", [])
            if int(s.get("Layer", -1)) != target
        ]
        shortcuts.append(entry)
        shortcuts.sort(key=lambda s: int(s.get("Layer", 0)))
        self.doc["LayerShortcuts"] = shortcuts
        self.detail.set(
            f"Shortcut saved for {self.layer_name.get()} (layer {target}): {format_shortcut(entry)}."
        )

    def clear_current_shortcut(self):
        self.store_current_layer()
        target = layer_key_to_index(self.doc, self.current_key)
        shortcuts = [
            button_to_dict(s)
            for s in self.doc.get("LayerShortcuts", [])
            if int(s.get("Layer", -1)) != target
        ]
        self.doc["LayerShortcuts"] = shortcuts
        self.detail.set(f"Shortcut cleared for {self.layer_name.get()}.")

    def load_redraw_shortcut_ui(self):
        sc = button_to_dict(self.doc.get("RedrawShortcut", default_redraw_shortcut()))
        self.redraw_key.set(shortcut_key_label(sc))
        self.redraw_requires_fn.set(bool(sc.get("Fn", sc.get("RequireFn", True))))
        self.redraw_ctrl.set(sc.get("Ctrl") is True)
        self.redraw_alt.set(sc.get("Alt") is True)
        self.redraw_shift.set(sc.get("Shift") is True)
        self.redraw_super.set(sc.get("Super") is True)

    def redraw_entry_from_ui(self) -> dict:
        key_name = normalize_key_name(self.redraw_key.get())
        if not key_name:
            return default_redraw_shortcut()
        entry: dict = {}
        if key_name in EVDEV_KEY_BY_NAME:
            entry["Code"] = EVDEV_KEY_BY_NAME[key_name]
        elif key_name.isdigit():
            entry["Code"] = int(key_name)
        else:
            entry["Key"] = key_name
        entry["Fn"] = self.redraw_requires_fn.get()
        for field, var in (
            ("Ctrl", self.redraw_ctrl),
            ("Alt", self.redraw_alt),
            ("Shift", self.redraw_shift),
            ("Super", self.redraw_super),
        ):
            if var.get():
                entry[field] = True
            else:
                entry[field] = False
        return entry

    def set_redraw_shortcut(self):
        entry = sanitize_shortcut(self.redraw_entry_from_ui())
        self.doc["RedrawShortcut"] = entry
        self.detail.set(f"Force redraw shortcut: {format_shortcut(entry)}.")

    def reset_redraw_shortcut(self):
        self.doc["RedrawShortcut"] = default_redraw_shortcut()
        self.load_redraw_shortcut_ui()
        self.detail.set(f"Force redraw shortcut reset to {format_shortcut(default_redraw_shortcut())}.")

    def write_settings(self, doc):
        doc["MediaLayerDefault"] = bool(self.media_layer_default.get())
        doc["DimTimeoutMs"] = int(self.dim_ms.get())
        doc["OffTimeoutMs"] = int(self.off_ms.get())
        doc["DimmedBrightness"] = int(self.dim_brightness.get())
        doc["ActiveBrightness"] = int(self.active_brightness.get())
        doc["FnMode"] = self.fn_mode.get()
        doc["FnTogglePressMs"] = int(self.fn_tap_ms.get())
        doc["RedrawShortcut"] = sanitize_shortcut(
            self.doc.get("RedrawShortcut", self.redraw_entry_from_ui())
        )
        if "AllowRootCommands" in self.doc:
            doc["AllowRootCommands"] = bool(self.doc["AllowRootCommands"])

    def materialize_doc(self):
        self.store_current_layer()
        out = tomlkit.parse(TEMPLATE_PATH.read_text(encoding="utf-8"))
        self.write_settings(out)
        for _name, key in self.layer_defs:
            if key.startswith("CustomBars["):
                continue
            items = [sanitize_button(b, self.doc) for b in layer_items(self.doc, key)]
            if items:
                out[key] = button_array(items)
        bars = tomlkit.array()
        bars.multiline(True)
        for bar in self.doc.get("CustomBars", []):
            items = [sanitize_button(b, self.doc) for b in bar]
            if items:
                bars.append(button_array(items))
        out["CustomBars"] = bars
        if self.doc.get("LayerShortcuts"):
            out["LayerShortcuts"] = button_array(
                [sanitize_shortcut(s) for s in self.doc["LayerShortcuts"]]
            )
        else:
            out.pop("LayerShortcuts", None)
        if self.doc.get("RedrawShortcut"):
            out["RedrawShortcut"] = inline_table(sanitize_shortcut(self.doc["RedrawShortcut"]))
        else:
            out.pop("RedrawShortcut", None)
        library = [sanitize_button(b, self.doc) for b in self.doc.get("ButtonLibrary", [])]
        if library:
            out["ButtonLibrary"] = button_array(library)
        else:
            out.pop("ButtonLibrary", None)
        return out

    def save(self, restart: bool):
        self.set_redraw_shortcut()
        out = self.materialize_doc()
        if not CONFIG_PATH.parent.exists():
            messagebox.showerror("Missing directory", str(CONFIG_PATH.parent))
            return
        text = tomlkit.dumps(out)
        try:
            import tomllib

            tomllib.loads(text)
        except Exception as exc:
            messagebox.showerror(
                "Config syntax error",
                f"The generated config is not valid TOML:\n{exc}",
            )
            return
        try:
            if CONFIG_PATH.exists():
                shutil.copy2(CONFIG_PATH, CONFIG_PATH.with_suffix(CONFIG_PATH.suffix + BACKUP_SUFFIX))
            CONFIG_PATH.write_text(text, encoding="utf-8")
        except PermissionError:
            self.save_with_pkexec(text, restart)
            return
        if restart:
            err = self.restart_service()
            if err:
                messagebox.showwarning("Saved", f"Saved, but restart failed:\n{err}")
                return
        messagebox.showinfo("Saved", "Configuration saved.")

    def save_with_pkexec(self, text: str, restart: bool):
        with tempfile.NamedTemporaryFile("w", suffix=".toml", delete=False, encoding="utf-8") as f:
            f.write(text)
            tmp = f.name
        cmd = f"cp '{tmp}' '{CONFIG_PATH}'"
        if restart:
            cmd += " && systemctl restart tiny-dfr"
        try:
            subprocess.run(["pkexec", "sh", "-c", cmd], check=True, timeout=30)
            messagebox.showinfo("Saved", "Configuration saved.")
        except Exception as exc:
            messagebox.showerror("Save failed", f"{exc}\n\nRun with sudo if pkexec is unavailable.")
        finally:
            Path(tmp).unlink(missing_ok=True)

    def restart_service(self):
        try:
            subprocess.run(["systemctl", "restart", "tiny-dfr"], check=True, timeout=15, capture_output=True, text=True)
            return None
        except subprocess.CalledProcessError as exc:
            try:
                subprocess.run(["pkexec", "systemctl", "restart", "tiny-dfr"], check=True, timeout=20)
                return None
            except Exception as exc2:
                return (exc.stderr or str(exc)) + "\n" + str(exc2)


def main():
    app = App()
    app.mainloop()


if __name__ == "__main__":
    main()
