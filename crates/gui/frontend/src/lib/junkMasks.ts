// Junk masks as the service reads them (crates/cleanup/src/junk.rs): the same
// matching and the same refusal of masks too broad, so the window says so at
// once instead of after an analysis.

/** `*` and `?`, case-insensitive; a mask ending in a dot (`~*.`) takes names
 *  without an extension. */
export function maskMatches(mask: string, name: string): boolean {
  if (mask.length > 1 && mask.endsWith('.')) {
    return !name.includes('.') && glob(mask.slice(0, -1), name);
  }
  return glob(mask, name);
}

function glob(mask: string, name: string): boolean {
  const m = [...mask.toLowerCase()];
  const n = [...name.toLowerCase()];
  let mi = 0;
  let ni = 0;
  let star = -1;
  let mark = 0;
  while (ni < n.length) {
    if (mi < m.length && (m[mi] === '?' || m[mi] === n[ni])) {
      mi++;
      ni++;
    } else if (mi < m.length && m[mi] === '*') {
      star = mi;
      mark = ni;
      mi++;
    } else if (star >= 0) {
      mi = star + 1;
      mark++;
      ni = mark;
    } else {
      return false;
    }
  }
  while (mi < m.length && m[mi] === '*') mi++;
  return mi === m.length;
}

/** Files that are anything but junk: a mask taking one is too broad. */
const PRECIOUS = [
  'Отчёт.docx', 'report.doc', 'table.xlsx', 'slides.pptx', 'paper.pdf', 'photo.jpg', 'image.png',
  'video.mp4', 'movie.mkv', 'song.mp3', 'archive.zip', 'archive.7z', 'setup.exe', 'library.dll',
  'notes.txt', 'readme.md', 'page.html', 'main.rs', 'program.py', 'script.js', 'config.ini',
  'settings.json', 'data.xml', 'database.db', 'database.sqlite', 'key.pem', 'id_rsa', 'Makefile',
  'book.epub', 'drawing.psd', 'project.drp', 'disk.vhdx', 'copy.obk',
];
const TEMP_MARKS = ['~', '$', '^', '_', '-'];

/** Why a mask will not do, or null. */
export function maskProblem(mask: string): 'empty' | 'path' | 'broad' | null {
  const t = mask.trim();
  if (!t) return 'empty';
  if (/[\\/:]/.test(t)) return 'path';
  const literal = [...t].filter((c) => !'*?.'.includes(c));
  const saysEnough = literal.length >= 2 || literal.some((c) => TEMP_MARKS.includes(c));
  if (!saysEnough || PRECIOUS.some((p) => maskMatches(t, p))) return 'broad';
  return null;
}
