//! Portable wrapping of a chain key for a job's X25519 public key.
//!
//! Each wrap uses a fresh ephemeral key, HKDF-SHA256 and AES-256-GCM. The
//! complete recipient key, chain identity and version are authenticated.
//! The service needs its protected PRIVATE job key to reopen an existing chain.
//! This is our versioned envelope, not an interoperable age file.

use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use super::keypair::{JobPrivateKey, JobPublicKey};
use super::{aead_open, aead_seal, bad, fill_random, ChainKey, SCHEME};
use own_backup_common::Result;

const PREFIX: &[u8; 6] = b"OBKW\0\x02";

#[derive(Clone, PartialEq, Eq)]
pub struct WrappedChainKey {
    pub chain_id: [u8; 16],
    pub ephemeral: [u8; 32],
    pub sealed: [u8; 48],
}

impl WrappedChainKey {
    pub const LEN: usize = 102;

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut out = [0; Self::LEN];
        out[..6].copy_from_slice(PREFIX);
        out[6..22].copy_from_slice(&self.chain_id);
        out[22..54].copy_from_slice(&self.ephemeral);
        out[54..].copy_from_slice(&self.sealed);
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != Self::LEN || &bytes[..6] != PREFIX {
            return Err(bad("invalid wrapped chain key format/version/length"));
        }
        let mut out = Self {
            chain_id: [0; 16],
            ephemeral: [0; 32],
            sealed: [0; 48],
        };
        out.chain_id.copy_from_slice(&bytes[6..22]);
        out.ephemeral.copy_from_slice(&bytes[22..54]);
        out.sealed.copy_from_slice(&bytes[54..]);
        Ok(out)
    }

    fn aad(&self, recipient: &JobPublicKey) -> Vec<u8> {
        let mut aad = self.to_bytes()[..54].to_vec();
        aad.extend_from_slice(recipient.as_bytes());
        aad
    }
}

impl std::fmt::Debug for WrappedChainKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WrappedChainKey(…)")
    }
}

pub fn wrap_chain_key(
    recipient: &JobPublicKey,
    chain_id: &[u8; 16],
    chain: &ChainKey,
) -> Result<WrappedChainKey> {
    let mut raw = Zeroizing::new([0; 32]);
    fill_random(&mut raw[..])?;
    let ephemeral = StaticSecret::from(*raw);
    let ephemeral_public = PublicKey::from(&ephemeral);
    let shared = ephemeral.diffie_hellman(&recipient.dalek());
    if !shared.was_contributory() {
        return Err(bad(
            "invalid X25519 recipient: non-contributory shared secret",
        ));
    }
    let key = wrap_key(shared.as_bytes(), ephemeral_public.as_bytes(), recipient);
    let mut wrapped = WrappedChainKey {
        chain_id: *chain_id,
        ephemeral: *ephemeral_public.as_bytes(),
        sealed: [0; 48],
    };
    // This key is used for exactly one encryption, with a fresh ephemeral
    // secret every time. A fixed nonce is safe for this single-use wrap key.
    let sealed = aead_seal(&key, &[0; 12], &wrapped.aad(recipient), chain.as_bytes())?;
    wrapped.sealed.copy_from_slice(&sealed);
    Ok(wrapped)
}

pub fn unwrap_chain_key(
    private: &JobPrivateKey,
    expected_chain: &[u8; 16],
    wrapped: &WrappedChainKey,
) -> Result<ChainKey> {
    if &wrapped.chain_id != expected_chain {
        return Err(bad("wrapped key belongs to another chain"));
    }
    let recipient = private.public();
    let shared = private
        .dalek()
        .diffie_hellman(&PublicKey::from(wrapped.ephemeral));
    if !shared.was_contributory() {
        return Err(bad(
            "invalid X25519 ephemeral key: non-contributory shared secret",
        ));
    }
    let key = wrap_key(shared.as_bytes(), &wrapped.ephemeral, &recipient);
    let opened = aead_open(&key, &[0; 12], &wrapped.aad(&recipient), &wrapped.sealed)?;
    let mut bytes = [0; 32];
    bytes.copy_from_slice(&opened);
    Ok(ChainKey::from_bytes(bytes))
}

