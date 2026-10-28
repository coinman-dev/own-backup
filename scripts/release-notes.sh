#!/usr/bin/env bash
# Prints the release description of <version> in English: the file, the section
# of CHANGELOG.md for that version, the license note.
#
#   scripts/release-notes.sh 0.1.2-beta > notes.md
set -euo pipefail

version="${1:?usage: scripts/release-notes.sh <version>}"
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

# The lines under «## <version>» or «## <version> — <date>», up to the next
# version, without the blank lines at its start. GitHub shows every line break
# of a release description, so the wrapped lines of a paragraph or of a list
# item are joined into one.
changes="$(awk -v heading="## $version" '
    /^## / {
        if (found) exit
        found = ($0 == heading || index($0, heading " ") == 1)
        next
    }
    found
' "$root/CHANGELOG.md" | sed '/./,$!d' | awk '
    function flush() { if (have) print line; have = 0 }
    /^$/ { flush(); print ""; next }
    /^(#|- |[0-9]+\. |\||>)/ { flush(); line = $0; have = 1; next }
    { sub(/^ +/, ""); if (have) line = line " " $0; else { line = $0; have = 1 } }
    END { flush() }
')"
if [[ -z "${changes//[[:space:]]/}" ]]; then
    echo "CHANGELOG.md has no section for $version" >&2
    exit 1
fi

if [[ "$version" == *-* ]]; then
    kind='Prerelease'
else
    kind='Release'
fi

# The Linux package and boot medium are built locally (AGENTS.md) and attached
# to the same release; their asset names carry the Debian version with "~" as
# a dot — the name the window looks for (media_linux.rs). A release's package
# has no Debian revision (DEB_REVISION=none in build-deb.sh); "-1" and up are
# local test builds.
deb_v="$(printf '%s' "$version" | sed 's/-/./')"

cat <<EOF
$kind for Windows 10/11 x64 and Linux (Ubuntu 24.04+, Debian 13+, amd64).

- **own-backup-install_${version}.exe** — Windows setup wizard in English and Russian: installs the program for all users and starts its backup service; needs administrator rights. It installs over an earlier version, keeping settings and jobs.
- **own-backup_${deb_v}_amd64.deb** — Linux package: \`sudo apt install ./own-backup_${deb_v}_amd64.deb\` installs the program, starts its service and adds you to the \`own-backup\` group (sign in again to open the window).
- **OwnBackup-Linux-${deb_v}_amd64.iso** — Linux boot medium (Debian 13 live): boots from a CD or a USB stick, UEFI with Secure Boot or BIOS, to back up or restore the computer's disks, Windows ones included.

$changes

Windows: settings and jobs are kept in \`C:\\ProgramData\\OwnBackup\`, the service log in the \`Logs\` folder next to the program. The window needs the Microsoft Edge WebView2 Runtime: Windows 11 has it, and the installer says so if a Windows 10 does not. Linux: settings and jobs in \`/var/lib/own-backup\`, the log in journald (\`journalctl -u own-backup\`).

License: PolyForm Noncommercial 1.0.0; third-party components keep their own licenses. The checksums of the files are in SHA256SUMS.txt.
EOF
