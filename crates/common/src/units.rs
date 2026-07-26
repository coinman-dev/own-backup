//! Byte formatting for user-facing text.

/// Binary units, matching what the interface shows. 1 KiB is 1024 bytes — the
/// GUI's `formatBytes` divides by 1024, so labelling the result "KB" would be
/// wrong in the way that makes a 130.28 GiB partition read as "130 GB" and a
/// user wonder where the rest went.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["Б", "КиБ", "МиБ", "ГиБ", "ТиБ", "ПиБ"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_sizes_stay_in_bytes() {
        assert_eq!(human_bytes(0), "0 Б");
        assert_eq!(human_bytes(1023), "1023 Б");
    }

    #[test]
    fn units_are_binary() {
        assert_eq!(human_bytes(1024), "1.00 КиБ");
        assert_eq!(human_bytes(1024 * 1024), "1.00 МиБ");
        assert_eq!(human_bytes(140_000_000_000), "130.39 ГиБ");
    }
}
