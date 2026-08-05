//! BitLocker unlock: recover the Full Volume Encryption Key from a passphrase.
//!
//! This is the Windows counterpart of [`crate::luks`]: Linux has no BitLocker
//! driver, so we parse the FVE metadata and run the key derivation ourselves,
//! then present a decrypted view of the volume (see [`BitLockerVolume`]). Only
//! the *user password* protector is handled (the common interactive case) — the
//! passphrase is stretched, used to AES-CCM-unwrap the Volume Master Key (VMK),
//! and the VMK in turn AES-CCM-unwraps the FVEK.
//!
//! Scope / safety (mirrors the "any doubt → Err" policy of the used-block maps):
//! - Sector cipher: **AES-XTS 128/256** (`0x8004`/`0x8005`), the Windows 10+
//!   default. AES-CBC and the Vista/7 Elephant diffuser are rejected rather than
//!   implemented from memory — a plaintext backup with a wrong cipher is worse
//!   than none. CBC support is future work.
//! - After recovering the FVEK we decrypt sector 0 and require a plausible NTFS
//!   boot sector; a wrong key or an unhandled on-disk layout fails loudly.
//! - All AES-CCM / key-stretch primitives are validated by unit tests (an RFC
//!   3610 known-answer vector for CCM, a self-built metadata round-trip for the
//!   whole VMK→FVEK→XTS pipeline).
//!
//! End-to-end reads of a *real* BitLocker volume (in particular the relocated
//! boot region and the FVE metadata holes) can only be confirmed on a Windows
//! host — the same validation status as the dynamic-VHDX output.
//!
//! Read-only: we never write the FVE metadata or the volume.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};
use aes::{Aes128, Aes256};
use sha2::{Digest, Sha256};
use xts_mode::Xts128;
use zeroize::Zeroizing;

use own_backup_common::image::VolumeEncryptionKind;
use own_backup_common::{OwnBackupError, Result};

use crate::fat_map::RawVolume;

// --- On-disk BitLocker constants -------------------------------------------

/// FVE metadata signature (also the OEM id of Vista/7 BitLocker volumes).
const FVE_SIGNATURE: &[u8; 8] = b"-FVE-FS-";
/// The "BitLocker" volume GUID as stored in the boot sector at offset 0x160
/// (mixed-endian: `4967D63B-2E29-4AD8-8399-F6A339E3D001`).
const BITLOCKER_GUID: [u8; 16] = [
    0x3B, 0xD6, 0x67, 0x49, 0x29, 0x2E, 0xD8, 0x4A, 0x83, 0x99, 0xF6, 0xA3, 0x39, 0xE3, 0xD0, 0x01,
];
/// "BitLocker To Go" volume GUID (`4967D63C-…-D000`).
const BITLOCKER_TO_GO_GUID: [u8; 16] = [
    0x3C, 0xD6, 0x67, 0x49, 0x29, 0x2E, 0xD8, 0x4A, 0x83, 0x99, 0xF6, 0xA3, 0x39, 0xE3, 0xD0, 0x00,
];
/// Boot-sector offset of the three FVE metadata block byte offsets (u64 each).
const INFO_OFFSETS_AT: usize = 0x170;

/// FVE datum `value_type` codes.
const DATUM_KEY: u16 = 0x0001;
const DATUM_STRETCH_KEY: u16 = 0x0003;
const DATUM_AES_CCM: u16 = 0x0005;
const DATUM_VMK: u16 = 0x0008;

/// Encryption algorithm codes (dataset + FVEK key datum).
const ALG_AES_XTS_128: u16 = 0x8004;
const ALG_AES_XTS_256: u16 = 0x8005;

/// VMK key-protection type for a user passphrase.
const PROTECT_PASSWORD: u16 = 0x2000;

/// The ways a volume can be unlocked, as the metadata names them.
///
/// A volume carries one VMK datum per way in, and each says which one it is.
/// Reading that list costs nothing — no password, no privileges beyond the
/// disk read we already do — and it is the difference between asking for a
/// passphrase that exists and telling the user their volume has none.
///
/// **The codes beyond `PASSWORD` come from libbde's documentation, not from a
/// volume we have opened.** They are reported with their raw value beside the
/// name so a real volume can confirm or correct them; nothing decides anything
/// on a code we have not seen.
pub const PROTECTOR_KINDS: [(u16, &str); 6] = [
    (0x0000, "защита приостановлена (ключ лежит открыто)"),
    (0x0100, "TPM"),
    (0x0200, "ключ запуска (USB)"),
    (0x0500, "TPM + PIN"),
    (0x0800, "ключ восстановления (48 цифр)"),
    (PROTECT_PASSWORD, "пароль"),
];

/// Real BitLocker stretch iteration count (2^20).
const STRETCH_ITERATIONS: u32 = 0x0010_0000;

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("bitlocker: {msg}"))
}

fn u16at(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) as u64
}
fn u32at(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64
}
fn u64at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// True if `header` (≥ 0x170 bytes off the volume start) is a BitLocker volume.
/// Detects both the Vista/7 `-FVE-FS-` signature and the Win8+ variant (an NTFS
/// boot sector carrying the BitLocker GUID at 0x160).
pub fn is_bitlocker(header: &[u8]) -> bool {
    if header.len() >= 11 && &header[3..11] == FVE_SIGNATURE {
        return true;
    }
    header.len() >= 0x170
        && (header[0x160..0x170] == BITLOCKER_GUID || header[0x160..0x170] == BITLOCKER_TO_GO_GUID)
}

// ---------------------------------------------------------------------------
// AES primitives (raw block, CCM) — the `ccm`/`cbc` crates are unavailable, so
// AES-CCM is built directly on the block cipher (CTR + CBC-MAC, RFC 3610).
// ---------------------------------------------------------------------------

/// An AES-128 or AES-256 block cipher chosen by key length.
enum Aes {
    A128(Box<Aes128>),
    A256(Box<Aes256>),
}

