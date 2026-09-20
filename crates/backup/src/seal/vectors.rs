//! Independent known-answer vectors, not just encrypt/decrypt round trips.

use super::{aead_open, aead_seal};

fn hex(input: &str) -> Vec<u8> {
    let (pairs, remainder) = input.as_bytes().as_chunks::<2>();
    assert!(remainder.is_empty());
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

/// NIST CAVS gcmEncryptExtIV256.rsp (128-bit plaintext, 96-bit IV, 128-bit tag).
/// https://csrc.nist.gov/projects/cryptographic-algorithm-validation-program/cavp-testing-block-cipher-modes
#[test]
fn aes256_gcm_matches_nist_cavs() {
    let key: [u8; 32] = hex("31bdadd96698c204aa9ce1448ea94ae1fb4a9a0b3c9d773b51bb1822666b8f22")
        .try_into()
        .unwrap();
    let nonce: [u8; 12] = hex("0d18e06c7c725ac9e362e1ce").try_into().unwrap();
    let plain = hex("2db5168e932556f8089a0622981d017d");
    let expected = hex("fa4362189661d163fcd6a56d8bf0405ad636ac1bbedd5cc3ee727dc2ab4a9489");
    assert_eq!(aead_seal(&key, &nonce, &[], &plain).unwrap(), expected);
    assert_eq!(&*aead_open(&key, &nonce, &[], &expected).unwrap(), &plain);
    assert!(aead_open(&key, &nonce, &[1], &expected).is_err());
}

/// RFC 5869 Appendix A.1.
#[test]
fn hkdf_sha256_matches_rfc5869() {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(&hex("000102030405060708090a0b0c")), &[0x0b; 22]);
    let mut out = [0; 42];
    hk.expand(&hex("f0f1f2f3f4f5f6f7f8f9"), &mut out).unwrap();
    assert_eq!(
        out.as_slice(),
        hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865")
            .as_slice()
    );
}

/// RFC 9106 section 5.3. The RFC's tiny KDF profile is used here only to test
/// the primitive, never accepted as a production portable key-file profile.
#[test]
fn argon2id_matches_rfc9106() {
    use argon2::{Algorithm, Argon2, AssociatedData, Block, ParamsBuilder, Version};
    use zeroize::Zeroizing;
    let params = ParamsBuilder::new()
        .m_cost(32)
        .t_cost(3)
        .p_cost(4)
        .data(AssociatedData::new(&[4; 12]).unwrap())
        .build()
        .unwrap();
    let mut memory = Zeroizing::new(vec![Block::default(); params.block_count()]);
    let argon =
        Argon2::new_with_secret(&[3; 8], Algorithm::Argon2id, Version::V0x13, params).unwrap();
    let mut out = [0; 32];
    argon
        .hash_password_into_with_memory(&[1; 32], &[2; 16], &mut out, &mut memory[..])
        .unwrap();
    assert_eq!(
        out.as_slice(),
        hex("0d640df58d78766c08c037a34a8b53c9d01ef0452d75b65eb52520e96b01e659").as_slice()
    );
}
