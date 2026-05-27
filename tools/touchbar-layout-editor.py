#!/usr/bin/env python3
"""
Drag Touch Bar buttons to reorder them in /etc/tiny-dfr/config.toml, then save and restart tiny-dfr.
Run: ./run-layout-editor.sh   (uses system python3-tomlkit or tools/.venv)
Or: sudo apt install python3-tomlkit && python3 touchbar-layout-editor.py
Use sudo/pkexec only when saving if /etc/tiny-dfr is not writable.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

try:
    import tkinter as tk
    from tkinter import messagebox, ttk
except ImportError:
    print("tkinter is required (install python3-tk)", file=sys.stderr)
    sys.exit(1)

try:
    import tomlkit
except ImportError:
    tools_dir = Path(__file__).resolve().parent
    print(
        f"tomlkit is not installed for this Python:\n  {sys.executable}\n\n"
        "On Ubuntu/Debian (recommended):\n"
        "  sudo apt install python3-tomlkit\n\n"
        "Or use a venv in the tools folder:\n"
        f"  cd {tools_dir}\n"
        f"  {sys.executable} -m venv .venv\n"
        "  .venv/bin/pip install -r requirements.txt\n"
        "  .venv/bin/python touchbar-layout-editor.py\n\n"
        "Plain `pip install` often fails here: pip may be from pipx and PEP 668 blocks system pip.\n"
        "Icons need: sudo apt install python3-cairosvg python3-pil.imagetk",
        file=sys.stderr,
    )
    sys.exit(1)

from touchbar_render import icon_photoimage, read_preview_settings

CONFIG_PATH = Path(os.environ.get("TINY_DFR_CONFIG", "/etc/tiny-dfr/config.toml"))
TEMPLATE_PATH = Path("/usr/share/tiny-dfr/config.toml")
BACKUP_SUFFIX = ".bak.layout-editor"

# Visual theme (Touch Bar–ish dark strip)
BG = "#141418"
PANEL = "#1e1e24"
CHIP = "#2c2c34"
CHIP_HOVER = "#3a3a48"
CHIP_ACTIVE = "#0a84ff"
TEXT = "#f5f5f7"
TEXT_DIM = "#98989f"
ACCENT = "#0a84ff"

# Match tiny-dfr/src/main.rs drawing constants
TOUCHBAR_HEIGHT = 60
BUTTON_SPACING_PX = 16
TOUCHBAR_STRIP_BG = "#000000"
BUTTON_COLOR_INACTIVE = "#333333"
BUTTON_COLOR_ACTIVE = "#666666"
BUTTON_RADIUS = 8

LAYER_KEYS = {
    "Media keys (default bar)": "MediaLayerKeys",
    "Fn / F-keys layer": "PrimaryLayerKeys",
    "App layer 1 (SpecialExtendedMode)": "AppLayerKeys1",
    "App layer 2 (SpecialExtendedMode)": "AppLayerKeys2",
    "App layer 3 (SpecialExtendedMode)": "AppLayerKeys3",
}

# Keys tiny-dfr merges from /etc over /usr/share (see src/config.rs).
MERGE_KEYS = (
    "MediaLayerDefault",
    "SpecialExtendedMode",
    "ShowButtonOutlines",
    "EnablePixelShift",
    "FontRenderer",
    "FontStyle",
    "Bold",
    "Italic",
    "FontTemplate",
    "AdaptiveBrightness",
    "ActiveBrightness",
    "MediaLayerKeys",
    "PrimaryLayerKeys",
    "AppLayerKeys1",
    "AppLayerKeys2",
    "AppLayerKeys3",
)

# MacBook Pro T1 Touch Bar width; tiny-dfr prepends Esc when width >= 2170.
TOUCHBAR_WIDTH = int(os.environ.get("TINY_DFR_TOUCHBAR_WIDTH", "2170"))
BUILTIN_ESC = {"Text": "esc", "Action": "Esc", "_builtin": True}


def load_config_document() -> tuple[tomlkit.TOMLDocument, Path]:
    """Load effective config: /usr/share base with /etc overrides (same as tiny-dfr)."""
    if not TEMPLATE_PATH.is_file():
        raise FileNotFoundError(f"Template not found: {TEMPLATE_PATH}")
    doc = tomlkit.parse(TEMPLATE_PATH.read_text(encoding="utf-8"))
    source = TEMPLATE_PATH
    if CONFIG_PATH.is_file():
        user = tomlkit.parse(CONFIG_PATH.read_text(encoding="utf-8"))
        source = CONFIG_PATH
        for key in MERGE_KEYS:
            if key in user and user[key] is not None:
                doc[key] = user[key]
        for key in user:
            if key not in doc:
                doc[key] = user[key]
    return doc, source


def layer_items(doc: tomlkit.TOMLDocument, key: str) -> list:
    items = doc.get(key)
    return list(items) if items else []


def button_label(btn: dict) -> str:
    if btn.get("Text"):
        return str(btn["Text"])
    if btn.get("Icon"):
        name = str(btn["Icon"])
        return name.replace("-symbolic", "").replace("-", " ")[:18]
    if btn.get("Action"):
        return str(btn["Action"])
    return "?"


def button_to_dict(btn) -> dict:
    """Convert a tomlkit table/inline-table to a plain dict."""
    if isinstance(btn, dict) and not hasattr(btn, "unwrap"):
        return dict(btn)
    if hasattr(btn, "unwrap"):
        plain = btn.unwrap()
        return plain if isinstance(plain, dict) else dict(btn)
    return dict(btn)


def button_detail(btn: dict) -> str:
    parts = [f"Action: {btn.get('Action', '?')}"]
    if btn.get("Icon"):
        parts.append(f"Icon: {btn['Icon']}")
    if btn.get("Text"):
        parts.append(f"Text: {btn['Text']}")
    if btn.get("Mode"):
        parts.append(f"Mode: {btn['Mode']}")
    if btn.get("Background") is not None:
        parts.append(f"Background: {btn['Background']}")
    return "\n".join(parts)


def compute_base_button_width(num_slots: int) -> int:
    if num_slots <= 0:
        return 150
    return int((TOUCHBAR_WIDTH - BUTTON_SPACING_PX * (num_slots - 1)) // num_slots)


def display_width_for_button(data: dict, base_w: int) -> int:
    action = str(data.get("Action", ""))
    if data.get("_builtin"):
        return base_w
    if action == "Time" or data.get("Mode") == "Time":
        return int(base_w * 3 + BUTTON_SPACING_PX * 2)
    return base_w


def button_wants_background(data: dict) -> bool:
    bg = data.get("Background")
    if bg is not None:
        if isinstance(bg, bool):
            return bg
        return str(bg).lower() == "true"
    if data.get("Mode") == "Blank" or str(data.get("Action", "")) == "Unknown":
        return False
    return True


def should_show_button_outline(data: dict, show_outlines: bool) -> bool:
    if not show_outlines or data.get("_builtin"):
        return False
    action = str(data.get("Action", ""))
    if action in ("Unknown", "Time", "Macro1", "Macro2", "Macro3", "Macro4"):
        return False
    return button_wants_background(data)


def _draw_rounded_rect(canvas: tk.Canvas, x1: float, y1: float, x2: float, y2: float, r: float, **kwargs):
    r = min(r, (x2 - x1) / 2, (y2 - y1) / 2)
    canvas.create_arc(x1, y1, x1 + 2 * r, y1 + 2 * r, start=90, extent=90, style=tk.PIESLICE, **kwargs)
    canvas.create_arc(x2 - 2 * r, y1, x2, y1 + 2 * r, start=0, extent=90, style=tk.PIESLICE, **kwargs)
    canvas.create_arc(x2 - 2 * r, y2 - 2 * r, x2, y2, start=270, extent=90, style=tk.PIESLICE, **kwargs)
    canvas.create_arc(x1, y2 - 2 * r, x1 + 2 * r, y2, start=180, extent=90, style=tk.PIESLICE, **kwargs)
    canvas.create_rectangle(x1 + r, y1, x2 - r, y2, **kwargs)
    canvas.create_rectangle(x1, y1 + r, x2, y2 - r, **kwargs)


class TouchbarChip(tk.Canvas):
    """One Touch Bar slot drawn like tiny-dfr (black strip, rounded tile, white icon/text)."""

    def __init__(
        self,
        master,
        bar: "ButtonBar",
        index: int,
        data: dict,
        width_px: int,
        preview: dict,
        *,
        readonly: bool = False,
        **kw,
    ):
        super().__init__(
            master,
            width=width_px,
            height=TOUCHBAR_HEIGHT,
            bg=TOUCHBAR_STRIP_BG,
            highlightthickness=0,
            **kw,
        )
        self.bar = bar
        self.index = index
        self.data = data
        self.width_px = width_px
        self.preview = preview
        self.readonly = readonly
        self._dragging = False
        self._active = False
        self._icon_photo = None

        self.redraw()
        if not readonly:
            self.bind("<ButtonPress-1>", self._press)
            self.bind("<B1-Motion>", self._motion)
            self.bind("<ButtonRelease-1>", self._release)
            self.bind("<Enter>", self._enter)
            self.bind("<Leave>", self._leave)
        else:
            self.bind("<Enter>", lambda _e: self.bar.app.detail_var.set(
                "Escape is inserted at runtime on wide Touch Bars (width ≥ 2170).\n"
                "It is not stored in config.toml."
            ))

    def redraw(self, *, active: bool | None = None):
        if active is not None:
            self._active = active
        self.delete("all")
        w = max(self.width_px, 4)
        h = TOUCHBAR_HEIGHT
        bot = h * 0.15
        top = h * 0.85
        mid_y = (bot + top) / 2
        pad = 2

        if should_show_button_outline(self.data, self.preview["show_outlines"]):
            fill = BUTTON_COLOR_ACTIVE if self._active else BUTTON_COLOR_INACTIVE
            _draw_rounded_rect(self, pad, bot, w - pad, top, BUTTON_RADIUS, fill=fill, outline="")

        if self.data.get("_builtin"):
            self.create_text(w / 2, mid_y, text="esc", fill=TEXT, font=("Sans", 12, "bold"))
            return

        action = str(self.data.get("Action", ""))
        mode = self.data.get("Mode")
        mode_str = str(mode) if mode is not None else None

        if self.data.get("Text"):
            self.create_text(
                w / 2,
                mid_y,
                text=str(self.data["Text"]),
                fill="white",
                font=("Sans", 14, "bold"),
            )
        elif self.data.get("Icon"):
            icon_name = str(self.data["Icon"])
            photo = icon_photoimage(
                self,
                icon_name,
                mode=mode_str,
                media_theme=self.preview["media_theme"],
                app_theme=self.preview["app_theme"],
                custom_path=self.data.get("Path"),
            )
            if photo is not None:
                self._icon_photo = photo
                self.create_image(w / 2, mid_y, image=photo)
            else:
                short = icon_name.replace("-symbolic", "").split("-")[-1][:8]
                self.create_text(w / 2, mid_y, text=short, fill=TEXT_DIM, font=("Sans", 8))
        elif action == "Time" or mode_str == "Time":
            from datetime import datetime

            self.create_text(
                w / 2,
                mid_y,
                text=datetime.now().strftime("%H:%M"),
                fill="white",
                font=("Sans", 11, "bold"),
            )
        elif mode_str == "Blank" or action == "Unknown":
            pass
        elif action:
            self.create_text(w / 2, mid_y, text=action[:10], fill=TEXT_DIM, font=("Sans", 9))

    def _enter(self, _e):
        if self.readonly:
            return
        self.redraw(active=True)
        self.bar.app.detail_var.set(button_detail(self.data))

    def _leave(self, _e):
        if self.readonly or self._dragging:
            return
        self.redraw(active=False)

    def _press(self, e):
        if self.readonly:
            return
        self._dragging = True
        self.bar.drag_index = self.index
        self.redraw(active=True)
        self.bar.app.detail_var.set(button_detail(self.data))

    def _motion(self, e):
        if self.bar.drag_index is None:
            return
        target = self.bar.chip_at_root(e.x_root, e.y_root)
        if target is not None and target != self.bar.drag_index:
            self.bar.swap(self.bar.drag_index, target)
            self.bar.drag_index = target

    def _release(self, _e):
        self._dragging = False
        self.bar.drag_index = None
        self.redraw(active=False)


class ButtonBar(tk.Frame):
    def __init__(self, master, app: "LayoutEditorApp", **kw):
        super().__init__(master, bg=PANEL, **kw)
        self.app = app
        self.buttons: list[dict] = []
        self.chips: list[TouchbarChip] = []
        self.drag_index: int | None = None
        self.show_builtin_esc = False
        self.preview: dict = app.preview_settings

        scroll_wrap = tk.Frame(self, bg=PANEL)
        scroll_wrap.pack(fill=tk.BOTH, expand=True, padx=4, pady=4)
        self._canvas = tk.Canvas(
            scroll_wrap,
            bg=TOUCHBAR_STRIP_BG,
            highlightthickness=0,
            height=TOUCHBAR_HEIGHT + 16,
        )
        hscroll = ttk.Scrollbar(scroll_wrap, orient=tk.HORIZONTAL, command=self._canvas.xview)
        self._canvas.configure(xscrollcommand=hscroll.set)
        hscroll.pack(side=tk.BOTTOM, fill=tk.X)
        self._canvas.pack(side=tk.TOP, fill=tk.BOTH, expand=True)

        self.strip = tk.Frame(self._canvas, bg=TOUCHBAR_STRIP_BG)
        self._canvas_window = self._canvas.create_window((0, 0), window=self.strip, anchor=tk.NW)
        self.strip.bind("<Configure>", self._on_strip_configure)
        self._canvas.bind("<Configure>", self._on_canvas_configure)
        self._canvas.bind("<Shift-MouseWheel>", self._scroll_wheel)
        self._canvas.bind("<Button-4>", lambda e: self._canvas.xview_scroll(-3, "units"))
        self._canvas.bind("<Button-5>", lambda e: self._canvas.xview_scroll(3, "units"))

    def _on_strip_configure(self, _event=None):
        self._canvas.configure(scrollregion=self._canvas.bbox("all"))

    def _on_canvas_configure(self, event):
        self._canvas.itemconfigure(self._canvas_window, width=max(event.width, self.strip.winfo_reqwidth()))

    def _scroll_wheel(self, event):
        self._canvas.xview_scroll(-1 * (event.delta // 120), "units")

    def set_buttons(self, items: list, *, show_builtin_esc: bool = False):
        self.show_builtin_esc = show_builtin_esc
        self.buttons = [button_to_dict(b) for b in items]
        if show_builtin_esc:
            self.buttons.insert(0, dict(BUILTIN_ESC))
        self._rebuild()

    def get_buttons(self) -> list:
        if self.show_builtin_esc and self.buttons and self.buttons[0].get("_builtin"):
            return [b for b in self.buttons if not b.get("_builtin")]
        return self.buttons

    def _rebuild(self):
        for c in self.chips:
            c.destroy()
        self.chips.clear()
        self.preview = self.app.preview_settings
        base_w = compute_base_button_width(len(self.buttons))
        for i, b in enumerate(self.buttons):
            readonly = bool(b.get("_builtin"))
            w_px = display_width_for_button(b, base_w)
            chip = TouchbarChip(
                self.strip,
                self,
                i,
                b,
                w_px,
                self.preview,
                readonly=readonly,
            )
            chip.pack(side=tk.LEFT, padx=(0, BUTTON_SPACING_PX), pady=8)
            self.chips.append(chip)
        self.strip.update_idletasks()
        self._on_strip_configure()

    def chip_at_root(self, x_root: int, y_root: int) -> int | None:
        for i, chip in enumerate(self.chips):
            if chip.readonly:
                continue
            x, y, w, h = chip.winfo_rootx(), chip.winfo_rooty(), chip.winfo_width(), chip.winfo_height()
            if x <= x_root <= x + w and y <= y_root <= y + h:
                return i
        return None

    def swap(self, a: int, b: int):
        self.buttons[a], self.buttons[b] = self.buttons[b], self.buttons[a]
        self._rebuild()


class LayoutEditorApp(tk.Tk):
    def __init__(self):
        super().__init__()
        self.title("Touch Bar Layout Editor")
        self.configure(bg=BG)
        self.geometry("1100x360")
        self.minsize(800, 320)

        try:
            self.doc, self.source_path = load_config_document()
        except FileNotFoundError as e:
            messagebox.showerror("Config missing", str(e))
            raise SystemExit(1) from e
        self.preview_settings = read_preview_settings(self.doc)
        self.layer_var = tk.StringVar(value="Media keys (default bar)")

        self._header()
        self._bar_area()
        self._footer()
        self.load_layer()

    def _header(self):
        head = tk.Frame(self, bg=BG, padx=16, pady=12)
        head.pack(fill=tk.X)
        tk.Label(
            head,
            text="Touch Bar button layout",
            bg=BG,
            fg=TEXT,
            font=("Sans", 16, "bold"),
        ).pack(side=tk.LEFT)
        tk.Label(
            head,
            text=f"Config: {CONFIG_PATH}",
            bg=BG,
            fg=TEXT_DIM,
            font=("Sans", 9),
        ).pack(side=tk.RIGHT)

        row = tk.Frame(self, bg=BG, padx=16)
        row.pack(fill=tk.X)
        tk.Label(row, text="Edit layer:", bg=BG, fg=TEXT_DIM, font=("Sans", 10)).pack(side=tk.LEFT)
        cb = ttk.Combobox(
            row,
            textvariable=self.layer_var,
            values=list(LAYER_KEYS.keys()),
            state="readonly",
            width=28,
        )
        cb.pack(side=tk.LEFT, padx=8)
        cb.bind("<<ComboboxSelected>>", lambda _: self.load_layer())
        ttk.Button(row, text="Reload", command=self.reload).pack(side=tk.LEFT, padx=4)

        self.detail_var = tk.StringVar(value="Drag chips left/right to reorder.")
        tk.Label(
            self,
            textvariable=self.detail_var,
            bg=BG,
            fg=TEXT_DIM,
            font=("Sans", 9),
            justify=tk.LEFT,
            padx=16,
        ).pack(fill=tk.X, pady=(0, 4))

    def _bar_area(self):
        wrap = tk.Frame(self, bg=PANEL, highlightbackground="#333", highlightthickness=1)
        wrap.pack(fill=tk.BOTH, expand=True, padx=16, pady=8)
        tk.Label(
            wrap,
            text="Preview matches tiny-dfr (icons, spacing, outlines) — scroll horizontally",
            bg=PANEL,
            fg=TEXT_DIM,
            font=("Sans", 9),
        ).pack(anchor=tk.W, padx=8, pady=(6, 0))
        self.bar = ButtonBar(wrap, self)
        self.bar.pack(fill=tk.BOTH, expand=True)

    def _show_detail(self, data: dict):
        self.detail_var.set(button_detail(data))

    def _footer(self):
        foot = tk.Frame(self, bg=BG, padx=16, pady=12)
        foot.pack(fill=tk.X)
        ttk.Button(foot, text="Save && restart tiny-dfr", command=self.save).pack(side=tk.RIGHT)
        ttk.Button(foot, text="Save only", command=lambda: self.save(restart=False)).pack(side=tk.RIGHT, padx=6)
        tk.Label(
            foot,
            text="Saves the selected layer to /etc/tiny-dfr/config.toml (merged view matches tiny-dfr).",
            bg=BG,
            fg=TEXT_DIM,
            font=("Sans", 9),
        ).pack(side=tk.LEFT)

    def current_layer_key(self) -> str:
        return LAYER_KEYS[self.layer_var.get()]

    def load_layer(self):
        self.preview_settings = read_preview_settings(self.doc)
        key = self.current_layer_key()
        items = layer_items(self.doc, key)
        show_esc = TOUCHBAR_WIDTH >= 2170
        self.bar.set_buttons(items, show_builtin_esc=show_esc)
        n_cfg = len(self.bar.get_buttons())
        n_bar = len(self.bar.buttons)
        ext = self.doc.get("SpecialExtendedMode")
        ext_on = ext is True or (isinstance(ext, str) and ext.lower() == "true")
        hint = f"{n_cfg} in config"
        if show_esc:
            hint += f", {n_bar} on Touch Bar (+ Esc auto)"
        if key.startswith("AppLayer") and not ext_on:
            hint += " — enable SpecialExtendedMode in config to use app layers"
        self.detail_var.set(f"{key}: {hint}. Drag chips to reorder.")

    def reload(self):
        try:
            self.doc, self.source_path = load_config_document()
        except FileNotFoundError as e:
            messagebox.showerror("Config missing", str(e))
            return
        self.preview_settings = read_preview_settings(self.doc)
        self.load_layer()
        messagebox.showinfo("Reloaded", f"Merged config (base + {CONFIG_PATH.name})")

    def save(self, restart: bool = True):
        key = self.current_layer_key()
        new_items = self.bar.get_buttons()
        arr = tomlkit.aot()
        for b in new_items:
            t = tomlkit.table()
            for k, v in b.items():
                if k.startswith("_"):
                    continue
                t[k] = v
            arr.append(t)

        if CONFIG_PATH.is_file():
            out_doc = tomlkit.parse(CONFIG_PATH.read_text(encoding="utf-8"))
        else:
            out_doc = tomlkit.document()
        out_doc[key] = arr
        self.doc[key] = arr

        if not CONFIG_PATH.parent.is_dir():
            messagebox.showerror("Error", f"Directory missing: {CONFIG_PATH.parent}")
            return

        out_text = tomlkit.dumps(out_doc)
        try:
            if CONFIG_PATH.is_file():
                shutil.copy2(CONFIG_PATH, CONFIG_PATH.with_suffix(CONFIG_PATH.suffix + BACKUP_SUFFIX))
            CONFIG_PATH.write_text(out_text, encoding="utf-8")
        except PermissionError:
            self._save_with_pkexec(out_text, restart)
            return
        except OSError as e:
            messagebox.showerror("Save failed", str(e))
            return

        if restart:
            err = self._restart_service()
            if err:
                messagebox.showwarning("Saved", f"Config saved but restart failed:\n{err}")
            else:
                messagebox.showinfo("Saved", f"Updated {key} and restarted tiny-dfr.")
        else:
            messagebox.showinfo("Saved", f"Updated {key}.")

    def _save_with_pkexec(self, text: str, restart: bool) -> None:
        with tempfile.NamedTemporaryFile("w", suffix=".toml", delete=False, encoding="utf-8") as f:
            f.write(text)
            tmp = f.name
        cmd = f"cp '{tmp}' '{CONFIG_PATH}'"
        if restart:
            cmd += " && systemctl restart tiny-dfr"
        try:
            subprocess.run(["pkexec", "sh", "-c", cmd], check=True, timeout=30)
            messagebox.showinfo(
                "Saved",
                f"Wrote {CONFIG_PATH}" + (" and restarted tiny-dfr." if restart else "."),
            )
        except (subprocess.CalledProcessError, FileNotFoundError, subprocess.TimeoutExpired) as e:
            messagebox.showerror(
                "Need admin",
                f"Could not save via pkexec:\n{e}\n\nRun: sudo python3 {sys.argv[0]}",
            )
        finally:
            Path(tmp).unlink(missing_ok=True)

    def _restart_service(self) -> str | None:
        try:
            subprocess.run(
                ["systemctl", "restart", "tiny-dfr"],
                check=True,
                timeout=15,
                capture_output=True,
                text=True,
            )
            return None
        except subprocess.CalledProcessError as e:
            try:
                subprocess.run(
                    ["pkexec", "systemctl", "restart", "tiny-dfr"],
                    check=True,
                    timeout=15,
                )
                return None
            except Exception as e2:
                return (e.stderr or str(e)) + "\n" + str(e2)
        except FileNotFoundError:
            return "systemctl not found"


def main():
    style = ttk.Style()
    if "clam" in style.theme_names():
        style.theme_use("clam")
    app = LayoutEditorApp()
    app.mainloop()


if __name__ == "__main__":
    main()
