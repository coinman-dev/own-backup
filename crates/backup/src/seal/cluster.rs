//! Шифрование кластеров: AES-256-XTS, твик — гостевой индекс кластера.
//!
//! Почему именно так, коротко (полностью — `.ai/PLAN.md` §7.2):
//!
//! * **Твик по ГОСТЕВОМУ смещению, не по месту в файле.** При объединении
//!   цепочки кластер переезжает из инкремента в базу, но гостевое место у него
//!   то же — значит, и шифротекст тот же, и его можно перенести байт в байт,
//!   не зная ключа. LUKS внутри qcow2 берёт физическое смещение, и у него это
//!   невозможно.
//! * **Длина не меняется.** Кластер шифруется после zstd, каким получился —
//!   XTS с кражей шифротекста берёт любую длину от 16 байт. Записи L2 со
//!   сжатыми кластерами хранят длину в байтах и остаются как были.
//! * **Меньше 16 байт XTS не умеет.** Такой кластер писатель обязан хранить
//!   несжатым — [`MIN_LEN`] и [`fits`] для этого.
//! * **Целостности здесь нет — и не должно быть.** GCM с nonce из гостевого
//!   индекса развалился бы при первом же инкременте, где тот же кластер
//!   переписан другим содержимым. Целостность даёт манифест: суммы по
//!   шифротексту (без ключа, от гниения диска) и по открытым данным (с ключом,
//!   от подмены).

use aes::cipher::KeyInit;
use aes::Aes256;
use xts_mode::Xts128;

use super::{bad, ChainKey};
use own_backup_common::Result;

/// Короче этого XTS не шифрует. Кластер, ужавшийся сильнее, хранится несжатым.
pub const MIN_LEN: usize = 16;

/// Годится ли буфер такой длины для шифрования как есть.
pub fn fits(len: usize) -> bool {
    len >= MIN_LEN
}

/// Шифр одной цепочки. Дёшево создаётся, ключи внутри — производные от
/// мастер-ключа, сам мастер сюда не попадает.
pub struct ClusterCipher {
    xts: Xts128<Aes256>,
}

impl ClusterCipher {
    pub fn new(chain: &ChainKey) -> Self {
        // XTS-256 — это два ключа AES-256: один шифрует данные, другой твик.
        let k1 = chain.derive("xts/1");
        let k2 = chain.derive("xts/2");
        let xts = Xts128::new(Aes256::new((&k1[..]).into()), Aes256::new((&k2[..]).into()));
        ClusterCipher { xts }
    }

    /// Зашифровать кластер на месте. `guest_index` — номер кластера в гостевом
    /// адресном пространстве (смещение ÷ размер кластера).
    pub fn encrypt(&self, guest_index: u64, buf: &mut [u8]) -> Result<()> {
        if !fits(buf.len()) {
            return Err(bad(format!(
                "кластер на {} байт короче {MIN_LEN} — такой хранится несжатым",
                buf.len()
            )));
        }
        self.xts.encrypt_sector(buf, tweak(guest_index));
        Ok(())
    }

    /// Расшифровать кластер на месте под тем же гостевым индексом.
    pub fn decrypt(&self, guest_index: u64, buf: &mut [u8]) -> Result<()> {
        if !fits(buf.len()) {
            return Err(bad(format!(
                "кластер на {} байт короче {MIN_LEN} — зашифрованным он быть не может",
                buf.len()
            )));
        }
        self.xts.decrypt_sector(buf, tweak(guest_index));
        Ok(())
    }
}

/// Твик как в `plain64`: индекс little-endian в первых восьми байтах, остальное
/// нули. Та же условность, что у LUKS и у нашего `decrypt.rs`.
fn tweak(guest_index: u64) -> [u8; 16] {
    let mut t = [0u8; 16];
    t[..8].copy_from_slice(&guest_index.to_le_bytes());
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> ClusterCipher {
        ClusterCipher::new(&ChainKey::from_bytes([0x42; 32]))
    }

    #[test]
    fn a_cluster_comes_back_whole() {
        let c = cipher();
        let plain: Vec<u8> = (0..65536u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut buf = plain.clone();
        c.encrypt(1234, &mut buf).unwrap();
        assert_ne!(buf, plain, "шифротекст отличается от открытого");
        c.decrypt(1234, &mut buf).unwrap();
        assert_eq!(buf, plain);
    }

    /// Ради этого выбран XTS с гостевым твиком: объединение переносит кластер
    /// на то же гостевое место, и шифротекст должен совпасть байт в байт.
    #[test]
    fn the_same_cluster_at_the_same_guest_place_encrypts_identically() {
        let c = cipher();
        let plain = vec![0xA5u8; 4096];
        let mut a = plain.clone();
        let mut b = plain.clone();
        c.encrypt(77, &mut a).unwrap();
        c.encrypt(77, &mut b).unwrap();
        assert_eq!(a, b, "перенос как есть возможен только при равенстве");
    }

    /// А на другом месте — другой шифротекст: одинаковые блоки диска не
    /// выдают друг друга.
    #[test]
    fn a_different_guest_place_gives_a_different_ciphertext() {
        let c = cipher();
        let plain = vec![0xA5u8; 4096];
        let mut a = plain.clone();
        let mut b = plain.clone();
        c.encrypt(77, &mut a).unwrap();
        c.encrypt(78, &mut b).unwrap();
        assert_ne!(a, b);
    }

    /// Перепутанный индекс при чтении даёт мусор, а не открытые данные —
    /// значит, читатель обязан знать гостевое место, и это защищает от
    /// подстановки кластера не туда.
    #[test]
    fn decrypting_under_the_wrong_index_yields_rubbish() {
        let c = cipher();
        let plain = vec![0x11u8; 4096];
        let mut buf = plain.clone();
        c.encrypt(5, &mut buf).unwrap();
        c.decrypt(6, &mut buf).unwrap();
        assert_ne!(buf, plain);
    }

    /// Сжатый кластер имеет произвольную длину — XTS с кражей шифротекста
    /// обязан брать её без выравнивания и без изменения длины.
    #[test]
    fn odd_lengths_keep_their_length_and_round_trip() {
        let c = cipher();
        for len in [16usize, 17, 31, 33, 1000, 65535] {
            let plain: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
            let mut buf = plain.clone();
            c.encrypt(9, &mut buf).unwrap();
            assert_eq!(buf.len(), len, "длина {len} не должна меняться");
            c.decrypt(9, &mut buf).unwrap();
            assert_eq!(buf, plain, "длина {len}");
        }
    }

    #[test]
    fn shorter_than_a_block_is_refused_rather_than_padded() {
        let c = cipher();
        let mut buf = vec![1u8; 15];
        assert!(c.encrypt(0, &mut buf).is_err());
        assert!(c.decrypt(0, &mut buf).is_err());
        assert!(!fits(15));
        assert!(fits(16));
    }

    #[test]
    fn a_different_chain_key_cannot_read_it() {
        let a = ClusterCipher::new(&ChainKey::from_bytes([1u8; 32]));
        let b = ClusterCipher::new(&ChainKey::from_bytes([2u8; 32]));
        let plain = vec![0x33u8; 512];
        let mut buf = plain.clone();
        a.encrypt(3, &mut buf).unwrap();
        b.decrypt(3, &mut buf).unwrap();
        assert_ne!(buf, plain);
    }
}
