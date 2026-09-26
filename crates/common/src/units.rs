//! Byte formatting for user-facing text.

/// Binary units, matching what the interface shows. 1 KiB is 1024 bytes — the
/// GUI's `formatBytes` divides by 1024, so labelling the result "KB" would be
/// wrong in the way that makes a 130.28 GiB partition read as "130 GB" and a
/// user wonder where the rest went.
pub fn human_bytes(bytes: u64) -> String {
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNIT_COUNT {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} {}", unit(0))
    } else {
        format!("{v:.2} {}", unit(i))
    }
}

/// B, KiB … PiB.
const UNIT_COUNT: usize = 6;

/// The name of the `i`-th binary unit, in the window's language.
fn unit(i: usize) -> std::borrow::Cow<'static, str> {
    match i {
        0 => crate::tr!("units.b"),
        1 => crate::tr!("units.kib"),
        2 => crate::tr!("units.mib"),
        3 => crate::tr!("units.gib"),
        4 => crate::tr!("units.tib"),
        _ => crate::tr!("units.pib"),
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
