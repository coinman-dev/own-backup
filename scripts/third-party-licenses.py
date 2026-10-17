#!/usr/bin/env python3
"""Full license texts of everything Own Backup ships, for THIRD-PARTY-LICENSES.txt.

    python3 scripts/third-party-licenses.py OUT.txt

`THIRD-PARTY-NOTICES.md` names the kinds of components; the licenses most of
them use (MIT, BSD, Apache-2.0 §4) ask for their text to travel with the
binary. This collects it from the sources themselves:

* Rust — every crate reachable through normal dependencies from the four
  programs, for the Windows target (`cargo metadata --filter-platform`).
  Dev- and build-only dependencies do not end up in the binaries.
* The window — the npm packages compiled into the page: the production
  dependencies, plus the Svelte runtime and Paraglide's generated code, which
  the bundler inlines although npm lists them as dev dependencies.

Each package's own license files are copied (LICENSE*, COPYING*, NOTICE*, …).
A package that ships none gets the text of the same license from another
package that does — the license is the same, only the copy was missing; the
MIT text names the package's authors. Whatever is still without a text is
listed by name at the end and makes the script exit with 2, so a build
notices instead of shipping an incomplete file.
"""
import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
TARGET = 'x86_64-pc-windows-msvc'
PROGRAMS = ('own-backup-gui', 'own-backup-service', 'own-backup-cli', 'own-backup-shell')
FRONTEND = REPO / 'crates/gui/frontend'
# Inlined into the page by the bundler though npm calls them dev dependencies.
BUNDLED_DEV = ('svelte', '@inlang/paraglide-js')

LICENSE_FILE = re.compile(r'^(LICEN[CS]E|COPYING|NOTICE|UNLICENSE|COPYRIGHT)([-_.].*)?$', re.I)
# A license file's name says which license it holds, when there are several.
SPDX_BY_NAME = [
    (re.compile(r'apache', re.I), 'Apache-2.0'),
    (re.compile(r'mit', re.I), 'MIT'),
    (re.compile(r'unlicense', re.I), 'Unlicense'),
    (re.compile(r'zlib', re.I), 'Zlib'),
    (re.compile(r'bsd', re.I), 'BSD-3-Clause'),
    (re.compile(r'mpl', re.I), 'MPL-2.0'),
    (re.compile(r'boost|bsl', re.I), 'BSL-1.0'),
    (re.compile(r'unicode', re.I), 'Unicode-3.0'),
]

MIT_TEMPLATE = """MIT License

Copyright (c) {holders}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"""


BSD3_TEMPLATE = """BSD 3-Clause License

Copyright (c) {holders}

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived from
   this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
"""
# Texts that name a copyright holder are never borrowed from another package:
# the borrowed copy would name the wrong one. These are filled in instead.
TEMPLATES = {'MIT': MIT_TEMPLATE, 'BSD-3-Clause': BSD3_TEMPLATE}


def license_files(folder: Path):
    if not folder.is_dir():
        return []
    found = []
    for p in sorted(folder.iterdir()):
        if p.is_file() and LICENSE_FILE.match(p.name):
            try:
                found.append((p.name, p.read_text(encoding='utf-8', errors='replace').strip()))
            except OSError:
                pass
    return found


def spdx_of_file(name: str):
    for rx, spdx in SPDX_BY_NAME:
        if rx.search(name):
            return spdx
    return None


def ids_in(expr: str):
    """SPDX ids an expression mentions ("MIT OR Apache-2.0", "MIT/Apache-2.0")."""
    return [t for t in re.split(r'[\s()/]+', expr or '') if t and t not in ('OR', 'AND', 'WITH')]


def rust_packages():
    out = subprocess.run(
        ['cargo', 'metadata', '--format-version', '1', '--filter-platform', TARGET, '--locked'],
        cwd=REPO, check=True, capture_output=True, text=True).stdout
    meta = json.loads(out)
    ws = set(meta['workspace_members'])
    pk = {p['id']: p for p in meta['packages']}
    nodes = {n['id']: n for n in meta['resolve']['nodes']}
    stack = [i for i in ws if pk[i]['name'] in PROGRAMS]
    seen = set()
    while stack:
        i = stack.pop()
        if i in seen:
            continue
        seen.add(i)
        for d in nodes[i]['deps']:
            if any(k['kind'] in (None, 'normal') for k in d['dep_kinds']):
                stack.append(d['pkg'])
    for i in sorted(seen - ws, key=lambda i: (pk[i]['name'], pk[i]['version'])):
        p = pk[i]
        yield {
            'name': p['name'], 'version': p['version'], 'license': p.get('license') or '',
            'authors': p.get('authors') or [], 'url': p.get('repository') or '',
            'dir': Path(p['manifest_path']).parent,
            'kind': 'crate',
        }


