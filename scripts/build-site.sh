#!/usr/bin/env bash
# Собирает одностраничный сайт (site/) вместе со снимками экрана из doc/images
# в одну папку и, с ключом --publish, выкладывает её в ветку gh-pages, из
# которой GitHub Pages отдаёт https://coinman-dev.github.io/own-backup/.
#
#   scripts/build-site.sh                 собрать в tmp/site-build
#   scripts/build-site.sh --out <папка>   собрать в другую папку
#   scripts/build-site.sh --publish       собрать и отправить в origin/gh-pages
#
# Снимки экрана в site/ не хранятся: они берутся из руководства, поэтому сайт
# показывает то же, что и doc/. Ветка gh-pages каждый раз пишется заново одним
# коммитом: это витрина, а не история. Исходники сайта живут в site/ на dev.
#
# Версию, ссылки и размеры файлов выпуска сборка берёт из API GitHub
# (scripts/site-release.py): в странице последний полный выпуск, и его видят
# поисковики. Без сети простая сборка оставляет запасные значения из
# site/index.html, а --publish останавливается. После каждого выпуска сайт
# пересобирает .github/workflows/site.yml.
#
# Для запуска в GitHub Actions: SITE_REMOTE (куда отправлять), SITE_GIT_NAME и
# SITE_GIT_EMAIL (автор коммита), SITE_COMMIT_MESSAGE.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/tmp/site-build"
publish=0
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out="$2"; shift 2 ;;
    --publish) publish=1; shift ;;
    *) echo "Неизвестный ключ: $1" >&2; exit 2 ;;
  esac
done

# Снимки, на которые ссылается страница: data-shot и data-shot-tab в index.html.
shots="$(grep -oE 'data-shot(-tab)?="[^"]+"' "$root/site/index.html" | sed -E 's/.*="([^"]+)"/\1/' | sort -u)"
[ -n "$shots" ] || { echo "В site/index.html не найдено ни одного снимка" >&2; exit 1; }

rm -rf "$out"
mkdir -p "$out"
cp -R "$root/site/." "$out/"
for lang in en ru; do
  mkdir -p "$out/img/$lang"
  for name in $shots; do
    src="$root/doc/images/$lang/$name.png"
    [ -f "$src" ] || { echo "Нет снимка: $src" >&2; exit 1; }
    cp "$src" "$out/img/$lang/"
  done
done

# Последний выпуск — в страницу.
py="$(command -v python3 || command -v python || true)"
[ -n "$py" ] || { echo "Нужен Python 3" >&2; exit 1; }
if ! "$py" "$root/scripts/site-release.py" "$out/index.html"; then
  [ "$publish" = 1 ] && { echo "Без данных о выпуске сайт не публикуется" >&2; exit 1; }
  echo "Предупреждение: в странице остались значения из site/index.html" >&2
fi

# Дата изменения в карте сайта — день сборки.
sed -i "s/__LASTMOD__/$(date +%F)/" "$out/sitemap.xml"
# Без Jekyll: файлы отдаются как есть.
: > "$out/.nojekyll"

echo "Сайт собран: $out ($(du -sh "$out" | cut -f1))"
[ "$publish" = 1 ] || exit 0

remote="${SITE_REMOTE:-$(git -C "$root" remote get-url origin)}"
rev="$(git -C "$root" rev-parse --short HEAD)"
msg="${SITE_COMMIT_MESSAGE:-Сайт: сборка из $rev}"
name="${SITE_GIT_NAME:-$(git -C "$root" config user.name)}"
email="${SITE_GIT_EMAIL:-$(git -C "$root" config user.email)}"

cd "$out"
git init -q -b gh-pages
git -c user.name="$name" -c user.email="$email" add -A
git -c user.name="$name" -c user.email="$email" commit -q -m "$msg"
git push -q -f "$remote" gh-pages:gh-pages
rm -rf "$out/.git"
echo "Отправлено в gh-pages: https://coinman-dev.github.io/own-backup/"
