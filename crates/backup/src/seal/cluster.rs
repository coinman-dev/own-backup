//! Authenticated records for encrypted backups. Contract: seal/FORMAT.md.
//!
//! Writers are not cloneable or resumable: opening a writer creates a fresh
//! random session. Shared writers reserve nonces under a mutex, before encryption
//! or I/O. Readers require the record identity from an authenticated index; a
//! valid old record must not silently replace the current one.

use std::sync::Mutex;

use own_backup_common::Result;
use zeroize::Zeroizing;

use super::{aead_open, aead_seal, bad, fill_random, ChainKey, SCHEME};

pub const RECORD_VERSION: u16 = 2;
pub const HEADER_LEN: usize = 84;
pub const TAG_LEN: usize = 16;
pub const MAX_PAYLOAD_LEN: usize = 1024 * 1024;
/// Conservative application budgets, not the maximum permitted by GCM.
pub const MAX_SESSION_RECORDS: u64 = 1 << 20;
pub const MAX_SESSION_BLOCKS: u64 = 1 << 28;
const MAGIC: &[u8; 4] = b"OBKR";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Purpose {
    Data = 1,
    Manifest = 2,
    Index = 3,
    Scratch = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Encoding {
    Raw = 0,
    Zstd = 1,
}

/// All fields are expected by the caller, not trusted from a record header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordContext {
    pub chain_id: [u8; 16],
    pub purpose: Purpose,
    /// Guest cluster for Data/Scratch; metadata chunk ordinal otherwise.
    pub index: u64,
}

/// This identity must come from the authenticated image index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RecordId {
    pub session: [u8; 32],
    pub nonce: [u8; 12],
}

/// Only ciphertext and public framing; no secret keys.
pub struct SealedRecord {
    pub id: RecordId,
    pub bytes: Vec<u8>,
}

/// Authenticated, still compressed when encoding == Zstd. The container must
/// bound decompression to decoded_len and verify the exact resulting size.
pub struct OpenedRecord {
    pub encoding: Encoding,
    pub decoded_len: u32,
    pub payload: Zeroizing<Vec<u8>>,
}

struct Session {
    id: [u8; 32],
    records: u64,
    blocks: u64,
}

impl Session {
    fn fresh() -> Result<Self> {
        let mut id = [0; 32];
        fill_random(&mut id)?;
        Ok(Self {
            id,
            records: 0,
            blocks: 0,
        })
    }
}

/// A single encryption writer, shared with Arc if concurrent writes are needed.
/// There is deliberately no Clone, deserialization, or resume-from-session API.
pub struct RecordWriter {
    chain: ChainKey,
    chain_id: [u8; 16],
    purpose: Purpose,
    session: Mutex<Session>,
}

impl RecordWriter {
    pub fn new(chain: &ChainKey, chain_id: [u8; 16], purpose: Purpose) -> Result<Self> {
        Ok(Self {
            chain: chain.clone(),
            chain_id,
            purpose,
            session: Mutex::new(Session::fresh()?),
        })
    }

    pub fn seal(
        &self,
        index: u64,
        encoding: Encoding,
        decoded_len: u32,
        payload: &[u8],
    ) -> Result<SealedRecord> {
        validate_lengths(encoding, decoded_len, payload.len())?;
        let id = self.reserve(payload.len())?;
        let context = RecordContext {
            chain_id: self.chain_id,
            purpose: self.purpose,
            index,
        };
        let header = header(context, encoding, decoded_len, payload.len() as u32, id);
        let key = record_key(&self.chain, context, id.session);
        // A reserved nonce is consumed even if encryption or subsequent I/O fails.
        let ciphertext = aead_seal(&key, &id.nonce, &header, payload)?;
        let mut bytes = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&ciphertext);
        Ok(SealedRecord { id, bytes })
    }

    fn reserve(&self, len: usize) -> Result<RecordId> {
        self.reserve_with(len, Session::fresh)
    }

    fn reserve_with(
        &self,
        len: usize,
        fresh: impl FnOnce() -> Result<Session>,
    ) -> Result<RecordId> {
        // Include GHASH's padded AAD/ciphertext and final lengths block.
        let blocks = HEADER_LEN.div_ceil(16) as u64 + len.div_ceil(16) as u64 + 1;
        let mut session = self
            .session
            .lock()
            .map_err(|_| bad("encryption writer is poisoned"))?;
        if session.records >= MAX_SESSION_RECORDS || session.blocks + blocks > MAX_SESSION_BLOCKS {
            // If the RNG fails, the exhausted session stays exhausted.
            *session = fresh()?;
        }
        let mut nonce = [0; 12];
        nonce[4..].copy_from_slice(&session.records.to_be_bytes());
        session.records += 1;
        session.blocks += blocks;
        Ok(RecordId {
            session: session.id,
            nonce,
        })
    }
}