fn wrap_key(
    shared: &[u8; 32],
    ephemeral_public: &[u8; 32],
    recipient: &JobPublicKey,
) -> Zeroizing<[u8; 32]> {
    let mut salt = [0; 64];
    salt[..32].copy_from_slice(ephemeral_public);
    salt[32..].copy_from_slice(recipient.as_bytes());
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(&salt), shared);
    let mut out = Zeroizing::new([0; 32]);
    hk.expand(format!("{SCHEME}/wrap").as_bytes(), &mut out[..])
        .expect("fixed HKDF output size");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::keypair::JobKeyPair;

    #[test]
    fn the_chain_key_comes_back_only_with_the_private_key() {
        let pair = JobKeyPair::generate().unwrap();
        let chain = ChainKey::generate().unwrap();
        let wrapped = wrap_chain_key(&pair.public, &[7; 16], &chain).unwrap();
        let back = unwrap_chain_key(&pair.private, &[7; 16], &wrapped).unwrap();
        assert_eq!(back.as_bytes(), chain.as_bytes());
    }

    #[test]
    fn wrong_key_or_damaged_file_is_rejected() {
        let pair = JobKeyPair::generate().unwrap();
        let other = JobKeyPair::generate().unwrap();
        let chain = ChainKey::generate().unwrap();
        let wrapped = wrap_chain_key(&pair.public, &[7; 16], &chain).unwrap();
        let err = unwrap_chain_key(&other.private, &[7; 16], &wrapped)
            .unwrap_err()
            .to_string();
        assert!(err.contains("authentication failed"), "{err}");
    }

    #[test]
    fn every_wrap_is_different_even_for_the_same_key() {
        // Эфемерная пара новая на каждую копию: по двум образам не видно, что
        // у них один ключ цепочки.
        let pair = JobKeyPair::generate().unwrap();
        let chain = ChainKey::generate().unwrap();
        let a = wrap_chain_key(&pair.public, &[7; 16], &chain).unwrap();
        let b = wrap_chain_key(&pair.public, &[7; 16], &chain).unwrap();
        assert_ne!(a.ephemeral, b.ephemeral);
        assert_ne!(a.sealed, b.sealed);
        assert_eq!(
            unwrap_chain_key(&pair.private, &[7; 16], &a)
                .unwrap()
                .as_bytes(),
            chain.as_bytes()
        );
        assert_eq!(
            unwrap_chain_key(&pair.private, &[7; 16], &b)
                .unwrap()
                .as_bytes(),
            chain.as_bytes()
        );
    }

    #[test]
    fn the_versioned_wrapped_form_round_trips() {
        let pair = JobKeyPair::generate().unwrap();
        let chain = ChainKey::generate().unwrap();
        let w = wrap_chain_key(&pair.public, &[7; 16], &chain).unwrap();
        let bytes = w.to_bytes();
        assert_eq!(bytes.len(), WrappedChainKey::LEN);
        assert_eq!(WrappedChainKey::from_bytes(&bytes).unwrap(), w);
        assert!(WrappedChainKey::from_bytes(&bytes[..WrappedChainKey::LEN - 1]).is_err());
    }

    #[test]
    fn a_flipped_byte_in_the_wrap_is_refused() {
        let pair = JobKeyPair::generate().unwrap();
        let chain = ChainKey::generate().unwrap();
        let mut w = wrap_chain_key(&pair.public, &[7; 16], &chain).unwrap();
        w.sealed[5] ^= 0x80;
        assert!(unwrap_chain_key(&pair.private, &[7; 16], &w).is_err());
    }

    #[test]
    fn chain_binding_versions_and_low_order_keys_are_checked() {
        let pair = JobKeyPair::generate().unwrap();
        let chain = ChainKey::generate().unwrap();
        let wrapped = wrap_chain_key(&pair.public, &[7; 16], &chain).unwrap();
        assert!(unwrap_chain_key(&pair.private, &[8; 16], &wrapped).is_err());
        let mut renamed = wrapped.clone();
        renamed.chain_id = [8; 16];
        assert!(unwrap_chain_key(&pair.private, &[8; 16], &renamed).is_err());
        for value in [0, 1] {
            let mut low = [0; 32];
            low[0] = value;
            assert!(wrap_chain_key(&JobPublicKey::from_bytes(low), &[7; 16], &chain).is_err());
            let mut altered = wrapped.clone();
            altered.ephemeral = low;
            assert!(unwrap_chain_key(&pair.private, &[7; 16], &altered).is_err());
        }
        let bytes = wrapped.to_bytes();
        for len in 0..bytes.len() {
            assert!(WrappedChainKey::from_bytes(&bytes[..len]).is_err());
        }
        let mut unknown = bytes;
        unknown[5] = 1;
        assert!(WrappedChainKey::from_bytes(&unknown).is_err());
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
