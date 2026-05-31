from __future__ import annotations

import io
import os
from functools import lru_cache
from pathlib import Path
from tkinter import PhotoImage

try:
    import cairosvg
    from PIL import Image, ImageTk
except Exception:
    cairosvg = None
    Image = None
    ImageTk = None


def read_preview_settings(doc) -> dict:
    return {
        "show_outlines": bool(doc.get("ShowButtonOutlines", True)),
        "media_theme": str(doc.get("MediaIconTheme", "tiny-dfr-icons")),
        "app_theme": str(doc.get("AppIconTheme", "hicolor")),
    }


def _repo_root() -> Path:
    return Path(__file__).resolve().parent


def _roots():
    return [
        Path("/etc/tiny-dfr/icons"),
        Path("/usr/share/tiny-dfr/icons"),
        Path("/usr/share/icons"),
        Path("/usr/share/pixmaps"),
        _repo_root() / "share" / "tiny-dfr" / "icons",
        _repo_root() / "share" / "icons",
    ]


def _icon_name_variants(icon_name: str) -> list[str]:
    names = [icon_name]
    if not icon_name.endswith("-symbolic"):
        names.append(f"{icon_name}-symbolic")
    expanded: list[str] = []
    for name in names:
        expanded.append(name)
        if not name.endswith(".svg"):
            expanded.append(f"{name}.svg")
        if not name.endswith(".png"):
            expanded.append(f"{name}.png")
    return expanded


def _candidate_icon_paths(icon_name: str, mode: str | None, media_theme: str, app_theme: str):
    raw = Path(icon_name)
    if raw.is_absolute() or raw.parent != Path("."):
        yield raw
        return

    theme = app_theme if mode == "App" else media_theme
    names = _icon_name_variants(icon_name)

    subdirs = [
        (),
        (theme,),
        (theme, "symbolic"),
        (theme, "symbolic", "status"),
        (theme, "symbolic", "actions"),
        (theme, "symbolic", "apps"),
        ("hicolor", "symbolic", "status"),
        ("hicolor", "symbolic", "actions"),
        ("hicolor", "symbolic", "apps"),
    ]
    for root in _roots():
        for subdir in subdirs:
            base = root.joinpath(*subdir)
            for name in names:
                yield base / name


def find_icon_path(icon_name: str, *, mode: str | None, media_theme: str, app_theme: str, custom_path=None) -> Path | None:
    candidates = [Path(custom_path)] if custom_path else list(
        _candidate_icon_paths(icon_name, mode, media_theme, app_theme)
    )
    for path in candidates:
        if path and path.is_file():
            return path
    return None


@lru_cache(maxsize=256)
def _load_icon_cached(path_str: str, size: int):
    path = Path(path_str)
    suffix = path.suffix.lower()
    if suffix == ".png":
        if Image is not None and ImageTk is not None:
            image = Image.open(path).convert("RGBA").resize((size, size), Image.LANCZOS)
            return ImageTk.PhotoImage(image)
        return PhotoImage(file=str(path))
    if suffix == ".svg" and cairosvg is not None and Image is not None and ImageTk is not None:
        png = cairosvg.svg2png(url=str(path), output_width=size, output_height=size)
        image = Image.open(io.BytesIO(png)).convert("RGBA")
        return ImageTk.PhotoImage(image)
    return None


def list_theme_icons(
    media_theme: str,
    app_theme: str,
    *,
    mode: str | None = None,
    limit: int = 400,
) -> list[str]:
    names: list[str] = []
    seen: set[str] = set()
    theme = app_theme if mode == "App" else media_theme
    subdirs = [
        (theme,),
        (theme, "symbolic"),
        (theme, "symbolic", "status"),
        (theme, "symbolic", "actions"),
        ("hicolor", "symbolic", "status"),
        ("hicolor", "symbolic", "actions"),
    ]
    for root in _roots():
        for subdir in subdirs:
            base = root.joinpath(*subdir)
            if not base.is_dir():
                continue
            for path in sorted(base.iterdir()):
                if path.suffix.lower() not in (".svg", ".png"):
                    continue
                name = path.stem
                if name.endswith("-symbolic"):
                    name = name[: -len("-symbolic")]
                if name in seen:
                    continue
                seen.add(name)
                names.append(name)
                if len(names) >= limit:
                    return sorted(names, key=str.lower)
    return sorted(names, key=str.lower)


def icon_photoimage(master, icon_name: str, *, mode: str | None, media_theme: str, app_theme: str, custom_path=None, size=28):
    path = find_icon_path(
        icon_name,
        mode=mode,
        media_theme=media_theme,
        app_theme=app_theme,
        custom_path=custom_path,
    )
    if path is None:
        return None
    try:
        # PhotoImage ownership is retained by caller; master is accepted for API compatibility.
        return _load_icon_cached(str(path), size)
    except Exception:
        return None