/// Reject malformed framing and authentication failures without releasing any
/// plaintext. expected_id prevents replay only when supplied by a trusted index.
pub fn open_record(
    chain: &ChainKey,
    expected: RecordContext,
    expected_id: RecordId,
    bytes: &[u8],
) -> Result<OpenedRecord> {
    if bytes.len() < HEADER_LEN + TAG_LEN || bytes.len() > HEADER_LEN + MAX_PAYLOAD_LEN + TAG_LEN {
        return Err(bad("invalid encrypted record length"));
    }
    if &bytes[..4] != MAGIC || u16::from_be_bytes(array(bytes, 4)) != RECORD_VERSION {
        return Err(bad("unknown encrypted record format/version"));
    }
    let encoding = match bytes[7] {
        0 => Encoding::Raw,
        1 => Encoding::Zstd,
        _ => return Err(bad("unknown encrypted record encoding")),
    };
    let decoded_len = u32::from_be_bytes(array(bytes, 32));
    let payload_len = u32::from_be_bytes(array(bytes, 36)) as usize;
    validate_lengths(encoding, decoded_len, payload_len)?;
    if bytes.len() != HEADER_LEN + payload_len + TAG_LEN {
        return Err(bad("truncated or trailing encrypted record bytes"));
    }
    let id = RecordId {
        session: array(bytes, 40),
        nonce: array(bytes, 72),
    };
    if id != expected_id
        || bytes[6] != expected.purpose as u8
        || array::<16>(bytes, 8) != expected.chain_id
        || u64::from_be_bytes(array(bytes, 24)) != expected.index
        || id.nonce[..4] != [0; 4]
        || u64::from_be_bytes(array(&id.nonce, 4)) >= MAX_SESSION_RECORDS
    {
        return Err(bad(
            "encrypted record does not match its authenticated index",
        ));
    }
    let key = record_key(chain, expected, id.session);
    let payload = aead_open(&key, &id.nonce, &bytes[..HEADER_LEN], &bytes[HEADER_LEN..])?;
    Ok(OpenedRecord {
        encoding,
        decoded_len,
        payload,
    })
}

/// Total length of the record starting at `bytes`, taken from its own framing
/// so a caller that read a rounded-up range can trim to the exact record.
///
/// This reads untrusted bytes and proves nothing: `open_record` re-checks every
/// field it returns, and only the tag decides whether they were ours.
pub fn framed_len(bytes: &[u8]) -> Result<usize> {
    if bytes.len() < HEADER_LEN {
        return Err(bad("encrypted record header truncated"));
    }
    if &bytes[..4] != MAGIC || u16::from_be_bytes(array(bytes, 4)) != RECORD_VERSION {
        return Err(bad("unknown encrypted record format/version"));
    }
    let payload_len = u32::from_be_bytes(array(bytes, 36)) as usize;
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(bad("encrypted record exceeds the size limit"));
    }
    Ok(HEADER_LEN + payload_len + TAG_LEN)
}

fn validate_lengths(encoding: Encoding, decoded_len: u32, payload_len: usize) -> Result<()> {
    if payload_len > MAX_PAYLOAD_LEN || decoded_len as usize > MAX_PAYLOAD_LEN {
        return Err(bad("encrypted record exceeds the size limit"));
    }
    match encoding {
        Encoding::Raw if payload_len == decoded_len as usize => Ok(()),
        Encoding::Zstd if payload_len > 0 && payload_len < decoded_len as usize => Ok(()),
        _ => Err(bad("invalid encoded/decoded record lengths")),
    }
}

