//! Every message a person reads exists in both languages, and every call
//! passes exactly the arguments its text takes.
//!
//! rust-i18n would compile all of it silently: a key missing from a catalog
//! comes out as the key itself («engine.open_failed»), a missing argument as a
//! literal «%{path}», an English text left empty as the Russian fallback. So
//! the whole workspace is read here instead — each crate's `locales/*.json`
//! (format v2: `"key": { "ru": …, "en": … }`) against the `tr!("key", a = …)`
//! calls in its `src/`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

struct Entry {
    ru: String,
    en: String,
    file: String,
}

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// `%{name}` placeholders of a text.
fn placeholders(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = text;
    while let Some(i) = rest.find("%{") {
        let after = &rest[i + 2..];
        if let Some(j) = after.find('}') {
            out.insert(after[..j].to_string());
            rest = &after[j + 1..];
        } else {
            break;
        }
    }
    out
}

fn has_cyrillic(s: &str) -> bool {
    s.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c))
}

fn load_catalog(dir: &Path, errors: &mut Vec<String>) -> BTreeMap<String, Entry> {
    let mut out: BTreeMap<String, Entry> = BTreeMap::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return out;
    };
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    files.sort();
    for f in files
        .into_iter()
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
    {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let text = fs::read_to_string(&f).unwrap();
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("{}: not JSON: {e}", f.display()));
                continue;
            }
        };
        let Some(obj) = v.as_object() else {
            errors.push(format!("{}: not an object", f.display()));
            continue;
        };
        if obj.get("_version").and_then(|v| v.as_i64()) != Some(2) {
            errors.push(format!("{}: needs \"_version\": 2", f.display()));
        }
        for (key, val) in obj {
            if key == "_version" {
                continue;
            }
            let ru = val.get("ru").and_then(|v| v.as_str());
            let en = val.get("en").and_then(|v| v.as_str());
            let (Some(ru), Some(en)) = (ru, en) else {
                errors.push(format!(
                    "{}: \"{key}\" needs both \"ru\" and \"en\" strings",
                    f.display()
                ));
                continue;
            };
            if let Some(prev) = out.get(key) {
                errors.push(format!(
                    "{}: \"{key}\" is also in {}",
                    f.display(),
                    prev.file
                ));
                continue;
            }
            out.insert(
                key.clone(),
                Entry {
                    ru: ru.to_string(),
                    en: en.to_string(),
                    file: name.clone(),
                },
            );
        }
    }
    out
}

