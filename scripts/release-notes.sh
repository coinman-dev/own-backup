#!/usr/bin/env bash
# Prints the release description of <version> in Russian: the file, the section
# of CHANGELOG.ru_RU.md for that version, the license note.
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
' "$root/CHANGELOG.ru_RU.md" | sed '/./,$!d' | awk '
    function flush() { if (have) print line; have = 0 }
    /^$/ { flush(); print ""; next }
    /^(#|- |[0-9]+\. |\||>)/ { flush(); line = $0; have = 1; next }
    { sub(/^ +/, ""); if (have) line = line " " $0; else { line = $0; have = 1 } }
    END { flush() }
')"
if [[ -z "${changes//[[:space:]]/}" ]]; then
    echo "CHANGELOG.ru_RU.md has no section for $version" >&2
    exit 1
fi

if [[ "$version" == *-* ]]; then
    kind='Предварительная версия'
else
    kind='Версия'
fi

cat <<EOF
$kind для Windows 10/11 x64.

- **own-backup-install.exe** — мастер установки на русском и английском: ставит программу для всех пользователей и запускает её службу, нужны права администратора. Ставится поверх прежней версии, настройки и задания сохраняются.

$changes

Настройки и задания хранятся в \`C:\\ProgramData\\OwnBackup\`, журнал службы — в папке \`Logs\` рядом с программой. Окну нужен Microsoft Edge WebView2 Runtime: в Windows 11 он есть, а если его нет в Windows 10, установщик об этом скажет.

Лицензия — PolyForm Noncommercial 1.0.0, сторонние компоненты — под своими лицензиями. Контрольная сумма файла — в SHA256SUMS.txt.
EOF
