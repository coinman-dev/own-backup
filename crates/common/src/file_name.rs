//! Turning a job's name into the stem of its image file.
//!
//! The rule is the user's, and it is narrower than "replace everything that is
//! not a letter or a digit":
//!
//! * a SPACE becomes `_` — it is the one character that has to be replaced
//!   rather than dropped, because dropping it would glue words together;
//! * a character Windows forbids in a file name is DROPPED, not replaced —
//!   `Win C:` is meant to read `Win_C`, and the old rule made it `Win_C_`,
//!   which then met the separator and came out `Win_C__VE7F57C39_full_…`;
//! * everything else is kept as typed. Cyrillic, dashes, dots, brackets are
//!   all perfectly good in a file name, and a person who typed them meant them.

/// Characters Windows refuses in a file name (plus the path separators).
const FORBIDDEN: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Device names Windows reserves whatever the extension: `CON.vhdx` is not a
/// file. A name that collides gets an underscore so it stays recognisable.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The file-name stem for `name`, or `fallback` when nothing usable is left.
pub fn safe_file_stem(name: &str, fallback: &str) -> String {
    // Trim BEFORE mapping: afterwards a leading space is already an underscore
    // and no longer looks like the padding it was.
    let name = name.trim_matches(|c: char| c.is_whitespace() || c == '.');
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c == ' ' {
            out.push('_');
        } else if c.is_control() || FORBIDDEN.contains(&c) {
            // dropped on purpose — see the module docs
        } else {
            out.push(c);
        }
    }
    // Windows cannot store a name ending in a dot or a space; a leading dot
    // would make it hidden on the other half of the world.
    let out = out.trim_matches(|c: char| c == '.' || c == ' ').to_string();
    if out.is_empty() {
        return fallback.to_string();
    }
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&out)) {
        return format!("{out}_");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case that started this: `Win C:` produced `Win_C_`, so the file came
    /// out `Win_C__VE7F57C39_full_…` with a double underscore.
    #[test]
    fn space_becomes_underscore_and_forbidden_is_dropped() {
        assert_eq!(safe_file_stem("Win C:", "backup"), "Win_C");
        assert_eq!(safe_file_stem("C:", "backup"), "C");
        assert_eq!(safe_file_stem("a/b\\c", "backup"), "abc");
    }

    /// Everything a file name may legally hold is kept as typed.
    #[test]
    fn legal_characters_survive() {
        assert_eq!(safe_file_stem("Копия-1.диск (важное)", "backup"), "Копия-1.диск_(важное)");
        assert_eq!(safe_file_stem("2026-08-22", "backup"), "2026-08-22");
    }

    #[test]
    fn nothing_usable_falls_back() {
        assert_eq!(safe_file_stem("", "backup"), "backup");
        assert_eq!(safe_file_stem(":::", "backup"), "backup");
        assert_eq!(safe_file_stem("   ", "backup"), "backup");
    }

    /// A name Windows reserves is not a file, whatever the extension.
    #[test]
    fn reserved_device_names_get_out_of_the_way() {
        assert_eq!(safe_file_stem("con", "backup"), "con_");
        assert_eq!(safe_file_stem("LPT1", "backup"), "LPT1_");
    }

    /// A trailing dot or space cannot be stored on Windows.
    #[test]
    fn trailing_dots_and_spaces_are_trimmed() {
        assert_eq!(safe_file_stem("копия.", "backup"), "копия");
        assert_eq!(safe_file_stem(" копия ", "backup"), "копия");
    }
}
