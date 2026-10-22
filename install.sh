#!/bin/sh
# Installs everdir for the current user (no root needed). In a release tarball the prebuilt
# binary sits next to this script; in a source checkout it is built with cargo.
set -e
cd "$(dirname "$0")"

have() { command -v "$1" >/dev/null 2>&1; }

if [ -x ./everdir ]; then
    BIN=./everdir
elif have cargo; then
    BIN=target/release/everdir
else
    echo "error: cargo not found; install Rust >= 1.88 (https://rustup.rs) or use a release tarball" >&2
    exit 1
fi
[ "$BIN" = ./everdir ] || cargo build --release

if [ "$(uname -s)" = Darwin ]; then
    # A launchd agent stands in for the systemd unit; unload it with ./uninstall.sh.
    BIN_DST="$HOME/.local/bin/everdir"
    AGENT="$HOME/Library/LaunchAgents/io.github.boisenme.everdir.plist"
    install -d "$HOME/.local/bin" "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
    install -m755 "$BIN" "$BIN_DST"
    xattr -d com.apple.quarantine "$BIN_DST" 2>/dev/null || true
    sed -e "s|@BIN@|$BIN_DST|" -e "s|@LOG@|$HOME/Library/Logs/everdir.log|" dist/io.github.boisenme.everdir.plist > "$AGENT"
    launchctl bootout "gui/$(id -u)" "$AGENT" 2>/dev/null && sleep 1 || true
    launchctl bootstrap "gui/$(id -u)" "$AGENT"
    echo "everdir running at http://127.0.0.1:${EVERDIR_PORT:-7171}  (launch: everdir open, stop: everdir quit)"
    echo "Give $BIN_DST Full Disk Access (System Settings > Privacy & Security) to index all of your files."
    case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "Add ~/.local/bin to your PATH to run everdir from the terminal." ;; esac
    exit 0
fi

have gio || echo "warning: gio not found; 'Move to trash' will fail (permanent delete still works)" >&2
have xdg-open || echo "warning: xdg-open not found; opening files and the browser fallback will fail" >&2
have dbus-send || echo "warning: dbus-send not found; 'Show in folder' falls back to opening the parent folder" >&2
if ! have google-chrome-stable && ! have google-chrome && ! have chromium && ! have chromium-browser; then
    echo "warning: no Chrome/Chromium found; 'everdir open' will use the default browser instead of an app window" >&2
fi

ICON="$HOME/.local/share/icons/hicolor/scalable/apps/everdir.svg"
APPS="$HOME/.local/share/applications"
install -Dm755 "$BIN" "$HOME/.local/bin/everdir"
install -Dm644 dist/everdir.svg "$ICON"
mkdir -p "$APPS"
sed "s|^Icon=everdir$|Icon=$ICON|" dist/everdir.desktop > "$APPS/everdir.desktop"
sed "s|^Icon=everdir$|Icon=$ICON|" dist/chrome-everdir.localhost__-Default.desktop > "$APPS/chrome-everdir.localhost__-Default.desktop"
if have kbuildsycoca6; then kbuildsycoca6 >/dev/null 2>&1 || true; fi

if have systemctl && systemctl --user show-environment >/dev/null 2>&1; then
    UNIT="$HOME/.config/systemd/user/everdir.service"
    first=; [ -e "$UNIT" ] || first=1
    install -Dm644 dist/everdir.service "$UNIT"
    systemctl --user daemon-reload
    # Start at login on a fresh install; on an update keep whatever you chose in the tray menu.
    if [ -n "$first" ]; then systemctl --user enable everdir.service; fi
    systemctl --user restart everdir.service
    echo "everdir running at http://127.0.0.1:${EVERDIR_PORT:-7171}  (launch: everdir open)"
else
    echo "systemd user session not available; the service was not installed."
    echo "Start the daemon yourself, e.g. from your session's autostart:"
    echo "    $HOME/.local/bin/everdir serve"
    echo "then open the UI with: everdir open"
fi
