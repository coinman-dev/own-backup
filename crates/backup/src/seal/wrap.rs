//! Заворачивание ключа цепочки открытым ключом задания.
//!
//! Схема — та же, что у `age` для получателя X25519, и по той же причине: она
//! маленькая, разобранная и делает ровно одно.
//!
//! ```text
//! эфемерная пара (e, E) — новая на каждую копию
//! общий секрет   = X25519(e, P)            P — открытый ключ задания
//! соль           = E ‖ P
//! ключ обёртки   = HKDF-SHA256(общий, соль, info = "own-backup/v1/wrap")
//! обёртка        = AES-256-GCM(ключ обёртки, nonce = 0, ключ цепочки, aad = отпечаток P)
//! в образ        = E ‖ обёртка              32 + 48 байт
//! ```
//!
//! Nonce нулевой намеренно: ключ обёртки уникален на каждую эфемерную пару,
//! и повтора nonce под одним ключом не бывает. Отпечаток в `aad` привязывает
//! обёртку к получателю: под другим ключом тег не сойдётся.
//!
//! Развернуть может только закрытый ключ задания. На машине, где идёт копия
//! по расписанию, его нет в открытом виде — только запечатанный
//! ([`super::keypair`]), и это вся суть §7.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use super::keypair::{JobPrivateKey, JobPublicKey};
use super::{bad, fill_random, ChainKey, SCHEME};
use own_backup_common::Result;

/// Завёрнутый ключ цепочки, как он лежит в открытой части манифеста.
#[derive(Clone, PartialEq, Eq)]
pub struct WrappedChainKey {
    /// Эфемерный открытый ключ E.
    pub ephemeral: [u8; 32],
    /// 32 байта ключа + 16 байт тега.
    pub sealed: [u8; 48],
}

impl WrappedChainKey {
    pub const LEN: usize = 80;

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut out = [0u8; Self::LEN];
        out[..32].copy_from_slice(&self.ephemeral);
        out[32..].copy_from_slice(&self.sealed);
        out
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        if b.len() != Self::LEN {
            return Err(bad(format!(
                "завёрнутый ключ цепочки должен быть {} байт, а не {}",
                Self::LEN,
                b.len()
            )));
        }
        let mut w = WrappedChainKey { ephemeral: [0; 32], sealed: [0; 48] };
        w.ephemeral.copy_from_slice(&b[..32]);
        w.sealed.copy_from_slice(&b[32..]);
        Ok(w)
    }
}

impl std::fmt::Debug for WrappedChainKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WrappedChainKey(…)")
    }
}

/// Завернуть ключ цепочки для задания. Нужен только открытый ключ.
pub fn wrap_chain_key(recipient: &JobPublicKey, chain: &ChainKey) -> Result<WrappedChainKey> {
    let mut raw = Zeroizing::new([0u8; 32]);
    fill_random(&mut raw[..])?;
    let ephemeral = StaticSecret::from(*raw);
    let ephemeral_public = PublicKey::from(&ephemeral);

    let shared = ephemeral.diffie_hellman(&recipient.dalek());
    let wrap_key = wrap_key(shared.as_bytes(), ephemeral_public.as_bytes(), recipient)?;

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&wrap_key[..]));
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&[0u8; 12]),
            Payload { msg: &chain.as_bytes()[..], aad: recipient.fingerprint().as_bytes() },
        )
        .map_err(|_| bad("AES-GCM не завернул ключ цепочки"))?;
    let mut w = WrappedChainKey { ephemeral: *ephemeral_public.as_bytes(), sealed: [0; 48] };
    w.sealed.copy_from_slice(&sealed);
    Ok(w)
}

/// Развернуть ключ цепочки закрытым ключом задания.
pub fn unwrap_chain_key(private: &JobPrivateKey, wrapped: &WrappedChainKey) -> Result<ChainKey> {
    let recipient = private.public();
    let shared = private.dalek().diffie_hellman(&PublicKey::from(wrapped.ephemeral));
    let wrap_key = wrap_key(shared.as_bytes(), &wrapped.ephemeral, &recipient)?;

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&wrap_key[..]));
    let opened = cipher
        .decrypt(
            Nonce::from_slice(&[0u8; 12]),
            Payload { msg: &wrapped.sealed[..], aad: recipient.fingerprint().as_bytes() },
        )
        // При целых байтах тег не сходится ровно в одном случае: копия
        // завёрнута для другого задания.
        .map_err(|_| bad("эта копия завёрнута для другого ключа — не того задания"))?;
    let opened = Zeroizing::new(opened);
    let arr: [u8; 32] = opened
        .as_slice()
        .try_into()
        .map_err(|_| bad("в обёртке не 32 байта ключа цепочки — образ повреждён"))?;
    Ok(ChainKey::from_bytes(arr))
}

