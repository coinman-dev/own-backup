#!/usr/bin/env bash
# The Windows programs, cross-built here with cargo xwin, copied to win/ for a
# test on a Windows machine or the laboratory's (scripts/lab/run-job-menu.py).
# The installer is scripts/build-installer.sh; this is the executables alone.
#
#   scripts/build-win.sh     # win/own-backup-{gui,service,cli,shell}.exe
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
# The window's pages are compiled into own-backup-gui.exe, so they come first.
(
    cd crates/gui/frontend
    [[ -d node_modules ]] || npm ci
    npm run build
)
cargo xwin build --locked --release --target x86_64-pc-windows-msvc \
    -p own-backup-gui -p own-backup-service -p own-backup-cli -p own-backup-shell
mkdir -p win
cp target/x86_64-pc-windows-msvc/release/own-backup-{gui,service,cli,shell}.exe win/
ls -l win/own-backup-*.exe
