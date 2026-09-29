#!/bin/sh
# Build socr and install it to ~/.local/bin (or $PREFIX/bin).
#   ./install.sh                 # user install
#   PREFIX=/usr/local sudo -E ./install.sh
set -eu
cd "$(dirname "$0")"

PREFIX="${PREFIX:-$HOME/.local}"

cargo build --release --locked

install -Dm755 target/release/socr "$PREFIX/bin/socr"
echo "installed: $PREFIX/bin/socr"

# Report missing runtime pieces for this session.
missing=""
command -v tesseract >/dev/null || missing="$missing tesseract"
if [ -n "${WAYLAND_DISPLAY:-}" ]; then
    command -v wl-copy >/dev/null || missing="$missing wl-clipboard"
elif [ -n "${DISPLAY:-}" ]; then
    command -v xclip >/dev/null || command -v xsel >/dev/null || missing="$missing xclip"
fi
[ -n "$missing" ] && echo "missing runtime dependencies:$missing"

if command -v tesseract >/dev/null && ! tesseract --list-langs 2>/dev/null | grep -qx "${SOCR_LANG:-eng}"; then
    echo "tesseract language '${SOCR_LANG:-eng}' not installed (Arch: tesseract-data-eng, Debian/Ubuntu: tesseract-ocr-eng, Fedora: tesseract-langpack-eng)"
fi

"$PREFIX/bin/socr" --list-backends
