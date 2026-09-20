//! The open half of an `.obk` header: the qcow2 header we write, the feature
//! name table that makes foreign readers refuse the file by name, and the JSON
//! document carrying what a listing may show without any key.
//!
//! Everything here is **untrusted input** until the sealed root record has been
//! opened and agrees with it (`reader.rs`). A fingerprint next to the public key
//! it claims to describe proves nothing on its own; the program must match the
//! key against the job it configured.

use std::io::{Read, Seek, SeekFrom};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use own_backup_common::Result;

use super::{
    bad, feature_name_table, HEXT_FEATURE_NAME_TABLE, HEXT_OBK_OPEN, INCOMPAT_OBK_SEAL,
    MAX_OBK_CLUSTER_BITS, MAX_OPEN_HEADER_BYTES, MIN_OBK_CLUSTER_BITS, OBK_OPEN_FORMAT,
};
use crate::qcow2::{
    COMPRESSION_TYPE_ZSTD, HEADER_LENGTH_V3, HEXT_END, INCOMPAT_COMPRESSION_TYPE, QCOW2_MAGIC,
};
use crate::seal::cluster::{RecordId, HEADER_LEN, MAX_PAYLOAD_LEN, TAG_LEN};
use crate::seal::keypair::{decode_fixed, JobPrivateKey, JobPublicKey, SealedPrivateKey};
use crate::seal::recovery::RecoveryKey;
use crate::seal::wrap::{unwrap_chain_key, WrappedChainKey};
use crate::seal::{ChainKey, SCHEME};
use crate::sidecar::ChainRole;

/// Where the sealed root record is and which record it has to be. Both halves
/// are needed: the offset alone would let a rearranged file answer with another
/// valid record of the same image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObkRootPointer {
    pub offset: u64,
    pub len: u32,
    /// Canonical padded base64: 32-byte session identity.
    pub session: String,
    /// Canonical padded base64: 12-byte nonce.
    pub nonce: String,
}

impl ObkRootPointer {
    pub fn new(offset: u64, len: usize, id: RecordId) -> Self {
        Self {
            offset,
            len: len as u32,
            session: B64.encode(id.session),
            nonce: B64.encode(id.nonce),
        }
    }

    pub fn record_id(&self) -> Result<RecordId> {
        Ok(RecordId {
            session: decode_fixed::<32>(&self.session)?,
            nonce: decode_fixed::<12>(&self.nonce)?,
        })
    }

    fn validate(&self) -> Result<()> {
        self.record_id()?;
        let len = self.len as usize;
        if !(HEADER_LEN + TAG_LEN..=HEADER_LEN + MAX_PAYLOAD_LEN + TAG_LEN).contains(&len) {
            return Err(bad("root record length out of range"));
        }
        if self.offset == 0 {
            return Err(bad("root record cannot live in the header cluster"));
        }
        Ok(())
    }
}

/// The open header, stored as UTF-8 JSON in a qcow2 header extension.
///
/// It holds no secret: the chain key inside `wrapped_chain_key` needs the job's
/// private key, and the private key inside `key_file` needs the password or the
/// recovery key. The service's local DPAPI copy is never here (`LOCAL.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObkOpenHeader {
    pub format: String,
    pub scheme: String,
    pub chain_id: Uuid,
    pub backup_id: Uuid,
    pub parent_backup_id: Option<Uuid>,
    pub role: ChainRole,
    /// RFC 3339, informational until the root record confirms the identities.
    pub created_at: String,
    pub virtual_size: u64,
    pub cluster_bits: u32,
    /// Canonical padded base64 of the 102-byte wrapped chain key.
    pub wrapped_chain_key: String,
    /// Portable password/recovery envelope, so any machine can open the image.
    pub key_file: SealedPrivateKey,
    pub root: ObkRootPointer,
}

