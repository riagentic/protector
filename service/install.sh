#!/bin/sh
# Build protectord and run it as a systemd user service: armed, started at
# login, restarted if it dies. Safe to re-run after every change.
set -eu
cd "$(dirname "$0")"
cargo build --release
install -Dm755 target/release/protectord "$HOME/.local/bin/protectord"
install -Dm644 protector.service "$HOME/.config/systemd/user/protector.service"
systemctl --user daemon-reload
systemctl --user enable protector
systemctl --user restart protector
systemctl --user --no-pager status protector | head -5
