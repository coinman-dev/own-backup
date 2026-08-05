#!/usr/bin/env bash
# Вторая ступень приёмки исключений (этап T плана
# .ai/old2/exclusions-full-repair-plan_2026-07-28.md): смонтировать то, что
# получилось бы после восстановления, прочитать все уцелевшие файлы, писать до
# заполнения, прочитать снова, размонтировать, повторить статику.
#
# Зачем отдельно от `cargo test`: fsck судит структуру, а пригодность структуры
# судит только ядро. Регрессия C-1 (2026-07-28) была образом, который
# `btrfs check` назвал чистым, а ядро на первом же чтении отдало EIO и уронило
# init'а с SIGBUS. Ни один статический инструмент этого не показывал.
#
# Запускать ОТ СЕБЯ, не под sudo — скрипт соберёт тесты вашими правами (иначе
# target/ зарастёт root-овыми файлами) и попросит пароль только на сам прогон:
#
#   bash scripts/verify-mounted.sh            # три динамических теста
#   bash scripts/verify-mounted.sh --all      # плюс все прочие #[ignore]
#
# Нужны: root через sudo, loop-устройства, mkfs.ext4/e2fsck, mkfs.btrfs/btrfs,
# mkfs.xfs/xfs_repair и btrfs в ядре (модуль подгружается ниже).
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "$(id -u)" = "0" ]; then
    echo "Запусти от своего пользователя: сборка под root'ом оставит в target/ файлы," >&2
    echo "которые потом сломают обычный cargo build. sudo скрипт вызовет сам." >&2
    exit 2
fi

TESTS=(
    ext_exclusions::ext_integration::ext_survives_mount_and_write
    xfs_exclusions::xfs_integration::xfs_survives_mount_and_write
    btrfs_exclusions::btrfs_integration::stage2_survives_mount_and_write
)
EXTRA=()
if [ "${1:-}" = "--all" ]; then
    TESTS=()
    EXTRA=(--ignored)
    echo "== режим --all: все #[ignore]-тесты (часть сама пропустится без своих инструментов)"
fi

missing=()
for t in mkfs.ext4 e2fsck debugfs mkfs.btrfs btrfs mkfs.xfs xfs_repair; do
    command -v "$t" >/dev/null || missing+=("$t")
done
if [ ${#missing[@]} -gt 0 ]; then
    echo "== нет инструментов: ${missing[*]} — соответствующие тесты пропустятся сами"
fi

echo "== сборка тестового бинаря (вашими правами)"
BIN=$(cargo test -p own-backup-backup --lib --no-run --message-format=json 2>/dev/null |
    python3 -c '
import sys, json
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("profile", {}).get("test") and m.get("executable"):
        print(m["executable"])
' | tail -1)
[ -n "$BIN" ] || { echo "не нашёл тестовый бинарь" >&2; exit 1; }
echo "   $BIN"

echo "== sudo (пароль спросится один раз) + btrfs в ядро"
sudo -v
sudo modprobe btrfs 2>/dev/null || echo "   modprobe btrfs не сработал — btrfs-тест это покажет"

rc=0
if [ ${#TESTS[@]} -eq 0 ]; then
    sudo "$BIN" "${EXTRA[@]}" --nocapture --test-threads=1 || rc=1
else
    for t in "${TESTS[@]}"; do
        echo
        echo "== $t"
        # --exact: имена тестов пересекаются как подстроки (…survives_mount_and_write).
        sudo "$BIN" --ignored --exact "$t" --nocapture --test-threads=1 || rc=1
    done
fi

echo
if [ $rc -eq 0 ]; then
    echo "== динамическая приёмка пройдена"
else
    echo "== ДИНАМИЧЕСКАЯ ПРИЁМКА ПРОВАЛЕНА — патч метаданных нельзя выпускать" >&2
fi
exit $rc
