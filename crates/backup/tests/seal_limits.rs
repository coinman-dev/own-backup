//! These tests link the ordinary library, without its unit-test-only KDF profile.
use own_backup_backup::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
use own_backup_backup::seal::recovery::RecoveryKey;

#[test]
fn production_library_refuses_the_unit_test_password_profile() {
    let pair = JobKeyPair::generate().unwrap();
    let recovery = RecoveryKey::generate().unwrap();
    let weak = KdfParams {
        algorithm: "argon2id".into(),
        m_kib: 1024,
        t: 1,
        p: 1,
    };
    let error = SealedPrivateKey::seal("job", &pair, "a long password", &recovery, &weak)
        .unwrap_err()
        .to_string();
    assert!(error.contains("resource/security limits"), "{error}");
}

#[test]
fn portable_keys_and_records_reopen_with_production_kdf_after_serialization() {
    use own_backup_backup::seal::cluster::{
        open_record, Encoding, Purpose, RecordContext, RecordWriter,
    };
    use own_backup_backup::seal::wrap::{unwrap_chain_key, wrap_chain_key, WrappedChainKey};
    use own_backup_backup::seal::ChainKey;

    let pair = JobKeyPair::generate().unwrap();
    let recovery = RecoveryKey::generate().unwrap();
    // The minimum supported production profile, not the unit-test exemption.
    let kdf = KdfParams {
        algorithm: "argon2id".into(),
        m_kib: 64 * 1024,
        t: 3,
        p: 1,
    };
    let portable = SealedPrivateKey::seal("job", &pair, "correct horse battery", &recovery, &kdf)
        .unwrap()
        .to_json()
        .unwrap();
    let chain = ChainKey::generate().unwrap();
    let chain_id = [9; 16];
    let wrapped = wrap_chain_key(&pair.public, &chain_id, &chain)
        .unwrap()
        .to_bytes();
    let writer = RecordWriter::new(&chain, chain_id, Purpose::Data).unwrap();
    let record = writer.seal(42, Encoding::Raw, 7, b"payload").unwrap();
    drop(writer);
    drop(chain);
    drop(pair);

    let parsed = SealedPrivateKey::from_json(&portable).unwrap();
    let wrapped = WrappedChainKey::from_bytes(&wrapped).unwrap();
    let by_password = parsed
        .unseal_with_password("correct horse battery")
        .unwrap();
    let by_recovery = parsed.unseal_with_recovery(&recovery).unwrap();
    for private in [by_password, by_recovery] {
        let chain = unwrap_chain_key(&private, &chain_id, &wrapped).unwrap();
        let expected = RecordContext {
            chain_id,
            purpose: Purpose::Data,
            index: 42,
        };
        let opened = open_record(&chain, expected, record.id, &record.bytes).unwrap();
        assert_eq!(&*opened.payload, b"payload");
    }
}