def npm_packages():
    # Without the installed packages there is nothing to read the texts from,
    # and an empty list here would ship a file silently missing the window's.
    if not (FRONTEND / 'node_modules').is_dir():
        sys.exit(f'{FRONTEND}/node_modules is missing: run npm ci there first')
    out = subprocess.run(['npm', 'ls', '--omit=dev', '--all', '--json'], cwd=FRONTEND,
                         capture_output=True, text=True).stdout
    names = set()

    def walk(node):
        for name, dep in (node.get('dependencies') or {}).items():
            names.add(name)
            walk(dep)
    walk(json.loads(out or '{}'))
    names.update(BUNDLED_DEV)
    for name in sorted(names):
        folder = FRONTEND / 'node_modules' / name
        manifest = folder / 'package.json'
        if not manifest.is_file():
            continue
        m = json.loads(manifest.read_text(encoding='utf-8'))
        author = m.get('author')
        if isinstance(author, dict):
            author = author.get('name')
        repo = m.get('repository')
        if isinstance(repo, dict):
            repo = repo.get('url')
        yield {
            'name': name, 'version': m.get('version', ''), 'license': m.get('license') or '',
            'authors': [author] if author else [], 'url': repo or m.get('homepage') or '',
            'dir': folder, 'kind': 'npm',
        }


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    packages = list(rust_packages()) + list(npm_packages())

    # Canonical texts by SPDX id, taken from packages that do ship them.
    # A file says which license it is by its name (LICENSE-APACHE), or, when the
    # package has one license and one file, by the package's own declaration.
    pool = {}
    for p in packages:
        files = license_files(p['dir'])
        ids = ids_in(p['license'])
        for fname, text in files:
            spdx = spdx_of_file(fname) or (ids[0] if len(ids) == 1 and len(files) == 1 else None)
            if spdx and spdx not in TEMPLATES and spdx not in pool and len(text) > 200:
                pool[spdx] = text

    # Texts grouped: one copy of each distinct text, with every package under it.
    groups = {}
    missing = []
    for p in packages:
        texts = [t for _, t in license_files(p['dir'])]
        how = 'own files'
        if not texts:
            ids = ids_in(p['license'])
            holders = ', '.join(a.split('<')[0].strip() for a in p['authors']) or f"the {p['name']} authors"
            templated = [i for i in ids if i in TEMPLATES]
            if templated:
                texts = [TEMPLATES[templated[0]].format(holders=holders)]
                how = f'{templated[0]} text with its authors, the package ships no file'
            else:
                texts = [pool[i] for i in ids if i in pool][:1]
                how = 'license text from another package, the package ships no file'
        if not texts:
            missing.append(p)
            continue
        body = '\n\n'.join(texts)
        key = hashlib.sha256(body.encode()).hexdigest()
        groups.setdefault(key, (body, []))[1].append((p, how))

    lines = [
        'Own Backup - third-party license texts',
        '',
        'Own Backup itself is under the PolyForm Noncommercial License 1.0.0 (LICENSE.txt).',
        'The components below are under their own licenses, reproduced here in full.',
        f'Rust crates: {sum(1 for p in packages if p["kind"] == "crate")}, '
        f'npm packages in the window: {sum(1 for p in packages if p["kind"] == "npm")}.',
        'Generated by scripts/third-party-licenses.py from the packages themselves.',
        '',
    ]
    for body, members in sorted(groups.values(), key=lambda g: (g[1][0][0]['name'])):
        lines.append('=' * 78)
        for p, how in sorted(members, key=lambda m: m[0]['name']):
            extra = f'  [{how}]' if how != 'own files' else ''
            lines.append(f"{p['name']} {p['version']} ({p['license'] or 'see text'}) {p['url']}{extra}".rstrip())
        lines.append('-' * 78)
        lines.append(body)
        lines.append('')
    if missing:
        lines.append('=' * 78)
        lines.append('No license text could be found for:')
        for p in missing:
            lines.append(f"  {p['name']} {p['version']} ({p['license']})")
    Path(sys.argv[1]).write_text('\n'.join(lines) + '\n', encoding='utf-8')
    print(f'{len(packages)} packages, {len(groups)} distinct texts, {len(missing)} without a text',
          file=sys.stderr)
    for p in missing:
        print(f"  no text: {p['name']} {p['version']} ({p['license']})", file=sys.stderr)
    sys.exit(2 if missing else 0)


if __name__ == '__main__':
    main()