impl ObkOpenHeader {
    /// Structural validation only — it says nothing about authenticity.
    pub fn validate(&self) -> Result<()> {
        if self.format != OBK_OPEN_FORMAT || self.scheme != SCHEME {
            return Err(bad("unknown .obk header format/scheme"));
        }
        if !(MIN_OBK_CLUSTER_BITS..=MAX_OBK_CLUSTER_BITS).contains(&self.cluster_bits) {
            return Err(bad(format!("cluster_bits {} out of range", self.cluster_bits)));
        }
        let cluster_size = 1u64 << self.cluster_bits;
        if self.virtual_size == 0 || !self.virtual_size.is_multiple_of(cluster_size) {
            return Err(bad("virtual size is not a whole number of clusters"));
        }
        if self.created_at.len() > 64 || self.created_at.chars().any(char::is_control) {
            return Err(bad("invalid creation timestamp"));
        }
        match (self.role, self.parent_backup_id) {
            (ChainRole::Full, None) => {}
            (ChainRole::Incremental | ChainRole::Differential, Some(parent)) => {
                if parent == self.backup_id {
                    return Err(bad("image claims to be its own parent"));
                }
            }
            _ => return Err(bad("chain role and parent identity disagree")),
        }
        if self.chain_id.is_nil() || self.backup_id.is_nil() {
            return Err(bad("nil chain/backup identity"));
        }
        let wrapped = self.wrapped()?;
        if &wrapped.chain_id != self.chain_id.as_bytes() {
            return Err(bad("wrapped chain key belongs to another chain"));
        }
        self.key_file.validate()?;
        self.root.validate()
    }

    pub fn wrapped(&self) -> Result<WrappedChainKey> {
        let bytes = decode_fixed::<{ WrappedChainKey::LEN }>(&self.wrapped_chain_key)?;
        WrappedChainKey::from_bytes(&bytes)
    }

    pub fn public_key(&self) -> Result<JobPublicKey> {
        self.key_file.public_key()
    }

    /// The scheduled path: the service already holds the private job key.
    pub fn chain_key_with_private(&self, private: &JobPrivateKey) -> Result<ChainKey> {
        self.validate()?;
        if private.public() != self.public_key()? {
            return Err(bad("this private key belongs to another job"));
        }
        unwrap_chain_key(private, self.chain_id.as_bytes(), &self.wrapped()?)
    }

    /// Manual opening on any machine: the portable password slot.
    pub fn chain_key_with_password(&self, password: &str) -> Result<ChainKey> {
        self.validate()?;
        let private = self.key_file.unseal_with_password(password)?;
        unwrap_chain_key(&private, self.chain_id.as_bytes(), &self.wrapped()?)
    }

    /// Manual opening without the password: the independent recovery slot.
    pub fn chain_key_with_recovery(&self, recovery: &RecoveryKey) -> Result<ChainKey> {
        self.validate()?;
        let private = self.key_file.unseal_with_recovery(recovery)?;
        unwrap_chain_key(&private, self.chain_id.as_bytes(), &self.wrapped()?)
    }

    pub fn to_json(&self) -> Result<String> {
        self.validate()?;
        let json = serde_json::to_string(self).map_err(|_| bad("cannot serialize open header"))?;
        if json.len() > MAX_OPEN_HEADER_BYTES {
            return Err(bad("open header exceeds the size limit"));
        }
        Ok(json)
    }

    pub fn from_json(json: &str) -> Result<Self> {
        if json.len() > MAX_OPEN_HEADER_BYTES {
            return Err(bad("open header exceeds the size limit"));
        }
        let header: Self =
            serde_json::from_str(json).map_err(|_| bad("invalid open header JSON"))?;
        header.validate()?;
        Ok(header)
    }
}

/// The qcow2 half of an `.obk` header, as read back from a file.
pub struct ObkFileHeader {
    pub cluster_bits: u32,
    pub cluster_size: u64,
    pub virtual_size: u64,
    pub l1_size: u32,
    pub l1_table_offset: u64,
    pub backing_file: Option<String>,
    pub open: ObkOpenHeader,
}