impl Aes {
    fn new(key: &[u8]) -> Result<Aes> {
        match key.len() {
            16 => Ok(Aes::A128(Box::new(Aes128::new(GenericArray::from_slice(key))))),
            32 => Ok(Aes::A256(Box::new(Aes256::new(GenericArray::from_slice(key))))),
            n => Err(bad(format!("unsupported AES key length {n}"))),
        }
    }
    fn encrypt_block(&self, block: &mut [u8; 16]) {
        let ga = GenericArray::from_mut_slice(block);
        match self {
            Aes::A128(c) => c.encrypt_block(ga),
            Aes::A256(c) => c.encrypt_block(ga),
        }
    }
}

/// AES-CCM decrypt-and-verify (RFC 3610). `M = tag.len()`, `L = 15 - nonce.len()`.
/// Returns the plaintext, or an error if the authentication tag does not match
/// (which — for BitLocker — means the wrong passphrase). AES-CCM uses block
/// *encryption* for both the keystream and the CBC-MAC, even when decrypting.
fn aes_ccm_decrypt(
    key: &[u8],
    nonce: &[u8],
    aad: &[u8],
    ct: &[u8],
    tag: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    let m = tag.len();
    if !(4..=16).contains(&m) || m % 2 != 0 {
        return Err(bad(format!("bad CCM tag length {m}")));
    }
    if !(7..=13).contains(&nonce.len()) {
        return Err(bad(format!("bad CCM nonce length {}", nonce.len())));
    }
    let l = 15 - nonce.len();
    if l < 8 && (ct.len() as u128) >= (1u128 << (8 * l)) {
        return Err(bad("CCM message too long for the nonce length"));
    }
    let cipher = Aes::new(key)?;

    // Counter block A_i = flags(L-1) ‖ nonce ‖ counter(L bytes, big-endian).
    let ctr_block = |counter: u64| -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0] = (l - 1) as u8;
        b[1..1 + nonce.len()].copy_from_slice(nonce);
        let cb = counter.to_be_bytes();
        b[16 - l..].copy_from_slice(&cb[8 - l..]);
        b
    };

    // S_0 encrypts the tag; S_1.. form the payload keystream.
    let mut s0 = ctr_block(0);
    cipher.encrypt_block(&mut s0);

    let mut plain = Zeroizing::new(vec![0u8; ct.len()]);
    for (i, chunk) in ct.chunks(16).enumerate() {
        let mut ks = ctr_block(i as u64 + 1);
        cipher.encrypt_block(&mut ks);
        for (j, &c) in chunk.iter().enumerate() {
            plain[i * 16 + j] = c ^ ks[j];
        }
    }

    // CBC-MAC over (optional AAD ‖ plaintext).
    let mut b0 = [0u8; 16];
    b0[0] = (if aad.is_empty() { 0 } else { 0x40 }) | (((m as u8 - 2) / 2) << 3) | (l - 1) as u8;
    b0[1..1 + nonce.len()].copy_from_slice(nonce);
    let lb = (plain.len() as u64).to_be_bytes();
    b0[16 - l..].copy_from_slice(&lb[8 - l..]);

    let mut x = b0;
    cipher.encrypt_block(&mut x);

    let mac_block = |x: &mut [u8; 16], block: &[u8], cipher: &Aes| {
        for (xi, bi) in x.iter_mut().zip(block.iter()) {
            *xi ^= bi;
        }
        cipher.encrypt_block(x);
    };

    if !aad.is_empty() {
        // Only the common 0 < len < 0xFF00 encoding is needed here.
        if aad.len() >= 0xFF00 {
            return Err(bad("CCM AAD too long"));
        }
        let mut enc = Vec::with_capacity(2 + aad.len());
        enc.extend_from_slice(&(aad.len() as u16).to_be_bytes());
        enc.extend_from_slice(aad);
        for block in enc.chunks(16) {
            let mut padded = [0u8; 16];
            padded[..block.len()].copy_from_slice(block);
            mac_block(&mut x, &padded, &cipher);
        }
    }
    for block in plain.chunks(16) {
        let mut padded = [0u8; 16];
        padded[..block.len()].copy_from_slice(block);
        mac_block(&mut x, &padded, &cipher);
    }

    // Transmitted tag = MAC ⊕ S_0[..M]. Verify in constant-ish time.
    let mut diff = 0u8;
    for i in 0..m {
        diff |= tag[i] ^ (x[i] ^ s0[i]);
    }
    if diff != 0 {
        return Err(bad("CCM authentication failed (wrong passphrase?)"));
    }
    Ok(plain)
}

/// Stretch a UTF-16-LE password with a 16-byte salt into a 32-byte AES key.
/// `iterations` is [`STRETCH_ITERATIONS`] in production; tests pass a small
/// value. The pool is `updated_hash(32) ‖ password_hash(32) ‖ salt(16) ‖
/// counter(u64 LE)` re-hashed with SHA-256 each round.
fn stretch_key(password_utf16: &[u8], salt: &[u8], iterations: u32) -> Zeroizing<[u8; 32]> {
    let h1 = Sha256::digest(password_utf16);
    let password_hash = Sha256::digest(h1);

    let mut pool = Zeroizing::new([0u8; 88]);
    pool[32..64].copy_from_slice(&password_hash);
    pool[64..80].copy_from_slice(&salt[..16]);
    for i in 0..iterations {
        pool[80..88].copy_from_slice(&(i as u64).to_le_bytes());
        let d = Sha256::digest(&pool[..]);
        pool[0..32].copy_from_slice(&d);
    }
    let mut out = Zeroizing::new([0u8; 32]);
    out.copy_from_slice(&pool[0..32]);
    out
}

fn utf16le(password: &str) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(password.encode_utf16().flat_map(|u| u.to_le_bytes()).collect())
}

// ---------------------------------------------------------------------------
// FVE metadata / datum parsing
// ---------------------------------------------------------------------------

/// One FVE datum (metadata entry): an 8-byte header then `size - 8` data bytes.
struct Datum<'a> {
    entry_type: u16,
    value_type: u16,
    data: &'a [u8],
}

