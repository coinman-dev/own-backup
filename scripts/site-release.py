#!/usr/bin/env python3
"""Вписывает в собранную страницу сайта последний выпуск с GitHub.

    site-release.py <index.html> [<repo>]

Берёт самый новый выпуск, не черновик, в котором уже есть установщик .exe и
пакет .deb (пока файлы выпуска загружаются, он неполон — тогда берётся
предыдущий). Меняет в странице версию, ссылки и имена файлов, размеры, команду
apt, softwareVersion в JSON-LD и канал: data-channel="beta" для пререлиза,
"stable" для обычного выпуска (тогда пометки beta скрыты, см. styles.css).

Ту же работу на лету делает main.js в браузере посетителя; этот скрипт нужен,
чтобы правильную версию видели поисковые роботы и браузеры без скриптов.

Токен (GITHUB_TOKEN или GH_TOKEN) не обязателен, но снимает лимит
анонимных запросов к API. Выход 1 — выпуск получить не удалось или разметка
страницы не нашлась.
"""
import json
import os
import re
import sys
import urllib.request

KINDS = [
    ('exe', re.compile(r'^own-backup-install.*\.exe$', re.I)),
    ('deb', re.compile(r'\.deb$', re.I)),
    ('iso', re.compile(r'\.iso$', re.I)),
    ('sums', re.compile(r'^SHA256SUMS', re.I)),
]
# У этих файлов на странице есть имя и размер; у SHA256SUMS — только ссылка.
NAMED = ('exe', 'deb', 'iso')


def fetch(repo):
    req = urllib.request.Request(
        'https://api.github.com/repos/%s/releases?per_page=10' % repo,
        headers={'Accept': 'application/vnd.github+json', 'User-Agent': 'own-backup-site-build'})
    token = os.environ.get('GITHUB_TOKEN') or os.environ.get('GH_TOKEN')
    if token:
        req.add_header('Authorization', 'Bearer ' + token)
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)


def pick(releases):
    for rel in releases:
        if rel.get('draft'):
            continue
        assets = {}
        for a in rel.get('assets') or []:
            for kind, rx in KINDS:
                if kind not in assets and rx.search(a['name']):
                    assets[kind] = a
        if 'exe' in assets and 'deb' in assets:
            return rel, assets
    return None, None


class Page:
    def __init__(self, text):
        self.s = text
        self.missing = []

    def sub(self, pattern, repl, required=True):
        self.s, k = re.subn(pattern, repl, self.s)
        if required and not k:
            self.missing.append(pattern)

    def text(self, before, value, required=True):
        # Текст элемента, открывающий тег которого кончается на before.
        self.sub('(' + before + ')[^<]*(<)', lambda m: m.group(1) + value + m.group(2), required)

    def attr(self, tag_rx, name, value, required=True):
        # Атрибут name в каждом открывающем теге, подходящем под tag_rx.
        def fix(m):
            return re.sub(r'\b%s="[^"]*"' % name, lambda _: '%s="%s"' % (name, value), m.group(0), count=1)
        self.sub(tag_rx, fix, required)


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    path = sys.argv[1]
    repo = sys.argv[2] if len(sys.argv) > 2 else 'coinman-dev/own-backup'
    try:
        rel, assets = pick(fetch(repo))
    except Exception as e:  # сеть, лимит API, ответ не JSON
        print('site-release: не удалось получить выпуски: %s' % e, file=sys.stderr)
        return 1
    if not rel:
        print('site-release: нет выпуска с .exe и .deb', file=sys.stderr)
        return 1

    with open(path, encoding='utf-8') as f:
        p = Page(f.read())
    version = re.sub(r'^v', '', rel['tag_name'])
    stable = not rel.get('prerelease')

    p.text('data-version>', version)
    p.attr(r'<a\b[^>]*\bdata-release-link\b[^>]*>', 'href', rel['html_url'])
    for kind, a in assets.items():
        p.attr(r'<a\b[^>]*\bdata-asset="%s"[^>]*>' % kind, 'href', a['browser_download_url'])
        if kind in NAMED:
            p.text('data-asset-name="%s">' % kind, a['name'])
            p.text('data-asset-size="%s">' % kind, '%d MB' % round(a['size'] / 1048576))
    p.text('<code id="debCmd">', 'sudo apt install ./' + assets['deb']['name'])
    p.sub(r'("softwareVersion":\s*")[^"]*(")', lambda m: m.group(1) + version + m.group(2))
    p.attr(r'<html\b[^>]*>', 'data-channel', 'stable' if stable else 'beta')
    if stable:
        # Заголовки и описание, которые видят поисковики, — без пометки beta.
        for rx in (r' \(beta\)', r' \(public beta\)', r' Public beta\.'):
            p.sub(rx, '', required=False)

    # Каждая обязательная разметка должна найтись: иначе index.html изменился,
    # и этот скрипт надо поправить вместе с ним.
    if p.missing:
        print('site-release: в странице не найдено: %s' % p.missing, file=sys.stderr)
        return 1
    with open(path, 'w', encoding='utf-8', newline='\n') as f:
        f.write(p.s)
    print('site-release: %s (%s), файлы: %s' % (
        version, 'выпуск' if stable else 'пререлиз', ', '.join(sorted(assets))))
    return 0


if __name__ == '__main__':
    sys.exit(main())