impl ObkFileHeader {
    /// Parse the header cluster. Refuses anything that is not exactly the shape
    /// we write: a foreign qcow2, a plaintext image, a file with unknown feature
    /// bits, internal snapshots or qcow2's own encryption.
    pub fn read<R: Read + Seek>(r: &mut R) -> Result<Self> {
        let mut prefix = [0u8; HEADER_LENGTH_V3 as usize];
        r.seek(SeekFrom::Start(0))?;
        r.read_exact(&mut prefix)?;

        if u32::from_be_bytes(super::array(&prefix, 0)) != QCOW2_MAGIC {
            return Err(bad("not a qcow2 image"));
        }
        if u32::from_be_bytes(super::array(&prefix, 4)) != 3 {
            return Err(bad("encrypted images are always qcow2 version 3"));
        }
        let incompatible = u64::from_be_bytes(super::array(&prefix, 72));
        if incompatible & INCOMPAT_OBK_SEAL == 0 {
            return Err(bad("not an encrypted image: the seal feature bit is unset"));
        }
        let unknown = incompatible & !(INCOMPAT_OBK_SEAL | INCOMPAT_COMPRESSION_TYPE);
        if unknown != 0 {
            return Err(bad(format!(
                "unknown incompatible features {unknown:#x} — refusing to read"
            )));
        }
        if u32::from_be_bytes(super::array(&prefix, 32)) != 0 {
            return Err(bad("qcow2's own encryption is never used by .obk"));
        }
        if u32::from_be_bytes(super::array(&prefix, 60)) != 0 {
            return Err(bad("images with internal snapshots are not supported"));
        }
        if prefix[104] != COMPRESSION_TYPE_ZSTD {
            return Err(bad("unexpected compression type"));
        }
        let header_length = u32::from_be_bytes(super::array(&prefix, 100));
        if header_length != HEADER_LENGTH_V3 {
            return Err(bad(format!("unexpected header length {header_length}")));
        }

        let cluster_bits = u32::from_be_bytes(super::array(&prefix, 20));
        if !(MIN_OBK_CLUSTER_BITS..=MAX_OBK_CLUSTER_BITS).contains(&cluster_bits) {
            return Err(bad(format!("cluster_bits {cluster_bits} out of range")));
        }
        let cluster_size = 1u64 << cluster_bits;
        let virtual_size = u64::from_be_bytes(super::array(&prefix, 24));
        if virtual_size == 0 || !virtual_size.is_multiple_of(cluster_size) {
            return Err(bad("virtual size is not a whole number of clusters"));
        }
        let l1_size = u32::from_be_bytes(super::array(&prefix, 36));
        // Defensive cap, as in the plaintext reader: a table this large is
        // corruption rather than a disk, and it is checked before the geometry
        // is believed enough to allocate anything from it.
        if l1_size > (1 << 25) {
            return Err(bad(format!("L1 size {l1_size} out of range")));
        }
        let expected_l1 = (virtual_size / cluster_size).div_ceil(cluster_size / 8);
        if u64::from(l1_size) != expected_l1 {
            return Err(bad(format!(
                "L1 table of {l1_size} entries does not match a {virtual_size}-byte disk"
            )));
        }
        let l1_table_offset = u64::from_be_bytes(super::array(&prefix, 40));
        if l1_table_offset < cluster_size || !l1_table_offset.is_multiple_of(cluster_size) {
            return Err(bad("L1 table offset is not a cluster boundary after the header"));
        }

        // Extensions and the backing file name live in the header cluster.
        let mut cluster = vec![0u8; cluster_size as usize];
        r.seek(SeekFrom::Start(0))?;
        r.read_exact(&mut cluster)?;

        let backing_file_offset = u64::from_be_bytes(super::array(&prefix, 8));
        let backing_file_size = u32::from_be_bytes(super::array(&prefix, 16)) as usize;
        let backing_file = if backing_file_offset == 0 {
            if backing_file_size != 0 {
                return Err(bad("backing file size without an offset"));
            }
            None
        } else {
            let start = backing_file_offset as usize;
            if backing_file_size == 0
                || backing_file_size > 1023
                || start < HEADER_LENGTH_V3 as usize
                || start + backing_file_size > cluster.len()
            {
                return Err(bad("backing file name outside the header cluster"));
            }
            Some(
                String::from_utf8(cluster[start..start + backing_file_size].to_vec())
                    .map_err(|_| bad("backing file name is not UTF-8"))?,
            )
        };

        let open = ObkOpenHeader::from_json(&read_open_extension(&cluster)?)?;
        if open.cluster_bits != cluster_bits || open.virtual_size != virtual_size {
            return Err(bad("open header and qcow2 header disagree about geometry"));
        }
        if open.parent_backup_id.is_some() != backing_file.is_some() {
            return Err(bad("open header and backing file disagree about a parent"));
        }

        Ok(Self {
            cluster_bits,
            cluster_size,
            virtual_size,
            l1_size,
            l1_table_offset,
            backing_file,
            open,
        })
    }
}

