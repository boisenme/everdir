#!/bin/sh
# Removes everything install.sh installed. Settings and the scan cache are kept.

if [ "$(uname -s)" = Darwin ]; then
    AGENT="$HOME/Library/LaunchAgents/io.github.boisenme.everdir.plist"
    launchctl bootout "gui/$(id -u)" "$AGENT" 2>/dev/null || true
    rm -f "$HOME/.local/bin/everdir" "$AGENT" "$HOME/Library/Logs/everdir.log"
    echo "everdir uninstalled. Kept (delete by hand if you want):"
    echo "    settings:   ${XDG_CONFIG_HOME:-$HOME/.config}/everdir"
    echo "    scan cache: ${XDG_CACHE_HOME:-$HOME/.cache}/everdir"
    exit 0
fi

if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
    systemctl --user disable --now everdir.service 2>/dev/null || true
fi

rm -f "$HOME/.local/bin/everdir" \
      "$HOME/.config/systemd/user/everdir.service" \
      "$HOME/.local/share/icons/hicolor/scalable/apps/everdir.svg" \
      "$HOME/.local/share/applications/everdir.desktop" \
      "$HOME/.local/share/applications/chrome-everdir.localhost__-Default.desktop"

if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
    systemctl --user daemon-reload
fi
if command -v kbuildsycoca6 >/dev/null 2>&1; then kbuildsycoca6 >/dev/null 2>&1 || true; fi

echo "everdir uninstalled. Kept (delete by hand if you want):"
echo "    settings:   ${XDG_CONFIG_HOME:-$HOME/.config}/everdir"
echo "    scan cache: ${XDG_CACHE_HOME:-$HOME/.cache}/everdir"
