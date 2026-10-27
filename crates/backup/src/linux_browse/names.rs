//! Linux file names as Windows can hold them, and back.
//!
//! A Linux name is any bytes but `/` and NUL. A Windows name is UTF-16 without
//! `\ / : * ? " < > |`, control characters, or a trailing dot or space — and
//! Total Commander, which shows these names and copies the files under them,
//! needs every one of them to be both displayable and creatable on NTFS.
//!
//! The characters Windows refuses are moved into the Private Use Area, where
//! Cygwin and WSL put them too (U+F000 + the character): `a:b` is shown as
//! `a\u{F03A}b` and copied under that name, the way WSL's own files look from
//! Windows. Bytes that are not UTF-8 at all — a name written by an old system
//! in KOI8-R — become U+F780..U+F7FF, one per byte. Both are reversed exactly
//! when a path comes back, so every name opens; a real name that happens to
//! contain those private characters is the only one that would not, and none
//! does in practice.

/// Characters a Windows name cannot hold, besides the control characters.
const FORBIDDEN: &[char] = &['\\', ':', '*', '?', '"', '<', '>', '|'];
const MAPPED_BASE: u32 = 0xF000;
const RAW_BYTE_BASE: u32 = 0xF700;

/// The name Windows is given for these bytes.
pub fn to_display(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    for chunk in raw.utf8_chunks() {
        for c in chunk.valid().chars() {
            out.push(map_char(c));
        }
        for &b in chunk.invalid() {
            // Invalid sequences are made of bytes >= 0x80 only.
            out.push(char::from_u32(RAW_BYTE_BASE + u32::from(b)).unwrap_or('\u{FFFD}'));
        }
    }
    // Windows drops a trailing dot or space when it creates a file.
    if let Some(last) = out.pop() {
        out.push(if last == '.' || last == ' ' {
            char::from_u32(MAPPED_BASE + last as u32).unwrap_or(last)
        } else {
            last
        });
    }
    out
}

fn map_char(c: char) -> char {
    if FORBIDDEN.contains(&c) || (c as u32) < 0x20 {
        char::from_u32(MAPPED_BASE + c as u32).unwrap_or(c)
    } else {
        c
    }
}

/// The bytes a displayed name stands for.
pub fn from_display(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len());
    for c in name.chars() {
        let u = c as u32;
        if (MAPPED_BASE..MAPPED_BASE + 0x80).contains(&u) {
            out.push((u - MAPPED_BASE) as u8);
        } else if (RAW_BYTE_BASE + 0x80..RAW_BYTE_BASE + 0x100).contains(&u) {
            out.push((u - RAW_BYTE_BASE) as u8);
        } else {
            let mut b = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
        }
    }
    out
}

/// The absolute Linux path for a path of displayed names, separated by `\`
/// (as Total Commander passes them) or `/`. Empty components, `.` and `..`
/// are dropped: a path never climbs out of the filesystem it is in.
pub fn path_from_display(path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for part in path.split(['\\', '/']) {
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        out.push(b'/');
        out.extend_from_slice(&from_display(part));
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_names_are_left_alone() {
        for n in ["readme.txt", "Документы", "a b", ".bashrc", "日本語"] {
            assert_eq!(to_display(n.as_bytes()), n);
            assert_eq!(from_display(n), n.as_bytes());
        }
    }

    /// What NTFS refuses goes where WSL and Cygwin put it, and comes back.
    #[test]
    fn characters_windows_refuses_are_moved_and_restored() {
        let raw = b"a:b*c?d\"e<f>g|h\\i\x01";
        let shown = to_display(raw);
        assert!(!shown.chars().any(|c| FORBIDDEN.contains(&c) || (c as u32) < 0x20), "{shown:?}");
        assert!(shown.starts_with("a\u{F03A}b"), "{shown:?}");
        assert_eq!(from_display(&shown), raw);
    }

    #[test]
    fn a_trailing_dot_or_space_survives_windows() {
        for raw in [&b"name."[..], b"name ", b"..."] {
            let shown = to_display(raw);
            assert!(!shown.ends_with('.') && !shown.ends_with(' '), "{shown:?}");
            assert_eq!(from_display(&shown), raw);
        }
    }

    /// A name that is not UTF-8 (KOI8-R "привет") still opens.
    #[test]
    fn bytes_that_are_not_utf8_round_trip() {
        let raw = b"\xd0\xd2\xc9\xd7\xc5\xd4.txt";
        let shown = to_display(raw);
        assert!(shown.ends_with(".txt"));
        assert_eq!(from_display(&shown), raw);
    }

    #[test]
    fn paths_are_built_from_displayed_names_and_never_climb_out() {
        assert_eq!(
            path_from_display("\\home\\user\\a\u{F03A}b"),
            b"/home/user/a:b"
        );
        assert_eq!(path_from_display(""), b"/");
        assert_eq!(path_from_display(r"\"), b"/");
        assert_eq!(path_from_display(r"\..\..\etc\.\passwd"), b"/etc/passwd");
        assert_eq!(path_from_display("/usr/bin"), b"/usr/bin");
    }
}