/// Walk the header extensions and return the single OBKH document.
fn read_open_extension(cluster: &[u8]) -> Result<String> {
    let mut at = HEADER_LENGTH_V3 as usize;
    let mut found: Option<String> = None;
    loop {
        if at + 8 > cluster.len() {
            return Err(bad("header extensions run past the header cluster"));
        }
        let kind = u32::from_be_bytes(super::array(cluster, at));
        let len = u32::from_be_bytes(super::array(cluster, at + 4)) as usize;
        if kind == HEXT_END {
            break;
        }
        if len > cluster.len() || at + 8 + len > cluster.len() {
            return Err(bad("header extension runs past the header cluster"));
        }
        if kind == HEXT_OBK_OPEN {
            if found.is_some() {
                return Err(bad("two open headers in one image"));
            }
            if len > MAX_OPEN_HEADER_BYTES {
                return Err(bad("open header exceeds the size limit"));
            }
            found = Some(
                String::from_utf8(cluster[at + 8..at + 8 + len].to_vec())
                    .map_err(|_| bad("open header is not UTF-8"))?,
            );
        }
        at += 8 + len.div_ceil(8) * 8;
    }
    found.ok_or_else(|| bad("encrypted image without an open header"))
}

/// The header extensions an `.obk` writer emits, in order.
pub(crate) fn obk_extensions(open: &ObkOpenHeader) -> Result<Vec<(u32, Vec<u8>)>> {
    Ok(vec![
        (HEXT_FEATURE_NAME_TABLE, feature_name_table()),
        (HEXT_OBK_OPEN, open.to_json()?.into_bytes()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::keypair::{JobKeyPair, KdfParams};

    /// One way of breaking a header, applied to a good one.
    type Edit = Box<dyn Fn(&mut ObkOpenHeader)>;
    use crate::seal::wrap::wrap_chain_key;

    pub(crate) fn sample_header(chain: &ChainKey, chain_id: Uuid, backup_id: Uuid) -> ObkOpenHeader {
        let pair = JobKeyPair::generate().unwrap();
        let recovery = RecoveryKey::generate().unwrap();
        let key_file = SealedPrivateKey::seal(
            "job-1",
            &pair,
            "correct horse battery staple",
            &recovery,
            &KdfParams::test_profile(),
        )
        .unwrap();
        let wrapped = wrap_chain_key(&pair.public, chain_id.as_bytes(), chain).unwrap();
        ObkOpenHeader {
            format: OBK_OPEN_FORMAT.into(),
            scheme: SCHEME.into(),
            chain_id,
            backup_id,
            parent_backup_id: None,
            role: ChainRole::Full,
            created_at: "2026-09-21T10:00:00Z".into(),
            virtual_size: 1 << 20,
            cluster_bits: 16,
            wrapped_chain_key: B64.encode(wrapped.to_bytes()),
            key_file,
            root: ObkRootPointer::new(
                1 << 16,
                HEADER_LEN + 10 + TAG_LEN,
                RecordId {
                    session: [1; 32],
                    nonce: [2; 12],
                },
            ),
        }
    }

    #[test]
    fn an_open_header_round_trips_through_json() {
        let chain = ChainKey::generate().unwrap();
        let header = sample_header(&chain, Uuid::new_v4(), Uuid::new_v4());
        let parsed = ObkOpenHeader::from_json(&header.to_json().unwrap()).unwrap();
        assert_eq!(parsed, header);
    }

    #[test]
    fn structural_nonsense_is_refused_before_any_key_work() {
        let chain = ChainKey::generate().unwrap();
        let good = sample_header(&chain, Uuid::new_v4(), Uuid::new_v4());
        let cases: Vec<(&str, Edit)> = vec![
            ("format", Box::new(|h: &mut ObkOpenHeader| h.format = "other".into())),
            ("scheme", Box::new(|h: &mut ObkOpenHeader| h.scheme = "own-backup/v1".into())),
            ("cluster bits", Box::new(|h: &mut ObkOpenHeader| h.cluster_bits = 9)),
            ("virtual size", Box::new(|h: &mut ObkOpenHeader| h.virtual_size += 1)),
            ("nil chain", Box::new(|h: &mut ObkOpenHeader| h.chain_id = Uuid::nil())),
            (
                "role without parent",
                Box::new(|h: &mut ObkOpenHeader| h.role = ChainRole::Incremental),
            ),
            (
                "parent without role",
                Box::new(|h: &mut ObkOpenHeader| h.parent_backup_id = Some(Uuid::new_v4())),
            ),
            (
                "root in the header cluster",
                Box::new(|h: &mut ObkOpenHeader| h.root.offset = 0),
            ),
            ("root length", Box::new(|h: &mut ObkOpenHeader| h.root.len = 1)),
            (
                "wrapped key of another chain",
                Box::new(|h: &mut ObkOpenHeader| h.chain_id = Uuid::new_v4()),
            ),
        ];
        for (what, break_it) in cases {
            let mut broken = good.clone();
            break_it(&mut broken);
            assert!(broken.validate().is_err(), "{what} must be refused");
        }
    }

    #[test]
    fn the_chain_key_comes_back_through_password_recovery_and_the_private_key() {
        let chain = ChainKey::generate().unwrap();
        let chain_id = Uuid::new_v4();
        let pair = JobKeyPair::generate().unwrap();
        let recovery = RecoveryKey::generate().unwrap();
        let key_file = SealedPrivateKey::seal(
            "job-1",
            &pair,
            "пароль архива",
            &recovery,
            &KdfParams::test_profile(),
        )
        .unwrap();
        let wrapped = wrap_chain_key(&pair.public, chain_id.as_bytes(), &chain).unwrap();
        let mut header = sample_header(&chain, chain_id, Uuid::new_v4());
        header.key_file = key_file;
        header.wrapped_chain_key = B64.encode(wrapped.to_bytes());

        assert_eq!(
            header.chain_key_with_password("пароль архива").unwrap().as_bytes(),
            chain.as_bytes()
        );
        assert_eq!(
            header.chain_key_with_recovery(&recovery).unwrap().as_bytes(),
            chain.as_bytes()
        );
        assert_eq!(
            header.chain_key_with_private(&pair.private).unwrap().as_bytes(),
            chain.as_bytes()
        );
        assert!(header.chain_key_with_password("wrong").is_err());
        let other = JobKeyPair::generate().unwrap();
        assert!(header.chain_key_with_private(&other.private).is_err());
    }
}
