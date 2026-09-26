// Fails the build when the two message files disagree. Paraglide compiles a
// key missing from en.json without a word — the English window would quietly
// show Russian — so this is where "every string has both languages" is kept.
//
// Checked, per key: present in both files; the same parameters ({name}); an
// English text without Cyrillic (a pasted, untranslated copy). Keys no source
// file mentions are listed as a warning — they cost nothing but are usually
// the leftovers of a reworded sentence.
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const load = (loc) => {
  const j = JSON.parse(readFileSync(join(root, 'messages', `${loc}.json`), 'utf8'));
  delete j.$schema;
  return j;
};
const ru = load('ru');
const en = load('en');

/** Every text of a message: a plain string, or each variant of a plural. */
const texts = (v) =>
  typeof v === 'string' ? [v] : v.flatMap((x) => Object.values(x.match ?? {}));
/** The parameters a message takes. */
const params = (v) => {
  const out = new Set();
  if (typeof v !== 'string') {
    for (const x of v) for (const d of x.declarations ?? []) {
      const m = /^input (\w+)$/.exec(d);
      if (m) out.add(m[1]);
    }
  }
  for (const t of texts(v)) for (const m of t.matchAll(/\{(\w+)\}/g)) out.add(m[1]);
  // A plural's local selector (countPlural) is not a parameter.
  if (typeof v !== 'string') {
    for (const x of v) for (const d of x.declarations ?? []) {
      const m = /^local (\w+) =/.exec(d);
      if (m) out.delete(m[1]);
    }
  }
  return [...out].sort().join(',');
};

const errors = [];
for (const k of Object.keys(ru)) if (!(k in en)) errors.push(`en.json: missing "${k}"`);
for (const k of Object.keys(en)) if (!(k in ru)) errors.push(`ru.json: missing "${k}" (only in en.json)`);
for (const k of Object.keys(ru)) {
  if (!(k in en)) continue;
  if (params(ru[k]) !== params(en[k]))
    errors.push(`"${k}": parameters differ — ru {${params(ru[k])}} vs en {${params(en[k])}}`);
  for (const t of texts(en[k])) if (/[А-Яа-яЁё]/.test(t)) errors.push(`"${k}": English text has Cyrillic: ${t}`);
  for (const t of [...texts(ru[k]), ...texts(en[k])]) if (!t.trim()) errors.push(`"${k}": empty text`);
}

// Unused keys: a warning, not a failure.
const src = [];
const walk = (d) => {
  for (const f of readdirSync(d)) {
    const p = join(d, f);
    if (f === 'paraglide' || f === 'node_modules') continue;
    if (statSync(p).isDirectory()) walk(p);
    else if (/\.(svelte|ts)$/.test(f)) src.push(readFileSync(p, 'utf8'));
  }
};
walk(join(root, 'src'));
const all = src.join('\n');
const unused = Object.keys(ru).filter((k) => !all.includes(k));

if (unused.length) console.warn(`i18n: ${unused.length} unused key(s): ${unused.join(', ')}`);
if (errors.length) {
  console.error(`i18n: ${errors.length} problem(s):\n  ` + errors.join('\n  '));
  process.exit(1);
}
console.log(`i18n: ${Object.keys(ru).length} messages, ru and en agree`);
