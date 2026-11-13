#!/usr/bin/env python3
"""Вписывает в собранную страницу сайта последний выпуск с GitHub.

    site-release.py <index.html> [<repo>]

Берёт самый новый выпуск, не черновик, в котором уже есть установщик .exe и
пакет .deb (пока файлы выпуска загружаются, он неполон — тогда берётся
предыдущий). Меняет в странице версию, ссылки и имена файлов, размеры, команду
apt, softwareVersion в JSON-LD и канал: data-channel="beta" для пререлиза,
"stable" для обычного выпуска (тогда пометки beta скрыты, см. styles.css).

Рядом лежащий pad.xml (описание для каталогов программ) получает версию, дату
и статус выпуска, ссылки и размер установщика; его обязательные поля и
ограничения длины проверяются при каждой сборке.

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


# Ограничения PAD 3.11 на длину текстов (в символах).
PAD_LIMITS = {
    'Keywords': 250, 'Char_Desc_45': 45, 'Char_Desc_80': 80, 'Char_Desc_250': 250,
    'Char_Desc_450': 450, 'Char_Desc_2000': 2000,
}
PAD_REQUIRED = [
    'Company_Info/Company_Name', 'Company_Info/Company_WebSite_URL',
    'Company_Info/Contact_Info/Author_Email', 'Company_Info/Contact_Info/Contact_Email',
    'Program_Info/Program_Name', 'Program_Info/Program_Version', 'Program_Info/Program_Type',
    'Program_Info/Program_Release_Status', 'Program_Info/Program_OS_Support',
    'Program_Info/Program_Category_Class', 'Program_Info/File_Info/File_Size_Bytes',
    'Program_Descriptions/English/Char_Desc_45', 'Program_Descriptions/English/Char_Desc_80',
    'Program_Descriptions/English/Char_Desc_250', 'Program_Descriptions/English/Char_Desc_450',
    'Program_Descriptions/English/Char_Desc_2000',
    'Web_Info/Application_URLs/Application_Info_URL', 'Web_Info/Application_URLs/Application_XML_File_URL',
    'Web_Info/Download_URLs/Primary_Download_URL',
]
# Фразы о бете в PAD и чем они становятся у обычного выпуска.
PAD_BETA = [
    ('Public beta. What is new:', 'What is new:'),
    ('Public beta, free for noncommercial use.', 'Free for noncommercial use.'),
    ('Публичная бета, бесплатно для некоммерческого', 'Бесплатно для некоммерческого'),
    ('This is a public beta: check an important backup by restoring it before you rely on it. ', ''),
    ('Это публичная бета: прежде чем полагаться на важную копию, проверьте её восстановлением. ', ''),
]


def check_pad(text):
    """Ошибки PAD: пустые обязательные поля и превышенные длины."""
    import xml.etree.ElementTree as ET
    try:
        root = ET.fromstring(text.encode('utf-8'))
    except ET.ParseError as e:
        return ['не XML: %s' % e]
    errors = []
    for p in PAD_REQUIRED:
        el = root.find(p)
        if el is None or not (el.text or '').strip():
            errors.append('пусто: ' + p)
    descs = root.find('Program_Descriptions')
    for lang in (list(descs) if descs is not None else []):
        for name, limit in PAD_LIMITS.items():
            el = lang.find(name)
            n = len((el.text or '') if el is not None else '')
            if n > limit:
                errors.append('%s/%s: %d символов, можно %d' % (lang.tag, name, n, limit))
    return errors


def update_pad(text, rel, assets, version, stable):
    p = Page(text)

    def el(name, value):
        p.sub('(<%s>)[^<]*(</%s>)' % (name, name), lambda m: m.group(1) + value + m.group(2))

    date = rel.get('published_at') or rel.get('created_at') or ''
    m = re.match(r'(\d{4})-(\d{2})-(\d{2})', date)
    if m:
        el('Program_Release_Year', m.group(1))
        el('Program_Release_Month', m.group(2))
        el('Program_Release_Day', m.group(3))
    el('Program_Version', version)
    el('Program_Release_Status', 'Major Update' if stable else 'Beta')
    p.sub(r'(<Program_Change_Info>[^<]*?)https://\S+?(</Program_Change_Info>)',
          lambda m: m.group(1) + rel['html_url'] + m.group(2))
    size = assets['exe']['size']
    el('File_Size_Bytes', str(size))
    el('File_Size_K', str(round(size / 1024)))
    el('File_Size_MB', '%.2f' % (size / 1048576))
    el('Primary_Download_URL', assets['exe']['browser_download_url'])
    el('Secondary_Download_URL', assets['deb']['browser_download_url'])
    if 'iso' in assets:
        el('Additional_Download_URL_1', assets['iso']['browser_download_url'])
    if stable:
        for beta, final in PAD_BETA:
            p.sub(re.escape(beta), lambda _, f=final: f)
    return p


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    path = sys.argv[1]
    repo = sys.argv[2] if len(sys.argv) > 2 else 'coinman-dev/own-backup'

    # PAD рядом со страницей проверяется всегда, даже без сети.
    pad_path = os.path.join(os.path.dirname(path), 'pad.xml')
    pad = None
    if os.path.exists(pad_path):
        with open(pad_path, encoding='utf-8') as f:
            pad = f.read()
        errors = check_pad(pad)
        if errors:
            print('site-release: pad.xml: %s' % '; '.join(errors), file=sys.stderr)
            return 1

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

    if pad is not None:
        pp = update_pad(pad, rel, assets, version, stable)
        errors = ['не найдено: %s' % m for m in pp.missing] + check_pad(pp.s)
        if errors:
            print('site-release: pad.xml: %s' % '; '.join(errors), file=sys.stderr)
            return 1
        with open(pad_path, 'w', encoding='utf-8', newline='\n') as f:
            f.write(pp.s)

    with open(path, 'w', encoding='utf-8', newline='\n') as f:
        f.write(p.s)
    print('site-release: %s (%s), файлы: %s' % (
        version, 'выпуск' if stable else 'пререлиз', ', '.join(sorted(assets))))
    return 0


if __name__ == '__main__':
    sys.exit(main())
