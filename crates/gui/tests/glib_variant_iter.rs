//! Optimised regression for the GTK3 glib compatibility backport.
#![cfg(target_os = "linux")]
#[cfg(test)]
mod tests {
    use glib::variant::ToVariant;

    #[test]
    fn string_variant_iterator_preserves_values_from_both_ends() {
        let values = ["first", "", "Привет", "日本語", "last"];
        let variant = values.to_variant();
        assert_eq!(
            variant.array_iter_str().unwrap().collect::<Vec<_>>(),
            values
        );
        assert_eq!(
            variant.array_iter_str().unwrap().rev().collect::<Vec<_>>(),
            values.iter().rev().copied().collect::<Vec<_>>()
        );
        let mut iter = variant.array_iter_str().unwrap();
        assert_eq!(iter.next(), Some("first"));
        assert_eq!(iter.next_back(), Some("last"));
        assert_eq!(iter.nth(1), Some("Привет"));
        assert_eq!(iter.nth_back(0), Some("日本語"));
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next_back(), None);
        assert_eq!(variant.array_iter_str().unwrap().last(), Some("last"));
        let empty: [&str; 0] = [];
        assert_eq!(empty.to_variant().array_iter_str().unwrap().next(), None);
    }
}
