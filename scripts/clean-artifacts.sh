#!/usr/bin/env bash
#
# clean-artifacts.sh — убрать из дерева результаты сборки, кэши и мусор ОС,
# чтобы архив для бэкапа нёс только исходники и то, без чего не собрать
# бинарники.
#
# ХРАНИТСЯ (не трогаем):
#   - весь код, Cargo.toml/Cargo.lock, proto/, scripts/, иконки и ассеты GUI,
#     package.json и package-lock.json фронтенда;
#   - .cargo/config.toml — lld-link и статический CRT: без него бинарник тянет
#     VCRUNTIME140.dll, которой в WinPE нет, и умирает до первой строки кода;
#   - заметки и история: .git, .ai, .claude, design/, image/ — всё, кроме .git,
#     лежит вне репозитория, и архив для них единственная копия.
#
# УДАЛЯЕТСЯ по --force (пересоберётся само):
#   - target/ (обычно десятки гигабайт), frontend/node_modules, frontend/dist,
#     кэши .svelte-kit / .vite;
#   - собранные exe и pdb в win-files/ и linux-files/ (сами каталоги остаются);
#   - мусор ОС и редактора (*Zone.Identifier, .DS_Store, *.swp, *.orig, *.rs.bk).
#
# НЕ УДАЛЯЕТСЯ, но и В АРХИВ НЕ ИДЁТ:
#   - win-files/ и linux-files/ целиком: там точки монтирования дисков Windows
#     с многогигабайтными образами — rm -rf по ним стёр бы данные на
#     смонтированном диске, а tar утащил бы их в архив;
#   - tmp/ — локальные репро;
#   - образы и артефакты тестов где угодно в дереве (*.vhdx, *.avhdx, *.obkm,
#     *.obk, *.vmdk, *.wim, *.iso).
#
# Использование:
#   scripts/clean-artifacts.sh                  # ПРОБНЫЙ ПРОГОН — что уйдёт и сколько это
#   scripts/clean-artifacts.sh --force          # собственно удалить артефакты
#   scripts/clean-artifacts.sh --archive [OUT]  # собрать лёгкий .tar.gz (ничего не удаляя)
#                                               # по умолчанию — в ТЕКУЩИЙ каталог
#   scripts/clean-artifacts.sh --archive --with-git [OUT]   # ...вместе с историей .git
#   scripts/clean-artifacts.sh --help
#
set -euo pipefail

INVOKE_DIR="$(pwd)"          # откуда запустили — туда и ляжет архив
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE="$(basename "$ROOT")"
cd "$ROOT"

# Чистый результат сборки и кэши (пути от корня репозитория).
ARTIFACT_DIRS=(
  "target"
  "crates/gui/frontend/node_modules"
  "crates/gui/frontend/dist"
  "crates/gui/frontend/.svelte-kit"
  "crates/gui/frontend/.vite"
  "crates/gui/frontend/.vite-temp"
  "log"
)

# Собранные бинарники: лежат в каталогах, которые сами трогать нельзя
# (win-files/* — точки монтирования). Удаляются только обычные файлы.
BUILT_BIN_GLOBS=(
  "win-files/*.exe"
  "win-files/*.pdb"
  "linux-files/*"
  "*.exe"
  "*.pdb"
)

# Мусор ОС и редактора (ищется по всему дереву).
JUNK_GLOBS=("*.DS_Store" "Thumbs.db" "*Zone.Identifier" "*.swp" "*.swo" "*.orig" "*.rej" "*~" "*.rs.bk")

# Образы и тестовые данные: в архив не идут никогда, но и не удаляются —
# среди них может оказаться копия, ради которой всё и затевалось.
TEST_IMAGE_GLOBS=("*.vhdx" "*.avhdx" "*.obkm" "*.obk" "*.vmdk" "*.wim" "*.iso")

# Не попадает В АРХИВ, но --force это не трогает.
ARCHIVE_EXTRA_EXCLUDES=("win-files" "linux-files" "tmp" ".idea" "*.iml")

human() { du -sh "$1" 2>/dev/null | cut -f1; }

usage() { sed -n '2,/^set -euo/p' "$0" | grep '^#' | sed 's/^# \{0,1\}//'; exit 0; }

# find по дереву без target/.git/node_modules — иначе обход 16 ГБ ради пары
# файлов мусора. win-files/ и linux-files/ тоже мимо: там точки монтирования, и
# обход смонтированного диска на 2 ТБ через drvfs занял бы минуты, а удаление
# по итогам писало бы на чужой раздел.
prune_find() {
  find . -path ./.git -prune -o -path ./target -prune -o -name node_modules -prune \
       -o -path ./win-files -prune -o -path ./linux-files -prune -o "$@"
}

# ── разбор аргументов ────────────────────────────────────────────────────────
MODE="dry"          # dry | force | archive
WITH_GIT=0
OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --force|-f)   MODE="force" ;;
    --archive|-a) MODE="archive" ;;
    --with-git)   WITH_GIT=1 ;;
    -h|--help)    usage ;;
    -*)           echo "неизвестный ключ: $1" >&2; exit 2 ;;
    *)            OUT="$1" ;;   # позиционный → путь архива
  esac
  shift
done