fn wrap_key(shared: &[u8; 32], ephemeral_public: &[u8; 32], recipient: &JobPublicKey) -> Result<Zeroizing<[u8; 32]>> {
    // Точка низкого порядка на чужом открытом ключе даёт нулевой общий
    // секрет — то есть ключ обёртки, известный всем. Такой ключ задания
    // не принимаем.
    if shared.iter().all(|&b| b == 0) {
        return Err(bad("открытый ключ задания негоден: общий секрет вырожден"));
    }
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(ephemeral_public);
    salt[32..].copy_from_slice(recipient.as_bytes());
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(&salt), shared);
    let mut out = Zeroizing::new([0u8; 32]);
    let info = format!("{SCHEME}/wrap");
    hk.expand(info.as_bytes(), &mut out[..]).expect("32 байта — в пределах HKDF");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::keypair::JobKeyPair;

    #[test]
    fn the_chain_key_comes_back_only_with_the_private_key() {
        let pair = JobKeyPair::generate().unwrap();
        let chain = ChainKey::generate().unwrap();
        let wrapped = wrap_chain_key(&pair.public, &chain).unwrap();
        let back = unwrap_chain_key(&pair.private, &wrapped).unwrap();
        assert_eq!(back.as_bytes(), chain.as_bytes());
    }

    #[test]
    fn another_jobs_key_is_told_apart_from_a_broken_file() {
        let pair = JobKeyPair::generate().unwrap();
        let other = JobKeyPair::generate().unwrap();
        let chain = ChainKey::generate().unwrap();
        let wrapped = wrap_chain_key(&pair.public, &chain).unwrap();
        let err = unwrap_chain_key(&other.private, &wrapped).unwrap_err().to_string();
        assert!(err.contains("другого ключа"), "{err}");
    }

    #[test]
    fn every_wrap_is_different_even_for_the_same_key() {
        // Эфемерная пара новая на каждую копию: по двум образам не видно, что
        // у них один ключ цепочки.
        let pair = JobKeyPair::generate().unwrap();
        let chain = ChainKey::generate().unwrap();
        let a = wrap_chain_key(&pair.public, &chain).unwrap();
        let b = wrap_chain_key(&pair.public, &chain).unwrap();
        assert_ne!(a.ephemeral, b.ephemeral);
        assert_ne!(a.sealed, b.sealed);
        assert_eq!(unwrap_chain_key(&pair.private, &a).unwrap().as_bytes(), chain.as_bytes());
        assert_eq!(unwrap_chain_key(&pair.private, &b).unwrap().as_bytes(), chain.as_bytes());
    }

    #[test]
    fn the_wrapped_form_is_eighty_bytes_and_round_trips() {
        let pair = JobKeyPair::generate().unwrap();
        let chain = ChainKey::generate().unwrap();
        let w = wrap_chain_key(&pair.public, &chain).unwrap();
        let bytes = w.to_bytes();
        assert_eq!(bytes.len(), WrappedChainKey::LEN);
        assert_eq!(WrappedChainKey::from_bytes(&bytes).unwrap(), w);
        assert!(WrappedChainKey::from_bytes(&bytes[..79]).is_err());
    }

    #[test]
    fn a_flipped_byte_in_the_wrap_is_refused() {
        let pair = JobKeyPair::generate().unwrap();
        let chain = ChainKey::generate().unwrap();
        let mut w = wrap_chain_key(&pair.public, &chain).unwrap();
        w.sealed[5] ^= 0x80;
        assert!(unwrap_chain_key(&pair.private, &w).is_err());
    }

    /// RFC 7748 §6.1: известные ключи Алисы и Боба дают известный общий
    /// секрет. Это проверяет, что x25519-dalek подключён и считает то, что
    /// должен, — а не нашу схему поверх него.
    #[test]
    fn x25519_matches_rfc_7748() {
        fn unhex(s: &str) -> [u8; 32] {
            let mut out = [0u8; 32];
            for (i, b) in out.iter_mut().enumerate() {
                *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
            }
            out
        }
        let alice = StaticSecret::from(unhex(
            "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a",
        ));
        let bob_public = PublicKey::from(unhex(
            "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f",
        ));
        let shared = alice.diffie_hellman(&bob_public);
        assert_eq!(
            shared.as_bytes(),
            &unhex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742")
        );
    }
}