/// Skip a string literal starting at `i` (at the opening quote or `r`/`b`
/// prefix); returns the index after it.
fn skip_string(s: &[u8], mut i: usize) -> usize {
    // raw string r"…" / r#"…"#
    if s[i] == b'r' || (s[i] == b'b' && i + 1 < s.len() && s[i + 1] == b'r') {
        if s[i] == b'b' {
            i += 1;
        }
        i += 1;
        let mut hashes = 0;
        while i < s.len() && s[i] == b'#' {
            hashes += 1;
            i += 1;
        }
        i += 1; // opening quote
        while i < s.len() {
            if s[i] == b'"'
                && s[i + 1..]
                    .iter()
                    .take(hashes)
                    .filter(|&&c| c == b'#')
                    .count()
                    == hashes
            {
                return i + 1 + hashes;
            }
            i += 1;
        }
        return i;
    }
    if s[i] == b'b' {
        i += 1;
    }
    i += 1; // opening quote
    while i < s.len() {
        match s[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// Skip a char literal (`'"'`, `'\\''`, `'ё'`) or a lifetime (`'a`) at `i`;
/// returns the index after it. A char literal holding a quote must not be
/// read as the start of a string — one `find('"')` hid every call below it.
fn skip_quote(s: &[u8], i: usize) -> usize {
    if i + 1 >= s.len() {
        return i + 1;
    }
    if s[i + 1] == b'\\' {
        // '\n', '\'', '\u{…}': up to the closing quote.
        let mut j = i + 2;
        while j < s.len() && j < i + 14 {
            if s[j] == b'\'' && j > i + 2 {
                return j + 1;
            }
            j += 1;
        }
        return i + 1;
    }
    // One UTF-8 character, then a quote: a char literal. Otherwise a lifetime.
    let len = match s[i + 1] {
        b if b < 0x80 => 1,
        b if b >= 0xF0 => 4,
        b if b >= 0xE0 => 3,
        _ => 2,
    };
    if i + 1 + len < s.len() && s[i + 1 + len] == b'\'' {
        i + 2 + len
    } else {
        i + 1
    }
}

/// The literal value of a plain `"…"` key (no escapes expected in keys).
fn literal(arg: &str) -> Option<String> {
    let a = arg.trim();
    (a.len() >= 2 && a.starts_with('"') && a.ends_with('"')).then(|| a[1..a.len() - 1].to_string())
}

struct Call {
    key: String,
    args: BTreeSet<String>,
    at: String,
}

/// Every `tr!(…)` in a file: its key and the names of its `name = value`
/// arguments.
fn calls_in(path: &Path, errors: &mut Vec<String>) -> Vec<Call> {
    let text = fs::read_to_string(path).unwrap();
    let s = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 1;
    while i < s.len() {
        match s[i] {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b'/' if i + 1 < s.len() && s[i + 1] == b'/' => {
                while i < s.len() && s[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < s.len() && s[i + 1] == b'*' => {
                i += 2;
                while i + 1 < s.len() && !(s[i] == b'*' && s[i + 1] == b'/') {
                    if s[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
                i += 2;
            }
            b'"' => {
                let end = skip_string(s, i);
                line += s[i..end.min(s.len())]
                    .iter()
                    .filter(|&&c| c == b'\n')
                    .count();
                i = end;
            }
            b'\'' => i = skip_quote(s, i),
            b'r' if i + 1 < s.len()
                && (s[i + 1] == b'"' || s[i + 1] == b'#')
                && (i == 0 || !s[i - 1].is_ascii_alphanumeric() && s[i - 1] != b'_') =>
            {
                let end = skip_string(s, i);
                line += s[i..end.min(s.len())]
                    .iter()
                    .filter(|&&c| c == b'\n')
                    .count();
                i = end;
            }
            b't' if s[i..].starts_with(b"tr!(")
                && (i == 0 || !(s[i - 1].is_ascii_alphanumeric() || s[i - 1] == b'_')) =>
            {
                // Split the argument list at top-level commas.
                let start = i + 4;
                let mut j = start;
                let mut depth = 0i32;
                let mut args = Vec::new();
                let mut arg_start = start;
                while j < s.len() {
                    match s[j] {
                        b'"' => {
                            j = skip_string(s, j);
                            continue;
                        }
                        b'\'' => {
                            j = skip_quote(s, j);
                            continue;
                        }
                        b'(' | b'[' | b'{' => depth += 1,
                        b')' | b']' | b'}' if depth > 0 => depth -= 1,
                        b')' => {
                            args.push(&text[arg_start..j]);
                            break;
                        }
                        b',' if depth == 0 => {
                            args.push(&text[arg_start..j]);
                            arg_start = j + 1;
                        }
                        _ => {}
                    }
                    j += 1;
                }
                let at = format!("{}:{line}", path.display());
                // The macro's own definition (`$key:literal`) is not a call.
                if !args
                    .first()
                    .is_some_and(|a| a.trim_start().starts_with('$'))
                {
                    match args.first().and_then(|a| literal(a)) {
                        Some(key) => {
                            let names = args[1..]
                                .iter()
                                .filter(|a| !a.trim().is_empty())
                                .filter_map(|a| {
                                    a.split_once('=').map(|(n, _)| n.trim().to_string())
                                })
                                .collect();
                            out.push(Call {
                                key,
                                args: names,
                                at,
                            });
                        }
                        None => errors.push(format!("{at}: tr! key must be a string literal")),
                    }
                }
                // On inside the arguments: a `tr!` nested in this one's
                // arguments is a call of its own.
                i = start;
            }
            _ => i += 1,
        }
    }
    out
}

#[test]
fn every_message_in_both_languages_and_every_call_matches_its_text() {
    let mut errors = Vec::new();
    let mut crates: Vec<PathBuf> = fs::read_dir(crates_dir())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("Cargo.toml").exists())
        .collect();
    crates.sort();
    let mut total = 0;
    for krate in crates {
        let catalog = load_catalog(&krate.join("locales"), &mut errors);
        total += catalog.len();
        for (key, e) in &catalog {
            let at = format!("{}/locales/{}: \"{key}\"", krate.display(), e.file);
            if e.ru.trim().is_empty() || e.en.trim().is_empty() {
                errors.push(format!("{at}: empty text"));
            }
            if has_cyrillic(&e.en) {
                errors.push(format!("{at}: English text has Cyrillic: {}", e.en));
            }
            let (pr, pe) = (placeholders(&e.ru), placeholders(&e.en));
            if pr != pe {
                errors.push(format!("{at}: parameters differ — ru {pr:?}, en {pe:?}"));
            }
        }
        let mut files = Vec::new();
        rs_files(&krate.join("src"), &mut files);
        files.sort();
        let mut used = BTreeSet::new();
        for f in files {
            for c in calls_in(&f, &mut errors) {
                used.insert(c.key.clone());
                match catalog.get(&c.key) {
                    None => errors.push(format!(
                        "{}: \"{}\" is in no locales/*.json of this crate",
                        c.at, c.key
                    )),
                    Some(e) => {
                        let want = placeholders(&e.ru);
                        if want != c.args {
                            errors.push(format!(
                                "{}: \"{}\" takes {want:?}, the call passes {:?}",
                                c.at, c.key, c.args
                            ));
                        }
                    }
                }
            }
        }
        for key in catalog.keys().filter(|k| !used.contains(*k)) {
            errors.push(format!(
                "{}/locales: \"{key}\" is used nowhere",
                krate.display()
            ));
        }
    }
    assert!(
        errors.is_empty(),
        "{} i18n problem(s) in {total} message(s):\n  {}",
        errors.len(),
        errors.join("\n  ")
    );
}
