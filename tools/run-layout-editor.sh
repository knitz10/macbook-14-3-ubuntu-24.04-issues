#!/usr/bin/env bash
# Run the Touch Bar layout editor with a Python that has tomlkit.
set -euo pipefail
cd "$(dirname "$0")"

if python3 -c "import tomlkit, cairosvg, PIL" 2>/dev/null; then
    exec python3 touchbar-layout-editor.py "$@"
fi

if [[ ! -d .venv ]]; then
    echo "Creating .venv and installing dependencies…" >&2
    python3 -m venv .venv
    .venv/bin/pip install -q -r requirements.txt
elif ! .venv/bin/python -c "import cairosvg" 2>/dev/null; then
    echo "Installing icon preview dependencies (cairosvg, Pillow)…" >&2
    .venv/bin/pip install -q -r requirements.txt
fi

exec .venv/bin/python touchbar-layout-editor.py "$@"