/// Iterate the datums packed in `buf` (each: u16 size, u16 entry_type, u16
/// value_type, u16 version, then data). Stops on a zero/oversized size.
fn datums(buf: &[u8]) -> Vec<Datum<'_>> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 8 <= buf.len() {
        let size = u16at(buf, off) as usize;
        if size < 8 || off + size > buf.len() {
            break;
        }
        out.push(Datum {
            entry_type: u16at(buf, off + 2) as u16,
            value_type: u16at(buf, off + 4) as u16,
            data: &buf[off + 8..off + size],
        });
        off += size;
    }
    out
}

/// Recursively collect every datum of `value_type` reachable from `buf`
/// (datums can nest inside VMK / stretch-key datums).
fn find_datums<'a>(buf: &'a [u8], value_type: u16, out: &mut Vec<&'a [u8]>) {
    for d in datums(buf) {
        if d.value_type == value_type {
            out.push(d.data);
        }
        // VMK and stretch-key datums carry nested datums after their fixed head.
        match d.value_type {
            DATUM_VMK if d.data.len() > 28 => find_datums(&d.data[28..], value_type, out),
            DATUM_STRETCH_KEY if d.data.len() > 20 => find_datums(&d.data[20..], value_type, out),
            _ => {}
        }
    }
}

/// Parameters recovered from a BitLocker volume: the FVEK and everything the
/// decrypting view needs.
#[derive(Clone)]
pub struct BitLockerParams {
    /// Full Volume Encryption Key (XTS: two concatenated AES keys).
    pub fvek: Zeroizing<Vec<u8>>,
    /// Volume encryption algorithm (`0x8004` XTS-128 / `0x8005` XTS-256).
    pub algorithm: u16,
    /// Cipher data-unit (sector) size in bytes.
    pub sector_size: u32,
    /// Size of the encrypted volume region in bytes (the plaintext length).
    pub encrypted_size: u64,
    /// Number of relocated boot sectors at the volume start.
    pub nb_backup_sectors: u64,
    /// Device byte offset holding those relocated sectors' ciphertext.
    pub boot_backup_addr: u64,
    pub kind: VolumeEncryptionKind,
}

impl BitLockerParams {
    /// Decrypt `buf`, which holds the ciphertext of `count` cipher sectors
    /// whose *virtual* sector index starts at `first_sector`. `buf.len()` must
    /// be a multiple of [`Self::sector_size`].
    pub fn decrypt(&self, first_sector: u64, buf: &mut [u8]) -> Result<()> {
        let ss = self.sector_size as usize;
        if ss == 0 || buf.len() % ss != 0 {
            return Err(bad("decrypt buffer not a multiple of the sector size"));
        }
        // BitLocker AES-XTS: the data-unit number (tweak) is the sector index,
        // stepping by 1 per sector, encoded little-endian.
        let base = first_sector as u128;
        let tweak = |i: u128| (base + i).to_le_bytes();
        match self.algorithm {
            ALG_AES_XTS_128 => {
                if self.fvek.len() != 32 {
                    return Err(bad("XTS-128 needs a 32-byte FVEK"));
                }
                let c1 = Aes128::new(GenericArray::from_slice(&self.fvek[..16]));
                let c2 = Aes128::new(GenericArray::from_slice(&self.fvek[16..32]));
                Xts128::new(c1, c2).decrypt_area(buf, ss, 0, tweak);
            }
            ALG_AES_XTS_256 => {
                if self.fvek.len() != 64 {
                    return Err(bad("XTS-256 needs a 64-byte FVEK"));
                }
                let c1 = Aes256::new(GenericArray::from_slice(&self.fvek[..32]));
                let c2 = Aes256::new(GenericArray::from_slice(&self.fvek[32..64]));
                Xts128::new(c1, c2).decrypt_area(buf, ss, 0, tweak);
            }
            a => return Err(bad(format!("unsupported volume cipher {a:#x}"))),
        }
        Ok(())
    }
}

/// Read `len` bytes at `offset`, padded up to a 512-byte multiple.
fn read_at(vol: &mut dyn RawVolume, offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len.next_multiple_of(512)];
    vol.read_at(offset, &mut buf)?;
    buf.truncate(len);
    Ok(buf)
}

/// One way into a volume, as its metadata declares it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Protector {
    /// Raw `protection_type` from the VMK datum.
    pub kind: u16,
    /// What that code means, or "неизвестно" when it is one we have not seen.
    pub name: String,
    /// Can Own Backup open the volume this way today?
    pub usable: bool,
}

/// What a BitLocker volume says about itself before anyone types a password.
///
/// Read-only, and read from the DISK rather than the volume, so it works on a
/// locked volume — which is the only kind worth asking about.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BitLockerInfo {
    /// Volume-level cipher from the dataset header.
    pub cipher: u16,
    pub cipher_name: String,
    /// Do we implement that cipher?
    pub cipher_supported: bool,
    pub sector_size: u32,
    /// Size of the encrypted region — the plaintext length of the volume.
    pub encrypted_size: u64,
    /// Relocated boot sectors: how many, and where their ciphertext lives.
    pub nb_backup_sectors: u64,
    pub boot_backup_addr: u64,
    /// Which of the three metadata copies parsed (0, 1, 2).
    pub metadata_copy: usize,
    pub protectors: Vec<Protector>,
    /// The 0x40-byte metadata block header and the 0x30-byte dataset header,
    /// as hex. Printed because the fields we do NOT claim to understand —
    /// among them whatever records a conversion in progress — have to be
    /// learned from real volumes before anything is decided on them.
    pub header_hex: String,
    pub dataset_hex: String,
}

impl BitLockerInfo {
    /// Can the volume be opened with a passphrase at all?
    pub fn has_password(&self) -> bool {
        self.protectors.iter().any(|p| p.kind == PROTECT_PASSWORD)
    }
    /// Nothing we can do with it today.
    pub fn openable(&self) -> bool {
        self.cipher_supported && self.protectors.iter().any(|p| p.usable)
    }
}

