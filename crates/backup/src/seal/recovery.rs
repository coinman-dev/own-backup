//! Ключ восстановления: 256 случайных бит, записанных так, чтобы человек мог
//! их переписать на бумагу и потом ввести без ошибок.
//!
//! Это второй слот файла ключа ([`super::keypair`]): тот же закрытый ключ,
//! запечатанный не паролем, а этим числом. Показывается один раз при
//! создании задания. Без пароля, этого ключа И доступного локального ключа
//! службы восстановление невозможно.
//!
//! Запись — bech32m: только строчные буквы и цифры без похожих друг на друга
//! символов (`1`, `b`, `i`, `o` исключены), регистр не важен, и в конце
//! контрольная сумма, которая ловит опечатку, а не пускает её в Argon2id
//! молча. Префикс `obkr` говорит, что это за строка, если она найдётся в
//! чужих бумагах.

use bech32::{primitives::decode::CheckedHrpstring, Bech32m, Hrp};
use zeroize::Zeroizing;

use super::{bad, fill_random};
use own_backup_common::{tr, Result};

const HRP: &str = "obkr";

/// Ключ восстановления. Байты — секрет, строка — тоже.
pub struct RecoveryKey(Zeroizing<[u8; 32]>);

impl RecoveryKey {
    pub fn generate() -> Result<Self> {
        let mut k = Zeroizing::new([0u8; 32]);
        fill_random(&mut k[..])?;
        Ok(RecoveryKey(k))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Одной строкой: `obkr1…`, 63 символа.
    pub fn to_string_plain(&self) -> Zeroizing<String> {
        let hrp = Hrp::parse(HRP).expect("постоянный префикс");
        Zeroizing::new(
            bech32::encode::<Bech32m>(hrp, &self.0[..]).expect("32 байта — в пределах bech32"),
        )
    }

    /// Для показа и переписывания на бумагу: группами по четыре.
    pub fn to_string_grouped(&self) -> Zeroizing<String> {
        let plain = self.to_string_plain();
        let mut out = String::with_capacity(plain.len() + plain.len() / 4);
        for (i, ch) in plain.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                out.push(' ');
            }
            out.push(ch);
        }
        Zeroizing::new(out)
    }

    /// Разобрать то, что ввёл человек: пробелы и регистр не важны, опечатка
    /// ловится контрольной суммой.
    pub fn parse(typed: &str) -> Result<Self> {
        if typed.len() > 256 {
            return Err(bad(tr!("seal_recovery.too_long")));
        }
        let compact = Zeroizing::new(
            typed
                .chars()
                .filter(|c| !c.is_whitespace())
                .map(|c| c.to_ascii_lowercase())
                .collect::<String>(),
        );
        let checked = CheckedHrpstring::new::<Bech32m>(&compact)
            .map_err(|_| bad(tr!("seal_recovery.unreadable")))?;
        if checked.hrp().as_str() != HRP {
            return Err(bad(tr!("seal_recovery.not_a_recovery_key")));
        }
        let data = Zeroizing::new(checked.byte_iter().collect::<Vec<u8>>());
        if data.len() != 32 {
            return Err(bad(tr!("seal_recovery.wrong_length")));
        }
        let mut bytes = Zeroizing::new([0; 32]);
        bytes.copy_from_slice(&data);
        let key = RecoveryKey(bytes);
        if *key.to_string_plain() != *compact {
            return Err(bad(tr!("seal_recovery.not_canonical")));
        }
        Ok(key)
    }
}

impl std::fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecoveryKey(…)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_survives_the_trip_through_paper() {
        let k = RecoveryKey::generate().unwrap();
        let grouped = k.to_string_grouped();
        assert!(grouped.starts_with("obkr"), "{}", *grouped);
        // Человек введёт с пробелами и, скорее всего, заглавными.
        let typed = grouped.to_uppercase();
        let back = RecoveryKey::parse(&typed).unwrap();
        assert_eq!(back.as_bytes(), k.as_bytes());
    }

    #[test]
    fn the_plain_form_is_short_enough_to_write_down() {
        let k = RecoveryKey::generate().unwrap();
        let s = k.to_string_plain();
        assert_eq!(s.len(), 63, "{}", *s);
        assert!(s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn a_typo_is_caught_by_the_checksum_not_fed_to_the_kdf() {
        let k = RecoveryKey::generate().unwrap();
        let mut s = k.to_string_plain().to_string();
        // Портим один символ данных на заведомо допустимый другой.
        let idx = 10;
        let c = s.as_bytes()[idx];
        let replacement = if c == b'q' { b'p' } else { b'q' };
        s.replace_range(idx..idx + 1, std::str::from_utf8(&[replacement]).unwrap());
        assert!(RecoveryKey::parse(&s).is_err());
    }

    #[test]
    fn a_foreign_bech32_string_is_named_as_foreign() {
        let hrp = Hrp::parse("age").unwrap();
        let foreign = bech32::encode::<Bech32m>(hrp, &[9u8; 32]).unwrap();
        let err = RecoveryKey::parse(&foreign).unwrap_err().to_string();
        assert!(err.contains("не ключ восстановления"), "{err}");
    }

    #[test]
    fn it_does_not_print_itself() {
        let k = RecoveryKey::generate().unwrap();
        assert_eq!(format!("{k:?}"), "RecoveryKey(…)");
    }

    #[test]
    fn checksum_variant_and_size_are_strict() {
        let hrp = Hrp::parse(HRP).unwrap();
        let legacy = bech32::encode::<bech32::Bech32>(hrp, &[7; 32]).unwrap();
        assert!(RecoveryKey::parse(&legacy).is_err());
        for len in [0, 1, 31, 33] {
            let s = bech32::encode::<Bech32m>(hrp, &vec![7; len]).unwrap();
            assert!(RecoveryKey::parse(&s).is_err());
        }
        assert!(RecoveryKey::parse(&" ".repeat(257)).is_err());
    }
}
