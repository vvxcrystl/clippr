#!/usr/bin/env bash
set -euo pipefail
repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_dir"
export CARGO_TARGET_DIR=target/lib
cargo build --release --workspace
install -d "$HOME/.local/bin"
for binary in clipprd clippr clippr-gui; do
    install -m 755 "$CARGO_TARGET_DIR/release/$binary" "$HOME/.local/bin/$binary"
done
if [[ -f packaging/clipprd.service ]]; then
    install -d "$HOME/.config/systemd/user"
    install -m 644 packaging/clipprd.service "$HOME/.config/systemd/user/clipprd.service"
    if command -v systemctl >/dev/null; then systemctl --user daemon-reload || true; fi
fi
for desktop in packaging/*.desktop; do
    [[ -f "$desktop" ]] || continue
    install -d "$HOME/.local/share/applications"
    install -m 644 "$desktop" "$HOME/.local/share/applications/"
done
printf '%s\n' 'Installed clippr into ~/.local/bin. Ensure it is on PATH.' 'Enable capture with: systemctl --user enable --now clipprd'