fn cipher_name(alg: u16) -> (String, bool) {
    match alg {
        ALG_AES_XTS_128 => ("AES-XTS 128".into(), true),
        ALG_AES_XTS_256 => ("AES-XTS 256".into(), true),
        0x8000 => ("AES-CBC 128 + диффузор (Vista/7)".into(), false),
        0x8001 => ("AES-CBC 256 + диффузор (Vista/7)".into(), false),
        0x8002 => ("AES-CBC 128".into(), false),
        0x8003 => ("AES-CBC 256".into(), false),
        other => (format!("неизвестный шифр {other:#06x}"), false),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// Read what a BitLocker volume declares about itself — cipher, geometry and
/// the list of ways in — without a password and without writing anything.
pub fn inspect_bitlocker(vol: &mut dyn RawVolume) -> Result<BitLockerInfo> {
    let boot = read_at(vol, 0, 512)?;
    if !is_bitlocker(&boot) {
        return Err(bad("not a BitLocker volume"));
    }
    let sector_size = u16at(&boot, 0x0B) as u32;

    let offsets: Vec<u64> = (0..3).map(|i| u64at(&boot, INFO_OFFSETS_AT + i * 8)).collect();
    let mut last_err = String::new();
    for (i, &info_off) in offsets.iter().enumerate() {
        if info_off == 0 || info_off + 0x70 > vol.len() {
            continue;
        }
        match inspect_metadata(vol, info_off, sector_size, i) {
            Ok(info) => return Ok(info),
            Err(e) => last_err = e.to_string(),
        }
    }
    // Nothing parsed. The answer is in the bytes, so they go into the message:
    // guessing at a layout from the outside is how an evening disappears.
    Err(bad(format!(
        "метаданные FVE не прочитаны. Окно {} байт, сектор {sector_size}; \
         смещения копий: {}; сектор 0 [0x00..0x10]: {}; [0x160..0x190]: {}{}",
        vol.len(),
        offsets.iter().map(|o| format!("{o:#x}")).collect::<Vec<_>>().join(", "),
        hex(&boot[..0x10]),
        hex(&boot[0x160..0x190.min(boot.len())]),
        if last_err.is_empty() { String::new() } else { format!("; последняя ошибка: {last_err}") },
    )))
}

fn inspect_metadata(
    vol: &mut dyn RawVolume,
    info_off: u64,
    sector_size: u32,
    copy: usize,
) -> Result<BitLockerInfo> {
    let head = read_at(vol, info_off, 0x70)?;
    if &head[0..8] != FVE_SIGNATURE {
        return Err(bad("FVE metadata lacks the -FVE-FS- signature"));
    }
    let dataset_size = u32at(&head, 0x40);
    if !(0x30..=16 * 1024 * 1024).contains(&dataset_size) {
        return Err(bad(format!("implausible dataset size {dataset_size}")));
    }
    let dataset = read_at(vol, info_off + 0x40, dataset_size as usize)?;
    let cipher = u16at(&dataset, 0x24) as u16;
    let (cipher_name, cipher_supported) = cipher_name(cipher);

    let mut protectors = Vec::new();
    for d in datums(&dataset[0x30..]) {
        if d.entry_type != 0x0002 || d.value_type != DATUM_VMK || d.data.len() < 28 {
            continue;
        }
        let kind = u16at(d.data, 0x1A) as u16;
        let known = PROTECTOR_KINDS.iter().find(|(k, _)| *k == kind);
        let p = Protector {
            kind,
            name: known.map(|(_, n)| (*n).to_string()).unwrap_or_else(|| "неизвестно".into()),
            // Only the passphrase, and only with a cipher we implement.
            usable: kind == PROTECT_PASSWORD && cipher_supported,
        };
        if !protectors.contains(&p) {
            protectors.push(p);
        }
    }

    Ok(BitLockerInfo {
        cipher,
        cipher_name,
        cipher_supported,
        sector_size,
        encrypted_size: u64at(&head, 0x10),
        nb_backup_sectors: u32at(&head, 0x1C),
        boot_backup_addr: u64at(&head, 0x38),
        metadata_copy: copy,
        protectors,
        header_hex: hex(&head[..0x40]),
        dataset_hex: hex(&dataset[..0x30.min(dataset.len())]),
    })
}

/// Recover the FVEK of a BitLocker volume using `password`. Errors on a wrong
/// passphrase, an unsupported cipher, or a malformed / unhandled layout.
pub fn open_bitlocker(vol: &mut dyn RawVolume, password: &str) -> Result<BitLockerParams> {
    open_bitlocker_with_iters(vol, password, STRETCH_ITERATIONS)
}

/// [`open_bitlocker`] with an explicit stretch iteration count (tests use a
/// small one; production always uses [`STRETCH_ITERATIONS`]).
fn open_bitlocker_with_iters(
    vol: &mut dyn RawVolume,
    password: &str,
    iterations: u32,
) -> Result<BitLockerParams> {
    let boot = read_at(vol, 0, 512)?;
    if !is_bitlocker(&boot) {
        return Err(bad("not a BitLocker volume"));
    }
    let sector_size = u16at(&boot, 0x0B);
    if !sector_size.is_power_of_two() || !(512..=4096).contains(&sector_size) {
        return Err(bad(format!("implausible sector size {sector_size}")));
    }

    // The three FVE metadata copies — try each until one parses and unlocks.
    let mut last_err = bad("no FVE metadata copy could be parsed");
    for i in 0..3 {
        let info_off = u64at(&boot, INFO_OFFSETS_AT + i * 8);
        if info_off == 0 || info_off + 0x70 > vol.len() {
            continue;
        }
        match unlock_from_metadata(vol, info_off, sector_size as u32, password, iterations) {
            Ok(params) => return verify_ntfs(vol, params),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Parse one FVE metadata block and recover the FVEK from the password.
fn unlock_from_metadata(
    vol: &mut dyn RawVolume,
    info_off: u64,
    sector_size: u32,
    password: &str,
    iterations: u32,
) -> Result<BitLockerParams> {
    // Metadata header (bitlocker_information): read the fixed head + dataset.
    let head = read_at(vol, info_off, 0x70)?;
    if &head[0..8] != FVE_SIGNATURE {
        return Err(bad("FVE metadata lacks the -FVE-FS- signature"));
    }
    let encrypted_size = u64at(&head, 0x10);
    let nb_backup_sectors = u32at(&head, 0x1C);
    let boot_backup_addr = u64at(&head, 0x38);

    // Dataset at metadata offset 0x40.
    let dataset_size = u32at(&head, 0x40);
    if !(0x30..=16 * 1024 * 1024).contains(&dataset_size) {
        return Err(bad(format!("implausible dataset size {dataset_size}")));
    }
    let dataset = read_at(vol, info_off + 0x40, dataset_size as usize)?;
    let entries = &dataset[0x30..]; // datums follow the 0x30-byte dataset header

    // --- Recover the VMK: try every VMK datum with the given password ---
    let mut vmk: Option<Zeroizing<Vec<u8>>> = None;
    for d in datums(entries) {
        if d.entry_type != 0x0002 || d.value_type != DATUM_VMK || d.data.len() < 28 {
            continue;
        }
        // VMK head: guid(16) ‖ last_modified(8) ‖ unknown(2) ‖ protection_type(2)
        // ‖ nested datums (at 0x1C). Password protectors only (recovery/TPM VMKs
        // simply won't match the passphrase anyway).
        let protection = u16at(d.data, 0x1A) as u16;
        if protection != PROTECT_PASSWORD {
            continue;
        }
        let nested = &d.data[0x1C..];

        // Salt from the stretch-key datum; encrypted VMK from the AES-CCM datum.
        let mut stretch = Vec::new();
        find_datums_typed(nested, DATUM_STRETCH_KEY, &mut stretch);
        let mut ccm = Vec::new();
        find_datums(nested, DATUM_AES_CCM, &mut ccm);
        let (Some(stretch_data), Some(ccm_data)) = (stretch.first(), ccm.first()) else {
            continue;
        };
        if stretch_data.len() < 0x14 {
            continue;
        }
        let salt = &stretch_data[0x04..0x14];
        let key = stretch_key(&utf16le(password), salt, iterations);

        match unwrap_ccm_key(&key[..], ccm_data) {
            // The unwrapped key datum is header(8) ‖ algorithm(4) ‖ VMK.
            Ok(k) if k.len() >= 12 + 16 => {
                vmk = Some(Zeroizing::new(k[12..].to_vec()));
                break;
            }
            _ => continue, // wrong password for this VMK — try the next
        }
    }
    let vmk = vmk.ok_or_else(|| bad("passphrase did not unlock any password VMK"))?;

    // --- Recover the FVEK: the top-level AES-CCM datum, unwrapped with the VMK ---
    let mut fvek_ccm = Vec::new();
    for d in datums(entries) {
        if d.value_type == DATUM_AES_CCM {
            fvek_ccm.push(d.data);
        }
    }
    let fvek_blob = fvek_ccm.first().ok_or_else(|| bad("no FVEK datum in the dataset"))?;
    let fvek_key = unwrap_ccm_key(&vmk, fvek_blob)?;

    // The unwrapped FVEK key datum: header(8) ‖ algorithm(u32) ‖ key bytes.
    if fvek_key.len() < 16 {
        return Err(bad("FVEK key datum too small"));
    }
    let algorithm = u16at(&fvek_key, 8) as u16;
    let fvek = Zeroizing::new(fvek_key[12..].to_vec());
    let expected = match algorithm {
        ALG_AES_XTS_128 => 32,
        ALG_AES_XTS_256 => 64,
        a => {
            return Err(bad(format!(
                "unsupported volume cipher {a:#x} (only AES-XTS 128/256)"
            )))
        }
    };
    if fvek.len() < expected {
        return Err(bad("FVEK shorter than the cipher requires"));
    }

    Ok(BitLockerParams {
        fvek: Zeroizing::new(fvek[..expected].to_vec()),
        algorithm,
        sector_size,
        encrypted_size,
        nb_backup_sectors: nb_backup_sectors as u64,
        boot_backup_addr,
        kind: VolumeEncryptionKind::BitLocker,
    })
}

/// Collect datums by `value_type` at the *current* nesting level only.
fn find_datums_typed<'a>(buf: &'a [u8], value_type: u16, out: &mut Vec<&'a [u8]>) {
    for d in datums(buf) {
        if d.value_type == value_type {
            out.push(d.data);
        }
    }
}

/// Unwrap an AES-CCM datum (`nonce(12) ‖ mac(16) ‖ payload`) with `key`, and
/// return the inner key datum's raw bytes (the plaintext datum).
fn unwrap_ccm_key(key: &[u8], ccm: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    if ccm.len() < 12 + 16 + 8 {
        return Err(bad("AES-CCM datum too small"));
    }
    let nonce = &ccm[0..12];
    let mac = &ccm[12..28];
    let payload = &ccm[28..];
    let plain = aes_ccm_decrypt(key, nonce, &[], payload, mac)?;
    // The plaintext is itself a datum; sanity-check its header value type.
    if plain.len() >= 8 && u16at(&plain, 4) as u16 != DATUM_KEY {
        return Err(bad("unwrapped datum is not a key"));
    }
    Ok(plain)
}

/// Decrypt sector 0 through the recovered parameters and require a plausible
/// NTFS boot sector — a strong guard against a wrong key or unhandled layout.
fn verify_ntfs(vol: &mut dyn RawVolume, params: BitLockerParams) -> Result<BitLockerParams> {
    let ss = params.sector_size as usize;
    // Sector 0's ciphertext lives in the relocated boot region when present.
    let src = if params.nb_backup_sectors > 0 {
        params.boot_backup_addr
    } else {
        0
    };
    let mut sector = read_at(vol, src, ss)?;
    params.decrypt(0, &mut sector)?;
    if &sector[3..11] != b"NTFS    " || sector[510] != 0x55 || sector[511] != 0xAA {
        return Err(bad(
            "decrypted sector 0 is not an NTFS boot sector — wrong key or unhandled layout",
        ));
    }
    Ok(params)
}

// ---------------------------------------------------------------------------
// Decrypting view
// ---------------------------------------------------------------------------

/// A decrypting view over a BitLocker volume: offset 0 is the first plaintext
/// byte. Relocated boot sectors are read from their backup address; everything
/// else is decrypted in place with the FVEK. (The FVE metadata regions are not
/// remapped — reads there are best-effort; see the module note.)
pub struct BitLockerVolume {
    inner: Box<dyn RawVolume + Send>,
    params: BitLockerParams,
    length: u64,
}

impl BitLockerVolume {
    pub fn new(inner: Box<dyn RawVolume + Send>, params: BitLockerParams) -> Self {
        let length = params.encrypted_size.min(inner.len());
        Self { inner, params, length }
    }

    pub fn params(&self) -> &BitLockerParams {
        &self.params
    }

    /// Device byte offset holding the ciphertext of virtual sector `s`.
    fn source_offset(&self, s: u64) -> u64 {
        let ss = self.params.sector_size as u64;
        if s < self.params.nb_backup_sectors {
            self.params.boot_backup_addr + s * ss
        } else {
            s * ss
        }
    }
}

impl RawVolume for BitLockerVolume {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let ss = self.params.sector_size as u64;
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| bad("read range overflows"))?;
        if end > self.length {
            return Err(bad(format!(
                "read of {} bytes at {offset} exceeds volume {}",
                buf.len(),
                self.length
            )));
        }

        // Decrypt sector by sector so relocated boot sectors can be sourced
        // from the backup region (their ciphertext is not at `offset`).
        let first = offset / ss;
        let last = (end - 1) / ss;
        for s in first..=last {
            let mut sector = vec![0u8; ss as usize];
            self.inner.read_at(self.source_offset(s), &mut sector)?;
            self.params.decrypt(s, &mut sector)?;

            let sec_start = s * ss;
            let lo = offset.max(sec_start);
            let hi = end.min(sec_start + ss);
            let dst = (lo - offset) as usize;
            let src = (lo - sec_start) as usize;
            buf[dst..dst + (hi - lo) as usize]
                .copy_from_slice(&sector[src..src + (hi - lo) as usize]);
        }
        Ok(())
    }

    fn len(&self) -> u64 {
        self.length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemVolume(Vec<u8>);
    impl RawVolume for MemVolume {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let (s, e) = (offset as usize, offset as usize + buf.len());
            if e > self.0.len() {
                return Err(bad("read past end"));
            }
            buf.copy_from_slice(&self.0[s..e]);
            Ok(())
        }
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
    }

    fn hexvec(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    /// RFC 3610 Packet Vector #1 — a known-answer test for AES-CCM decryption
    /// (AES-128, 13-byte nonce, 8-byte AAD, 8-byte tag).
    #[test]
    fn ccm_matches_rfc3610_vector1() {
        let key = hexvec("C0 C1 C2 C3 C4 C5 C6 C7 C8 C9 CA CB CC CD CE CF");
        let nonce = hexvec("00 00 00 03 02 01 00 A0 A1 A2 A3 A4 A5");
        let aad = hexvec("00 01 02 03 04 05 06 07");
        let ct = hexvec(
            "58 8C 97 9A 61 C6 63 D2 F0 66 D0 C2 C0 F9 89 80 6D 5F 6B 61 DA C3 84",
        );
        let tag = hexvec("17 E8 D1 2C FD F9 26 E0");
        let want = hexvec(
            "08 09 0A 0B 0C 0D 0E 0F 10 11 12 13 14 15 16 17 18 19 1A 1B 1C 1D 1E",
        );
        let got = aes_ccm_decrypt(&key, &nonce, &aad, &ct, &tag).unwrap();
        assert_eq!(&got[..], &want[..]);

        // A flipped tag byte must fail authentication.
        let mut bad_tag = tag.clone();
        bad_tag[0] ^= 1;
        assert!(aes_ccm_decrypt(&key, &nonce, &aad, &ct, &bad_tag).is_err());
    }

    /// Self-consistent CCM round-trip with a 12-byte nonce and 16-byte tag —
    /// exactly BitLocker's parameters, and no AAD.
    #[test]
    fn ccm_roundtrip_bitlocker_params() {
        // Encrypt with our own primitives, then decrypt must recover the input.
        let key = [0x42u8; 32];
        let nonce = [7u8; 12];
        let plain: Vec<u8> = (0..44u8).collect();

        // Build ciphertext + tag using the same CCM math (encrypt path).
        let cipher = Aes::new(&key).unwrap();
        let l = 3usize;
        let ctr_block = |counter: u64| {
            let mut b = [0u8; 16];
            b[0] = (l - 1) as u8;
            b[1..13].copy_from_slice(&nonce);
            b[13..].copy_from_slice(&counter.to_be_bytes()[5..]);
            b
        };
        let mut s0 = ctr_block(0);
        cipher.encrypt_block(&mut s0);
        let mut ct = vec![0u8; plain.len()];
        for (i, chunk) in plain.chunks(16).enumerate() {
            let mut ks = ctr_block(i as u64 + 1);
            cipher.encrypt_block(&mut ks);
            for (j, &p) in chunk.iter().enumerate() {
                ct[i * 16 + j] = p ^ ks[j];
            }
        }
        let mut x = [0u8; 16];
        x[0] = ((16u8 - 2) / 2) << 3 | (l as u8 - 1);
        x[1..13].copy_from_slice(&nonce);
        x[13..].copy_from_slice(&(plain.len() as u64).to_be_bytes()[5..]);
        cipher.encrypt_block(&mut x);
        for block in plain.chunks(16) {
            let mut padded = [0u8; 16];
            padded[..block.len()].copy_from_slice(block);
            for (xi, bi) in x.iter_mut().zip(padded.iter()) {
                *xi ^= bi;
            }
            cipher.encrypt_block(&mut x);
        }
        let tag: Vec<u8> = (0..16).map(|i| x[i] ^ s0[i]).collect();

        let got = aes_ccm_decrypt(&key, &nonce, &[], &ct, &tag).unwrap();
        assert_eq!(&got[..], &plain[..]);
    }

    #[test]
    fn stretch_key_is_deterministic() {
        let a = stretch_key(&utf16le("hunter2"), &[3u8; 16], 64);
        let b = stretch_key(&utf16le("hunter2"), &[3u8; 16], 64);
        let c = stretch_key(&utf16le("hunter3"), &[3u8; 16], 64);
        assert_eq!(&a[..], &b[..]);
        assert_ne!(&a[..], &c[..]);
    }

    // --- Helpers to build a synthetic BitLocker volume for an end-to-end test.

    fn put_u16(b: &mut [u8], o: usize, v: u16) {
        b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u32(b: &mut [u8], o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u64(b: &mut [u8], o: usize, v: u64) {
        b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Encrypt a key datum (algorithm ‖ key) with AES-CCM under `key`, producing
    /// the on-disk `nonce ‖ mac ‖ payload` bytes.
    fn ccm_encrypt_keydatum(key: &[u8], nonce: &[u8; 12], algorithm: u16, keybytes: &[u8]) -> Vec<u8> {
        // Plaintext datum: size(u16) type(u16) valuetype(u16=KEY) ver(u16)
        //                  algorithm(u32) key...
        let mut plain = vec![0u8; 12 + keybytes.len()];
        let plain_len = plain.len() as u16;
        put_u16(&mut plain, 0, plain_len);
        put_u16(&mut plain, 4, DATUM_KEY);
        put_u32(&mut plain, 8, algorithm as u32);
        plain[12..].copy_from_slice(keybytes);

        let cipher = Aes::new(key).unwrap();
        let l = 3usize;
        let ctr_block = |counter: u64| {
            let mut b = [0u8; 16];
            b[0] = (l - 1) as u8;
            b[1..13].copy_from_slice(nonce);
            b[13..].copy_from_slice(&counter.to_be_bytes()[5..]);
            b
        };
        let mut s0 = ctr_block(0);
        cipher.encrypt_block(&mut s0);
        let mut ct = vec![0u8; plain.len()];
        for (i, chunk) in plain.chunks(16).enumerate() {
            let mut ks = ctr_block(i as u64 + 1);
            cipher.encrypt_block(&mut ks);
            for (j, &p) in chunk.iter().enumerate() {
                ct[i * 16 + j] = p ^ ks[j];
            }
        }
        let mut x = [0u8; 16];
        x[0] = ((16u8 - 2) / 2) << 3 | (l as u8 - 1);
        x[1..13].copy_from_slice(nonce);
        x[13..].copy_from_slice(&(plain.len() as u64).to_be_bytes()[5..]);
        cipher.encrypt_block(&mut x);
        for block in plain.chunks(16) {
            let mut padded = [0u8; 16];
            padded[..block.len()].copy_from_slice(block);
            for (xi, bi) in x.iter_mut().zip(padded.iter()) {
                *xi ^= bi;
            }
            cipher.encrypt_block(&mut x);
        }
        let mut out = Vec::new();
        out.extend_from_slice(nonce);
        out.extend((0..16).map(|i| x[i] ^ s0[i]));
        out.extend_from_slice(&ct);
        out
    }

    /// Build a tiny XTS-128 BitLocker volume protected by `password`, encrypting
    /// a fake NTFS boot sector, and prove the full pipeline unlocks + decrypts.
    #[test]
    fn end_to_end_unlock_and_decrypt_xts128() {
        let password = "s3cret";
        let salt = [0x11u8; 16];
        let stretch_iters = 64u32; // fast; real volumes use 2^20
        let vmk = [0x55u8; 32];
        let fvek: Vec<u8> = (0..32u8).map(|i| i.wrapping_mul(7).wrapping_add(1)).collect();
        let sector_size = 512usize;

        // Stretched key from the password — CCM-wrap the VMK with it.
        let stretched = stretch_key(&utf16le(password), &salt, stretch_iters);
        let vmk_ccm = ccm_encrypt_keydatum(&stretched[..], &[1u8; 12], 0, &vmk);
        // CCM-wrap the FVEK with the VMK.
        let fvek_ccm = ccm_encrypt_keydatum(&vmk, &[2u8; 12], ALG_AES_XTS_128, &fvek);

        // --- Assemble the dataset entries ---
        // Stretch-key datum: header(8) algorithm(u32) salt(16) = 28 bytes.
        let mut stretch = vec![0u8; 28];
        put_u16(&mut stretch, 0, 28);
        put_u16(&mut stretch, 4, DATUM_STRETCH_KEY);
        stretch[12..28].copy_from_slice(&salt);
        // AES-CCM datum wrapping the VMK.
        let mut vmk_ccm_datum = vec![0u8; 8 + vmk_ccm.len()];
        let n = vmk_ccm_datum.len() as u16;
        put_u16(&mut vmk_ccm_datum, 0, n);
        put_u16(&mut vmk_ccm_datum, 4, DATUM_AES_CCM);
        vmk_ccm_datum[8..].copy_from_slice(&vmk_ccm);
        // VMK datum: header(8) guid(16) time(8) unk(2) protection(2) nested...
        let nested_len = stretch.len() + vmk_ccm_datum.len();
        let mut vmk_datum = vec![0u8; 8 + 28 + nested_len];
        let n = vmk_datum.len() as u16;
        put_u16(&mut vmk_datum, 0, n);
        put_u16(&mut vmk_datum, 2, 0x0002); // entry_type = VMK
        put_u16(&mut vmk_datum, 4, DATUM_VMK);
        put_u16(&mut vmk_datum, 8 + 0x1A, PROTECT_PASSWORD); // protection_type
        vmk_datum[8 + 28..8 + 28 + stretch.len()].copy_from_slice(&stretch);
        vmk_datum[8 + 28 + stretch.len()..].copy_from_slice(&vmk_ccm_datum);
        // FVEK datum: top-level AES-CCM.
        let mut fvek_datum = vec![0u8; 8 + fvek_ccm.len()];
        let n = fvek_datum.len() as u16;
        put_u16(&mut fvek_datum, 0, n);
        put_u16(&mut fvek_datum, 2, 0x0003); // entry_type = FVEK
        put_u16(&mut fvek_datum, 4, DATUM_AES_CCM);
        fvek_datum[8..].copy_from_slice(&fvek_ccm);

        let mut entries = Vec::new();
        entries.extend_from_slice(&vmk_datum);
        entries.extend_from_slice(&fvek_datum);

        // Dataset: 0x30-byte header then the entries.
        let dataset_size = 0x30 + entries.len();
        let mut dataset = vec![0u8; dataset_size];
        put_u32(&mut dataset, 0x00, dataset_size as u32);
        put_u32(&mut dataset, 0x08, 0x30);
        put_u16(&mut dataset, 0x24, ALG_AES_XTS_128);
        dataset[0x30..].copy_from_slice(&entries);

        // FVE metadata block: 0x40-byte info head then the dataset.
        let encrypted_size = 16 * 1024u64; // plaintext region
        let info_off = 64 * 1024u64;
        let mut info = vec![0u8; 0x40 + dataset.len()];
        info[0..8].copy_from_slice(FVE_SIGNATURE);
        put_u64(&mut info, 0x10, encrypted_size);
        put_u32(&mut info, 0x1C, 1); // nb_backup_sectors = 1 (virtual sector 0 relocated)
        info[0x40..].copy_from_slice(&dataset);
        // The relocated boot sector lives just past the metadata block.
        let boot_backup_addr = (info_off + info.len() as u64).next_multiple_of(512);
        put_u64(&mut info, 0x38, boot_backup_addr);

        // Plaintext: NTFS boot at virtual sector 0, marker data at sector 1.
        let mut plain = vec![0u8; encrypted_size as usize];
        plain[3..11].copy_from_slice(b"NTFS    ");
        put_u16(&mut plain, 0x0B, 512);
        plain[510] = 0x55;
        plain[511] = 0xAA;
        for (i, b) in plain[512..1024].iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(3).wrapping_add(9);
        }
        // Encrypt the whole region with XTS-128 (tweak = virtual sector index).
        let c1 = Aes128::new(GenericArray::from_slice(&fvek[..16]));
        let c2 = Aes128::new(GenericArray::from_slice(&fvek[16..32]));
        let mut enc = plain.clone();
        Xts128::new(c1, c2).encrypt_area(&mut enc, sector_size, 0, |i| i.to_le_bytes());

        // --- Device: BitLocker boot sector at 0; virtual sectors 1.. in place;
        // virtual sector 0's ciphertext relocated to boot_backup_addr. ---
        let dev_len = (boot_backup_addr + 512) as usize;
        let mut dev = vec![0u8; dev_len];
        dev[512..encrypted_size as usize].copy_from_slice(&enc[512..encrypted_size as usize]);
        dev[boot_backup_addr as usize..boot_backup_addr as usize + 512]
            .copy_from_slice(&enc[0..512]);

        // BitLocker boot sector at device offset 0 (its own boot code + pointers).
        dev[3..11].copy_from_slice(FVE_SIGNATURE);
        put_u16(&mut dev, 0x0B, 512);
        put_u64(&mut dev, INFO_OFFSETS_AT, info_off);
        // Metadata block.
        dev[info_off as usize..info_off as usize + info.len()].copy_from_slice(&info);

        // --- Unlock ---
        // Use a small stretch count by temporarily shadowing: open_bitlocker
        // uses STRETCH_ITERATIONS, so drive the parser directly with the same
        // metadata but the test's iteration count via unlock helper is not
        // exposed; instead assert the crypto pipeline via the public path with
        // matching iterations.
        let mut vol = MemVolume(dev);

        // --- What the volume says about itself, before any password ---
        let info = inspect_bitlocker(&mut vol).unwrap();
        assert_eq!(info.cipher, ALG_AES_XTS_128);
        assert!(info.cipher_supported);
        assert_eq!(info.encrypted_size, encrypted_size);
        assert_eq!(info.sector_size, 512);
        assert_eq!(
            info.protectors.iter().map(|p| p.kind).collect::<Vec<_>>(),
            vec![PROTECT_PASSWORD],
            "the volume has exactly one way in, and it is a passphrase"
        );
        assert!(info.has_password() && info.openable());
        assert!(!info.header_hex.is_empty() && !info.dataset_hex.is_empty());

        let params = super::open_bitlocker_with_iters(&mut vol, password, stretch_iters).unwrap();
        assert_eq!(params.algorithm, ALG_AES_XTS_128);
        assert_eq!(&params.fvek[..], &fvek[..]);

        // Decrypt sector 1 through the view and compare to the known plaintext.
        let mut view = BitLockerVolume::new(Box::new(MemVolume(vol.0)), params);
        let mut got = vec![0u8; 512];
        view.read_at(512, &mut got).unwrap();
        assert_eq!(got, &plain[512..1024]);

        // Wrong password fails.
        let mut vol2 = MemVolume(view.inner_into_vec());
        assert!(super::open_bitlocker_with_iters(&mut vol2, "nope", stretch_iters).is_err());
    }

    impl BitLockerVolume {
        fn inner_into_vec(self) -> Vec<u8> {
            // Test-only: recover the backing bytes.
            let mut buf = vec![0u8; self.inner.len() as usize];
            let mut inner = self.inner;
            inner.read_at(0, &mut buf).unwrap();
            buf
        }
    }

    #[test]
    fn rejects_non_bitlocker() {
        let mut vol = MemVolume(vec![0u8; 4096]);
        assert!(open_bitlocker(&mut vol, "x").is_err());
    }
}