fn header(
    context: RecordContext,
    encoding: Encoding,
    decoded_len: u32,
    payload_len: u32,
    id: RecordId,
) -> [u8; HEADER_LEN] {
    let mut h = [0; HEADER_LEN];
    h[..4].copy_from_slice(MAGIC);
    h[4..6].copy_from_slice(&RECORD_VERSION.to_be_bytes());
    h[6] = context.purpose as u8;
    h[7] = encoding as u8;
    h[8..24].copy_from_slice(&context.chain_id);
    h[24..32].copy_from_slice(&context.index.to_be_bytes());
    h[32..36].copy_from_slice(&decoded_len.to_be_bytes());
    h[36..40].copy_from_slice(&payload_len.to_be_bytes());
    h[40..72].copy_from_slice(&id.session);
    h[72..84].copy_from_slice(&id.nonce);
    h
}

fn record_key(chain: &ChainKey, context: RecordContext, session: [u8; 32]) -> Zeroizing<[u8; 32]> {
    let mut salt = [0; 48];
    salt[..16].copy_from_slice(&context.chain_id);
    salt[16..].copy_from_slice(&session);
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(&salt), chain.as_bytes());
    let mut info = format!("{SCHEME}/record/").into_bytes();
    info.push(context.purpose as u8);
    let mut out = Zeroizing::new([0; 32]);
    hk.expand(&info, &mut out[..])
        .expect("fixed HKDF output size");
    out
}

