#!/usr/bin/env bash
# Builds the Windows installer: the frontend, the four release executables,
# then NSIS (packaging/windows/own-backup.nsi).
#
#   sudo apt install nsis              # once, for makensis
#   scripts/build-installer.sh         # target/dist/own-backup-install.exe
#
# Pass --skip-build to package executables that are already built — from
# target/x86_64-pc-windows-msvc/release, or from the folder named by EXE_DIR.
# OUTPUT names another file for the result. That is also how the release is
# packed on Windows (.github/workflows/release.yml): `cargo build` there, then
# this script from Git Bash.
#
# Everything makensis reads is first gathered in one folder and named
# relatively from inside it. The checkout's own path therefore never reaches
# makensis, and a project kept under a folder with a Cyrillic name builds the
# same installer as one that is not.
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

skip_build=0
if [[ "${1:-}" == "--skip-build" ]]; then
    skip_build=1
fi

if ! command -v makensis >/dev/null; then
    echo 'makensis not found; install NSIS: sudo apt install nsis' >&2
    exit 1
fi

script="$root/packaging/windows/own-backup.nsi"
# The script holds the installer's Russian text as UTF-8, and makensis.exe on
# Windows reads a script without a byte-order mark in the ANSI code page. An
# editor that drops the mark on save would go unnoticed here on Linux and
# produce mojibake in the first build made on Windows.
if [[ "$(head -c 3 "$script" | od -An -tx1 | tr -d ' \n')" != "efbbbf" ]]; then
    echo "$script lost its UTF-8 byte-order mark; restore it:" >&2
    echo "  sed -i '1s/^/\xEF\xBB\xBF/' packaging/windows/own-backup.nsi" >&2
    exit 1
fi

version="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml | head -1)"
if [[ -z "$version" ]]; then
    echo 'cannot read the version from Cargo.toml' >&2
    exit 1
fi
# VIProductVersion only accepts four numbers: "0.1.1-beta" becomes 0.1.1.0.
numeric="$(printf '%s' "$version" | sed 's/[-+].*//')"
while [[ "$(tr -cd . <<<"$numeric" | wc -c)" -lt 3 ]]; do
    numeric="$numeric.0"
done

target_dir="${CARGO_TARGET_DIR:-$root/target}"
exe_dir="${EXE_DIR:-$target_dir/x86_64-pc-windows-msvc/release}"
executables=(own-backup-gui.exe own-backup-service.exe own-backup-cli.exe own-backup-shell.exe)

if [[ "$skip_build" -eq 0 ]]; then
    # The window's pages are compiled into own-backup-gui.exe, so they come first.
    (
        cd crates/gui/frontend
        [[ -d node_modules ]] || npm ci
        npm run build
    )
    cargo xwin build --locked --release --target x86_64-pc-windows-msvc \
        -p own-backup-gui -p own-backup-service -p own-backup-cli -p own-backup-shell \
        -p own-backup-tc-plugin
fi

stage="$target_dir/installer"
rm -rf "$stage"
mkdir -p "$stage"

for name in "${executables[@]}"; do
    if [[ ! -f "$exe_dir/$name" ]]; then
        echo "executable not found: $exe_dir/$name" >&2
        exit 1
    fi
    cp "$exe_dir/$name" "$stage/$name"
done

# Texts land beside the program as .txt and are opened in Notepad: CRLF line
# ends, and a byte-order mark where there is anything beyond ASCII — older
# Notepads take UTF-8 without one for the ANSI code page.
stage_text() {
    local out="$stage/$2"
    : > "$out"
    if LC_ALL=C grep -q '[^[:print:][:space:]]' "$1"; then
        printf '\xef\xbb\xbf' > "$out"
    fi
    sed 's/\r$//; s/$/\r/' "$1" >> "$out"
}
stage_text LICENSE LICENSE.txt
stage_text NOTICE NOTICE.txt
stage_text COMMERCIAL-LICENSE.md COMMERCIAL-LICENSE.txt
stage_text THIRD-PARTY-NOTICES.md THIRD-PARTY-NOTICES.txt
# The full license texts of every crate and npm package the programs carry,
# collected from those packages at build time (scripts/third-party-licenses.py).
# A package without any text stops the build rather than shipping a gap.
python3 scripts/third-party-licenses.py "$target_dir/THIRD-PARTY-LICENSES.txt"
stage_text "$target_dir/THIRD-PARTY-LICENSES.txt" THIRD-PARTY-LICENSES.txt
# The Total Commander plugin (crates/tc-plugin): a DLL named as Total
# Commander's 64-bit file system plugins are, with the description it reads
# when the plugin is installed from its folder.
if [[ ! -f "$exe_dir/own_backup_tc.dll" ]]; then
    echo "plugin not found: $exe_dir/own_backup_tc.dll" >&2
    exit 1
fi
mkdir -p "$stage/TotalCommander"
cp "$exe_dir/own_backup_tc.dll" "$stage/TotalCommander/OwnBackup.wfx64"
sed 's/\r$//; s/$/\r/' crates/tc-plugin/pluginst.inf >"$stage/TotalCommander/pluginst.inf"
cp crates/gui/icons/icon.ico "$stage/icon.ico"
cp "$script" "$stage/own-backup.nsi"

(
    cd "$stage"
    makensis -V2 -INPUTCHARSET UTF8 \
        "-DVERSION=$version" \
        "-DVERSION_NUMERIC=$numeric" \
        "-DOUTPUT=own-backup-install.exe" \
        own-backup.nsi
)

output="${OUTPUT:-$target_dir/dist/own-backup-install.exe}"
mkdir -p "$(dirname -- "$output")"
mv -f "$stage/own-backup-install.exe" "$output"

# Which builds went in: packaging a stale executable looks exactly like
# packaging a fresh one, and the installer is where that would be shipped.
echo "packed from $exe_dir:"
for name in "${executables[@]}"; do
    printf '  %s  %s\n' "$(date -r "$exe_dir/$name" '+%Y-%m-%d %H:%M')" "$name"
done
echo "installer $version: $output"
ls -l "$output"
# Beside the installer, for whoever downloads it: a release publishes both.
(
    cd "$(dirname -- "$output")"
    sha256sum "$(basename -- "$output")" > SHA256SUMS.txt
    cat SHA256SUMS.txt
)