# ── режим архива ─────────────────────────────────────────────────────────────
if [ "$MODE" = "archive" ]; then
  ts="$(date +%Y%m%d-%H%M%S)"
  if [ -z "$OUT" ]; then
    case "$BASE" in
      *backup) OUT="$INVOKE_DIR/${BASE}-${ts}.tar.gz" ;;      # own-backup-backup-... ни к чему
      *)       OUT="$INVOKE_DIR/${BASE}-backup-${ts}.tar.gz" ;;
    esac
  elif [ "${OUT#/}" = "$OUT" ]; then
    OUT="$INVOKE_DIR/$OUT"                                    # относительный — от каталога запуска
  fi

  excludes=()
  excludes+=(--exclude='*.tar.gz')      # не паковать архив в самого себя и не тащить старые
  for d in "${ARTIFACT_DIRS[@]}";     do excludes+=(--exclude="$BASE/$d"); done
  excludes+=(--exclude='node_modules' --exclude='.svelte-kit' --exclude='.vite' --exclude='.vite-temp')
  for g in "${JUNK_GLOBS[@]}";        do excludes+=(--exclude="$g"); done
  for g in "${TEST_IMAGE_GLOBS[@]}";  do excludes+=(--exclude="$g"); done
  for e in "${ARCHIVE_EXTRA_EXCLUDES[@]}"; do
    case "$e" in
      *.*|.*) excludes+=(--exclude="$e") ;;                   # шаблон (*.iml) или скрытый каталог (.idea)
    esac
    excludes+=(--exclude="$BASE/$e")                          # и копия в корне репозитория
  done
  [ "$WITH_GIT" -eq 0 ] && excludes+=(--exclude="$BASE/.git")

  echo "Собираю архив (весь код плюс .cargo/config.toml, .ai, design/, image/ —"
  echo "того, что вне git, это единственная копия):"
  echo "  -> $OUT"
  [ "$WITH_GIT" -eq 0 ] && echo "  (история .git не входит — нужна, добавьте --with-git)"
  echo "  (win-files/ и linux-files/ не входят: там точки монтирования дисков Windows)"
  tar -C "$(dirname "$ROOT")" -czf "$OUT" "${excludes[@]}" "$BASE"
  echo "Готово. Размер архива: $(human "$OUT")"
  exit 0
fi

# ── чистка / пробный прогон ──────────────────────────────────────────────────
total_kb=0
found=0
note() { printf '  %-9s %s\n' "$1" "$2"; }

echo "Артефакты под $ROOT:"
for d in "${ARTIFACT_DIRS[@]}"; do
  if [ -d "$d" ]; then
    found=1
    note "$(human "$d")" "$d/"
    total_kb=$(( total_kb + $(du -sk "$d" | cut -f1) ))
    [ "$MODE" = "force" ] && rm -rf -- "$d"
  fi
done

# Собранные бинарники. Только обычные файлы: каталоги win-files/disk-* могут
# быть смонтированными дисками Windows.
shopt -s nullglob
for pat in "${BUILT_BIN_GLOBS[@]}"; do
  for b in $pat; do
    [ -f "$b" ] || continue
    [ -L "$b" ] && continue
    found=1
    note "$(human "$b")" "$b (сборка)"
    total_kb=$(( total_kb + $(du -sk "$b" | cut -f1) ))
    [ "$MODE" = "force" ] && rm -f -- "$b"
  done
done
shopt -u nullglob

# Мусор ОС и редактора: считаем, по --force удаляем.
junk_args=()
for g in "${JUNK_GLOBS[@]}"; do junk_args+=(-name "$g" -o); done
unset 'junk_args[${#junk_args[@]}-1]'   # убрать хвостовой -o
junk_count=$(prune_find -type f \( "${junk_args[@]}" \) -print 2>/dev/null | wc -l | tr -d ' ')
if [ "$junk_count" -gt 0 ]; then
  found=1
  note "" "$junk_count файл(ов) мусора ОС/редактора (Zone.Identifier, .DS_Store, swap …)"
  [ "$MODE" = "force" ] && prune_find -type f \( "${junk_args[@]}" \) -print0 2>/dev/null | xargs -0 -r rm -f
fi

if [ "$found" -eq 0 ]; then
  echo "  (чисто — удалять нечего)"
else
  echo
  if [ "$total_kb" -ge 1024 ]; then
    printf 'Освободится: ~%s МБ\n' "$(( total_kb / 1024 ))"
  else
    printf 'Освободится: ~%s КБ\n' "$total_kb"
  fi
fi

# Образы: показываем, но не трогаем — решение за человеком.
img_args=()
for g in "${TEST_IMAGE_GLOBS[@]}"; do img_args+=(-name "$g" -o); done
unset 'img_args[${#img_args[@]}-1]'
img_list=$(prune_find -type f \( "${img_args[@]}" \) -print 2>/dev/null || true)
if [ -n "$img_list" ]; then
  echo
  echo "Образы и тестовые данные (НЕ удаляются, в архив не идут):"
  while IFS= read -r f; do note "$(human "$f")" "${f#./}"; done <<< "$img_list"
fi
if [ -d win-files ] || [ -d linux-files ]; then
  echo
  echo "  win-files/ и linux-files/ внутрь не осматривались — там могут быть"
  echo "  смонтированные диски Windows; трогаются только собранные exe/pdb в их корне."
fi

if [ "$MODE" = "force" ]; then
  echo
  echo "Удалено. ✓  (код, .cargo/config.toml, .ai, design/, image/ на месте)"
  echo "  вернуть сборку: cargo build  и  npm --prefix crates/gui/frontend install"
elif [ "$found" -ne 0 ]; then
  echo
  echo "Это был ПРОБНЫЙ ПРОГОН — ничего не удалено."
  echo "  удалить:        scripts/clean-artifacts.sh --force"
  echo "  вместо этого архив: scripts/clean-artifacts.sh --archive"
fi
