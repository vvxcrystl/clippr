#!/usr/bin/env bash
set -euo pipefail
repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
if command -v systemctl >/dev/null; then systemctl --user disable --now clipprd.service 2>/dev/null || true; fi
rm -f -- "$HOME/.local/bin/clipprd" "$HOME/.local/bin/clippr" "$HOME/.local/bin/clippr-gui" "$HOME/.config/systemd/user/clipprd.service"
for desktop in "$repo_dir"/packaging/*.desktop; do
    [[ -f "$desktop" ]] || continue
    rm -f -- "$HOME/.local/share/applications/$(basename -- "$desktop")"
done
if command -v systemctl >/dev/null; then systemctl --user daemon-reload 2>/dev/null || true; fi
printf '%s\n' 'Uninstalled clippr. Clips, configuration, and cached data were preserved.'
