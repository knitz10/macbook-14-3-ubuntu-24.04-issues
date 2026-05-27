"""Resolve tiny-dfr icons and rasterize SVGs for the layout editor preview."""

from __future__ import annotations

import subprocess
from io import BytesIO
from pathlib import Path

ICON_SIZE = 48
ICON_SEARCH_DIRS = (
    Path("/etc/tiny-dfr/icons"),
    Path("/usr/share/tiny-dfr/icons"),
    Path("/usr/share/icons"),
)
SYMBOLIC_SUBDIRS = ("status", "actions", "apps", "devices", "places", "mimetypes")


def read_preview_settings(doc) -> dict:
    """Pull display-related options from merged config document."""
    def _bool(key: str, default: bool) -> bool:
        v = doc.get(key)
        if v is None:
            return default
        if isinstance(v, bool):
            return v
        return str(v).lower() in ("true", "1", "yes")

    return {
        "show_outlines": _bool("ShowButtonOutlines", True),
        "media_theme": str(doc.get("MediaIconTheme") or "tiny-dfr-icons"),
        "app_theme": str(doc.get("AppIconTheme") or "hicolor"),
    }


def find_icon_file(
    icon_name: str,
    *,
    mode: str | None,
    media_theme: str,
    app_theme: str,
    custom_path: str | None = None,
) -> Path | None:
    if custom_path:
        p = Path(custom_path)
        if p.is_file():
            return p

    theme = app_theme if mode == "App" else media_theme
    names = [icon_name]
    if not icon_name.endswith((".svg", ".png")):
        names.extend([f"{icon_name}.svg", f"{icon_name}.png"])

    for base in ICON_SEARCH_DIRS:
        if not base.is_dir():
            continue
        for name in names:
            stem = Path(name).stem
            ext = Path(name).suffix or ".svg"
            candidates = [
                base / theme / "symbolic" / sub / f"{stem}{ext}"
                for sub in SYMBOLIC_SUBDIRS
            ]
            candidates += [
                base / theme / "symbolic" / f"{stem}{ext}",
                base / theme / f"{stem}{ext}",
            ]
            candidates += [
                base / theme / "48x48" / sub / f"{stem}{ext}"
                for sub in SYMBOLIC_SUBDIRS
            ]
            candidates.append(base / theme / "48x48" / f"{stem}{ext}")
            for path in candidates:
                if path.is_file():
                    return path

    for name in names:
        stem = Path(name).stem
        for ext in (".svg", ".png"):
            p = Path(f"/usr/share/pixmaps/{stem}{ext}")
            if p.is_file():
                return p
    return None


def _rasterize_cairosvg(svg_path: Path, size: int) -> bytes | None:
    try:
        import cairosvg
    except ImportError:
        return None
    return cairosvg.svg2png(url=str(svg_path), output_width=size, output_height=size)


def _rasterize_rsvg(svg_path: Path, size: int) -> bytes | None:
    try:
        out = subprocess.run(
            [
                "rsvg-convert",
                "-w",
                str(size),
                "-h",
                str(size),
                "-o",
                "-",
                str(svg_path),
            ],
            check=True,
            capture_output=True,
            timeout=10,
        )
        return out.stdout
    except (FileNotFoundError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return None


def load_icon_image(icon_name: str, *, mode: str | None, media_theme: str, app_theme: str, custom_path: str | None = None):
    """Return a PIL RGBA image or None."""
    try:
        from PIL import Image
    except ImportError:
        return None

    path = find_icon_file(
        icon_name,
        mode=mode,
        media_theme=media_theme,
        app_theme=app_theme,
        custom_path=custom_path,
    )
    if path is None:
        return None

    if path.suffix.lower() == ".png":
        img = Image.open(path).convert("RGBA")
        img.thumbnail((ICON_SIZE, ICON_SIZE), Image.Resampling.LANCZOS)
        return img

    png_bytes = _rasterize_cairosvg(path, ICON_SIZE) or _rasterize_rsvg(path, ICON_SIZE)
    if not png_bytes:
        return None
    return Image.open(BytesIO(png_bytes)).convert("RGBA")


# module-level cache: (path, mtime, size) -> PhotoImage
_photo_cache: dict[tuple, object] = {}


def icon_photoimage(master, icon_name: str, *, mode: str | None, media_theme: str, app_theme: str, custom_path: str | None = None):
    """Tk PhotoImage for an icon, with caching."""
    try:
        from PIL import ImageTk  # on Debian: apt install python3-pil.imagetk
    except ImportError:
        return None

    path = find_icon_file(
        icon_name,
        mode=mode,
        media_theme=media_theme,
        app_theme=app_theme,
        custom_path=custom_path,
    )
    if path is None:
        return None

    key = (str(path), path.stat().st_mtime_ns, ICON_SIZE)
    if key in _photo_cache:
        return _photo_cache[key]

    img = load_icon_image(
        icon_name,
        mode=mode,
        media_theme=media_theme,
        app_theme=app_theme,
        custom_path=str(path),
    )
    if img is None:
        return None
    photo = ImageTk.PhotoImage(img, master=master)
    _photo_cache[key] = photo
    return photo