/// Internal only; every caller has already checked the enclosing fixed size.
fn array<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    let mut out = [0; N];
    out.copy_from_slice(&bytes[at..at + N]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn context() -> RecordContext {
        RecordContext {
            chain_id: [7; 16],
            purpose: Purpose::Data,
            index: 55,
        }
    }

    fn writer(key: &ChainKey) -> RecordWriter {
        RecordWriter::new(key, context().chain_id, context().purpose).unwrap()
    }

    #[test]
    fn raw_and_compressed_records_round_trip_including_short_and_tail_data() {
        let key = ChainKey::generate().unwrap();
        let writer = writer(&key);
        for len in [0, 1, 15, 17, 4096, 65535, MAX_PAYLOAD_LEN] {
            let plain = vec![42; len];
            let record = writer.seal(55, Encoding::Raw, len as u32, &plain).unwrap();
            assert_eq!(record.bytes.len(), HEADER_LEN + len + TAG_LEN);
            let opened = open_record(&key, context(), record.id, &record.bytes).unwrap();
            assert_eq!(&*opened.payload, &plain);
        }
        let plain = vec![42; 65536];
        let compressed = zstd::bulk::compress(&plain, 3).unwrap();
        let record = writer
            .seal(55, Encoding::Zstd, plain.len() as u32, &compressed)
            .unwrap();
        let opened = open_record(&key, context(), record.id, &record.bytes).unwrap();
        assert_eq!(opened.encoding, Encoding::Zstd);
        assert_eq!(
            zstd::bulk::decompress(&opened.payload, opened.decoded_len as usize).unwrap(),
            plain
        );
    }

    #[test]
    fn every_header_ciphertext_and_tag_byte_is_protected() {
        let key = ChainKey::generate().unwrap();
        let record = writer(&key).seal(55, Encoding::Raw, 20, &[3; 20]).unwrap();
        for at in 0..record.bytes.len() {
            let mut broken = record.bytes.clone();
            broken[at] ^= 1;
            assert!(
                open_record(&key, context(), record.id, &broken).is_err(),
                "byte {at}"
            );
        }
        for len in 0..record.bytes.len() {
            assert!(open_record(&key, context(), record.id, &record.bytes[..len]).is_err());
        }
        let mut trailing = record.bytes.clone();
        trailing.push(0);
        assert!(open_record(&key, context(), record.id, &trailing).is_err());
        assert!(open_record(
            &ChainKey::generate().unwrap(),
            context(),
            record.id,
            &record.bytes
        )
        .is_err());
    }

    #[test]
    fn wrong_context_and_old_valid_record_are_rejected() {
        let key = ChainKey::generate().unwrap();
        let writer = writer(&key);
        let old = writer.seal(55, Encoding::Raw, 1, &[1]).unwrap();
        let new = writer.seal(55, Encoding::Raw, 1, &[2]).unwrap();
        assert!(open_record(&key, context(), new.id, &old.bytes).is_err());
        for wrong in [
            RecordContext {
                index: 56,
                ..context()
            },
            RecordContext {
                chain_id: [9; 16],
                ..context()
            },
            RecordContext {
                purpose: Purpose::Scratch,
                ..context()
            },
        ] {
            assert!(open_record(&key, wrong, new.id, &new.bytes).is_err());
        }
        // Copying a complete record to a new physical location changes nothing.
        let moved = new.bytes.clone();
        assert_eq!(
            &*open_record(&key, context(), new.id, &moved)
                .unwrap()
                .payload,
            &[2]
        );
    }

    #[test]
    fn concurrent_writes_and_new_writers_never_reuse_record_ids() {
        let key = ChainKey::generate().unwrap();
        let writer = Arc::new(writer(&key));
        let mut ids = HashSet::new();
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let writer = Arc::clone(&writer);
                    scope.spawn(move || {
                        (0..64)
                            .map(|_| writer.seal(55, Encoding::Raw, 1, &[1]).unwrap())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            for handle in handles {
                for record in handle.join().unwrap() {
                    assert!(ids.insert(record.id));
                    open_record(&key, context(), record.id, &record.bytes).unwrap();
                }
            }
        });
        let reopened = RecordWriter::new(&key, context().chain_id, Purpose::Data).unwrap();
        let new = reopened.seal(55, Encoding::Raw, 1, &[1]).unwrap();
        assert!(!ids.iter().any(|id| id.session == new.id.session));
        // An abandoned reservation (failed/cancelled I/O) is never reclaimed.
        let burned = writer.reserve(1).unwrap();
        let next = writer.seal(55, Encoding::Raw, 1, &[1]).unwrap();
        assert_ne!(burned, next.id);
    }

    #[test]
    fn both_session_budgets_rotate_before_reuse() {
        let key = ChainKey::generate().unwrap();
        let writer = writer(&key);
        for records_limit in [true, false] {
            let old = writer.seal(55, Encoding::Raw, 1, &[1]).unwrap();
            {
                let mut session = writer.session.lock().unwrap();
                if records_limit {
                    session.records = MAX_SESSION_RECORDS;
                } else {
                    session.blocks = MAX_SESSION_BLOCKS;
                }
            }
            let new = writer.seal(55, Encoding::Raw, 1, &[2]).unwrap();
            assert_ne!(old.id.session, new.id.session);
            assert_eq!(new.id.nonce, [0; 12]);
            open_record(&key, context(), old.id, &old.bytes).unwrap();
            open_record(&key, context(), new.id, &new.bytes).unwrap();
        }
    }

    #[test]
    fn malformed_lengths_are_rejected_before_decryption_or_allocation() {
        let key = ChainKey::generate().unwrap();
        let writer = writer(&key);
        assert!(writer.seal(55, Encoding::Raw, 2, &[1]).is_err());
        assert!(writer.seal(55, Encoding::Zstd, 2, &[]).is_err());
        assert!(writer.seal(55, Encoding::Zstd, 2, &[1, 2]).is_err());
        assert!(writer.seal(55, Encoding::Raw, u32::MAX, &[1]).is_err());
        let record = writer.seal(55, Encoding::Raw, 1, &[1]).unwrap();
        for at in [32, 36] {
            let mut bytes = record.bytes.clone();
            bytes[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
            assert!(open_record(&key, context(), record.id, &bytes).is_err());
        }
    }

    #[test]
    fn rng_failure_does_not_reset_an_exhausted_session() {
        let key = ChainKey::generate().unwrap();
        let writer = writer(&key);
        let original_session = {
            let mut session = writer.session.lock().unwrap();
            session.records = MAX_SESSION_RECORDS;
            session.id
        };
        assert!(writer
            .reserve_with(1, || Err(bad("injected RNG failure")))
            .is_err());
        {
            let session = writer.session.lock().unwrap();
            assert_eq!(session.id, original_session);
            assert_eq!(session.records, MAX_SESSION_RECORDS);
        }
        let resumed = writer.seal(55, Encoding::Raw, 1, &[1]).unwrap();
        assert_ne!(resumed.id.session, original_session);
        open_record(&key, context(), resumed.id, &resumed.bytes).unwrap();
    }
}
