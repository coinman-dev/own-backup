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
/// Boot-sector offset of the three FVE metadata block byte offsets (u64 each),
/// in a volume whose header still looks like NTFS: the BitLocker GUID sits at
/// 0x160 and the offsets follow it.
const INFO_OFFSETS_NTFS: usize = 0x170;

/// The same three offsets in a volume whose header is a FAT-shaped BPB with
/// `-FVE-FS-` as its OEM id — the original boot sector having been moved aside.
///
/// **Found by dumping real volumes (2026-08-05).** Every test in this file
/// builds its own volume, and every one of them put the offsets at
/// [`INFO_OFFSETS_NTFS`] because that is where the parser looked; the first
/// four real volumes we ever pointed it at — Windows 10, XTS-AES 128, "encrypt
/// used space only" — all carry this header instead, with zeros at 0x160 and
/// the offsets here. So the parser had never once been right about a real
/// disk, and its own tests could not have told us.
const INFO_OFFSETS_FVE: usize = 0x00B0;

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
/// Checked against a real machine (2026-08-05, Windows 10, four volumes): the
/// password (`0x2000`), the recovery key (`0x0800`), the external key
/// (`0x0200`) and TPM+PIN (`0x0500`) all matched what `manage-bde -status`
/// reported for the same volumes, name for name. `0x0000` (a suspended volume,
/// key in the clear) and `0x0100` (TPM alone) have still never been seen here,
/// so they keep their raw value beside the name and decide nothing.
pub const PROTECTOR_KINDS: [(u16, &str); 6] = [
    (0x0000, "защита приостановлена (ключ лежит открыто)"),
    (0x0100, "TPM"),
    (0x0200, "внешний ключ (авторазблокировка / ключ запуска)"),
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
    stretch_hash(&Sha256::digest(h1), salt, iterations)
}

/// The stretch itself, given the hash the secret boils down to.
///
/// A user passphrase reaches it as `SHA-256(SHA-256(utf16))`; a recovery key
/// as the hash of its sixteen bytes. Everything after that is identical, and
/// it is the expensive part: 2^20 rounds.
fn stretch_hash(password_hash: &[u8], salt: &[u8], iterations: u32) -> Zeroizing<[u8; 32]> {
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

/// VMK protection type of the 48-digit recovery key.
const PROTECT_RECOVERY: u16 = 0x0800;

/// The sixteen bytes behind a `123456-…-654321` recovery key.
///
/// Eight groups of six digits. Each group is a 16-bit number multiplied by 11
/// — that multiple is the format's own checksum, and a group that fails it is
/// a typo, which is worth saying before a minute of key stretching rather than
/// after. The eight numbers are the key, little-endian.
pub fn parse_recovery_key(text: &str) -> Result<Zeroizing<[u8; 16]>> {
    let split: Vec<&str> = text
        .split(['-', ' ', '\t'])
        .map(|g| g.trim())
        .filter(|g| !g.is_empty())
        .collect();
    // Forty-eight digits in a row are the same key: that is how it arrives when
    // it is copied out of a field, read off a phone, or typed by someone who
    // did not think the dashes were part of it. Regrouping is not guessing —
    // the group checksums below still have to hold.
    let regrouped: Vec<String>;
    let groups: Vec<&str> = if split.len() == 1
        && split[0].len() == 48
        && split[0].bytes().all(|b| b.is_ascii_digit())
    {
        regrouped = split[0].as_bytes().chunks(6).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
        regrouped.iter().map(String::as_str).collect()
    } else {
        split
    };
    if groups.len() != 8 {
        return Err(bad(format!(
            "ключ восстановления — 8 групп по 6 цифр, а здесь {}",
            groups.len()
        )));
    }
    let mut out = Zeroizing::new([0u8; 16]);
    for (i, g) in groups.iter().enumerate() {
        if g.len() != 6 || !g.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad(format!("группа {} — не шесть цифр: {g:?}", i + 1)));
        }
        let n: u32 = g.parse().map_err(|_| bad(format!("группа {} не читается", i + 1)))?;
        if n % 11 != 0 {
            return Err(bad(format!(
                "группа {} ({g}) не делится на 11 — опечатка в ключе",
                i + 1
            )));
        }
        let value = n / 11;
        if value > u16::MAX as u32 {
            return Err(bad(format!("группа {} ({g}) вне диапазона", i + 1)));
        }
        out[i * 2..i * 2 + 2].copy_from_slice(&(value as u16).to_le_bytes());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// FVE metadata / datum parsing
// ---------------------------------------------------------------------------

/// Where the three FVE metadata copies might be, best first.
///
/// Both header layouts are read rather than chosen between: a value that is
/// zero, unaligned or outside the volume is not an offset, and what survives
/// that is tried until one parses. Choosing by signature would work today and
/// would break on the next variant Microsoft ships — this cannot.
fn metadata_offsets(boot: &[u8], volume_len: u64) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    for base in [INFO_OFFSETS_FVE, INFO_OFFSETS_NTFS] {
        for i in 0..3 {
            let at = base + i * 8;
            if at + 8 > boot.len() {
                continue;
            }
            let off = u64at(boot, at);
            // A metadata block is sector-aligned, inside the volume, and not
            // at the very start (that is the boot sector itself).
            if off >= 512 && off % 512 == 0 && off + 0x70 <= volume_len && !out.contains(&off) {
                out.push(off);
            }
        }
    }
    out
}

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
    /// How many VMK datums of this kind the volume carries. A machine with
    /// auto-unlock collects external keys — seven of them on the disk this was
    /// written against — and one line saying "×7" matches what `manage-bde`
    /// lists, while one saying nothing does not.
    pub count: u32,
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

    let offsets = metadata_offsets(&boot, vol.len());
    let mut last_err = String::new();
    for (i, &info_off) in offsets.iter().enumerate() {
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
        if offsets.is_empty() {
            "ни одного правдоподобного".to_string()
        } else {
            offsets.iter().map(|o| format!("{o:#x}")).collect::<Vec<_>>().join(", ")
        },
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
        match protectors.iter_mut().find(|p: &&mut Protector| p.kind == kind) {
            Some(seen) => seen.count += 1,
            None => protectors.push(Protector {
                kind,
                name: known.map(|(_, n)| (*n).to_string()).unwrap_or_else(|| "неизвестно".into()),
                // What the user can type and we can turn into a VMK: the
                // passphrase and the 48-digit recovery key, both stretched in
                // software (see `open_bitlocker`). The recovery key belongs
                // here as much as the passphrase does — and on a SYSTEM volume
                // it is usually the only one of the two, because Windows does
                // not allow a passphrase protector there at all. Leaving it out
                // made the wizard tell such a volume "открыть нечем: TPM + PIN,
                // ключ восстановления (48 цифр)" — naming, in the same
                // sentence, the thing that would have worked.
                usable: matches!(kind, PROTECT_PASSWORD | PROTECT_RECOVERY) && cipher_supported,
                count: 1,
            }),
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

/// The shape of every VMK datum in the metadata, in words.
///
/// Types and sizes only — no salts, no wrapped keys, nothing that would matter
/// if it were pasted into a chat. It exists because "passphrase did not unlock
/// any password VMK" has two very different meanings — the password was wrong,
/// or we are reading the datums wrongly — and on a volume the parser has never
/// successfully opened, the second is at least as likely as the first.
pub fn describe_vmks(vol: &mut dyn RawVolume) -> Result<Vec<String>> {
    let boot = read_at(vol, 0, 512)?;
    if !is_bitlocker(&boot) {
        return Err(bad("not a BitLocker volume"));
    }
    let mut out = Vec::new();
    for info_off in metadata_offsets(&boot, vol.len()) {
        let head = read_at(vol, info_off, 0x70)?;
        if &head[0..8] != FVE_SIGNATURE {
            continue;
        }
        let dataset_size = u32at(&head, 0x40);
        if !(0x30..=16 * 1024 * 1024).contains(&dataset_size) {
            continue;
        }
        let dataset = read_at(vol, info_off + 0x40, dataset_size as usize)?;
        for d in datums(&dataset[0x30..]) {
            if d.value_type != DATUM_VMK || d.data.len() < 0x1C {
                continue;
            }
            let protection = u16at(d.data, 0x1A) as u16;
            out.push(format!(
                "VMK: тип входа {:#06x}, защита {protection:#06x}, данных {} байт",
                d.entry_type,
                d.data.len()
            ));
            describe_datums(&d.data[0x1C..], 1, &mut out);
        }
        return Ok(out); // one copy is enough; they are duplicates
    }
    Err(bad("метаданные FVE не прочитаны"))
}

fn describe_datums(buf: &[u8], depth: usize, out: &mut Vec<String>) {
    for d in datums(buf) {
        let kind = match d.value_type {
            DATUM_KEY => "ключ",
            DATUM_STRETCH_KEY => "растяжка",
            DATUM_AES_CCM => "AES-CCM",
            DATUM_VMK => "VMK",
            0x0002 => "UTF-16 строка",
            0x000F => "смещение+размер",
            _ => "?",
        };
        // The first four bytes of a datum are a method or a counter, never a
        // key: enough to tell a stretch datum's algorithm without exposing one.
        let head4 = if d.data.len() >= 4 { hex(&d.data[..4]) } else { String::new() };
        out.push(format!(
            "{:indent$}тип {:#06x} ({kind}), вход {:#06x}, данных {} байт, первые 4: {head4}",
            "",
            d.value_type,
            d.entry_type,
            d.data.len(),
            indent = depth * 2
        ));
        if depth < 4 {
            let inner = match d.value_type {
                DATUM_VMK if d.data.len() > 0x1C => Some(&d.data[0x1C..]),
                DATUM_STRETCH_KEY if d.data.len() > 0x14 => Some(&d.data[0x14..]),
                _ => None,
            };
            if let Some(inner) = inner {
                describe_datums(inner, depth + 1, out);
            }
        }
    }
}

/// Recover the FVEK of a BitLocker volume using `password`. Errors on a wrong
/// passphrase, an unsupported cipher, or a malformed / unhandled layout.
pub fn open_bitlocker(vol: &mut dyn RawVolume, password: &str) -> Result<BitLockerParams> {
    open_bitlocker_with_iters(vol, password, STRETCH_ITERATIONS, true)
}

/// [`open_bitlocker`] without the "and it decrypts to NTFS" check.
///
/// For diagnosis only: a key that unwrapped cleanly (AES-CCM verifies its own
/// tag, so the passphrase was right) but decrypts to something unrecognisable
/// means the LAYOUT is misunderstood, not the key — and telling those two apart
/// needs the key in hand. Never use it to read data.
pub fn open_bitlocker_unverified(
    vol: &mut dyn RawVolume,
    password: &str,
) -> Result<BitLockerParams> {
    open_bitlocker_with_iters(vol, password, STRETCH_ITERATIONS, false)
}

/// [`open_bitlocker`] with an explicit stretch iteration count (tests use a
/// small one; production always uses [`STRETCH_ITERATIONS`]).
fn open_bitlocker_with_iters(
    vol: &mut dyn RawVolume,
    password: &str,
    iterations: u32,
    verify: bool,
) -> Result<BitLockerParams> {
    let boot = read_at(vol, 0, 512)?;
    if !is_bitlocker(&boot) {
        return Err(bad("not a BitLocker volume"));
    }
    let sector_size = u16at(&boot, 0x0B);
    if !sector_size.is_power_of_two() || !(512..=4096).contains(&sector_size) {
        return Err(bad(format!("implausible sector size {sector_size}")));
    }

    // The FVE metadata copies — try each until one parses and unlocks.
    let offsets = metadata_offsets(&boot, vol.len());
    let mut last_err = bad(format!(
        "метаданные FVE не найдены (проверено адресов: {})",
        offsets.len()
    ));
    for info_off in offsets {
        match unlock_from_metadata(vol, info_off, sector_size as u32, password, iterations) {
            Ok(params) => {
                if verify {
                    verify_covers_volume(&params, vol.len())?;
                    return verify_ntfs(vol, params);
                }
                return Ok(params);
            }
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

    // --- Recover the VMK from whatever the user typed ---
    //
    // A 48-digit recovery key and a passphrase reach the same stretch by
    // different doors: the key's sixteen bytes are hashed once, a passphrase's
    // UTF-16 twice. Which VMK datums are worth trying follows from which of
    // the two the input is.
    let recovery = parse_recovery_key(password).ok();
    let mut vmk: Option<Zeroizing<Vec<u8>>> = None;
    for d in datums(entries) {
        if d.entry_type != 0x0002 || d.value_type != DATUM_VMK || d.data.len() < 28 {
            continue;
        }
        // VMK head: guid(16) ‖ last_modified(8) ‖ unknown(2) ‖ protection_type(2)
        // ‖ nested datums (at 0x1C).
        let protection = u16at(d.data, 0x1A) as u16;
        let wanted = if recovery.is_some() { PROTECT_RECOVERY } else { PROTECT_PASSWORD };
        if protection != wanted {
            continue;
        }
        let nested = &d.data[0x1C..];

        // Salt from a stretch-key datum, wrapped VMK from an AES-CCM datum —
        // and EVERY pair is tried, not the first of each. Which of the two
        // comes first, and whether the AES-CCM datum is the stretch datum's
        // sibling or lives inside it, is not something to assume about a
        // format we cannot see; the stretch is computed once per distinct salt
        // (it is the expensive half — 2^20 rounds of SHA-256) and each wrapped
        // key is tried against it.
        let mut stretch = Vec::new();
        find_datums(nested, DATUM_STRETCH_KEY, &mut stretch);
        let mut ccm = Vec::new();
        find_datums(nested, DATUM_AES_CCM, &mut ccm);

        'salts: for stretch_data in &stretch {
            if stretch_data.len() < 0x14 {
                continue;
            }
            let salt = &stretch_data[0x04..0x14];
            // A recovery key is hashed with ONE round of SHA-256 — measured, not
            // guessed: on 2026-08-19 a 128 GiB system volume opened by its
            // 48-digit key and the log said `protector="ключ восстановления,
            // один SHA-256"`. Until then both readings were tried, because the
            // sources disagree and only the AES-CCM tag could settle it. It has;
            // the second candidate is gone, and with it a million rounds of
            // SHA-256 on every unlock.
            let candidates: Vec<(&str, Zeroizing<[u8; 32]>)> = match &recovery {
                Some(bytes) => vec![(
                    "ключ восстановления, один SHA-256",
                    stretch_hash(&Sha256::digest(&bytes[..]), salt, iterations),
                )],
                None => vec![("пароль", stretch_key(&utf16le(password), salt, iterations))],
            };
            for (how, key) in &candidates {
                for ccm_data in &ccm {
                    // The unwrapped key datum is header(8) ‖ algorithm(4) ‖ VMK.
                    if let Ok(k) = unwrap_ccm_key(&key[..], ccm_data) {
                        if k.len() >= 12 + 16 {
                            tracing::info!(protector = %how, "BitLocker VMK unwrapped");
                            vmk = Some(Zeroizing::new(k[12..].to_vec()));
                            break 'salts;
                        }
                    }
                }
            }
        }
        if vmk.is_some() {
            break;
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

/// With the key in hand, ask the volume where its first sector really is.
///
/// The metadata says the first `nb_backup_sectors` were moved to
/// `boot_backup_addr`, but not what tweak they were encrypted under: their
/// virtual position (0…15) or their physical one. Both are plausible, the
/// difference is invisible until it is tried, and our own tests cannot answer
/// it — they encrypt what they then decrypt.
///
/// So each way is tried and the one that yields an NTFS boot sector wins. Only
/// the first sixteen bytes are shown: a boot sector's signature area, which is
/// the same on every NTFS volume in the world.
pub fn diagnose_first_sector(vol: &mut dyn RawVolume, params: &BitLockerParams) -> Vec<String> {
    let ss = params.sector_size as usize;
    let backup = params.boot_backup_addr;
    let phys_index = backup / params.sector_size.max(1) as u64;

    let looks_like_ntfs =
        |b: &[u8]| b.len() >= 11 && &b[3..11] == b"NTFS    ";

    let mut out = Vec::new();
    let mut attempt = |label: &str, at: u64, tweak: Option<u64>, out: &mut Vec<String>| {
        let mut buf = match read_at(vol, at, ss) {
            Ok(b) => b,
            Err(e) => {
                out.push(format!("{label}: прочитать не удалось — {e}"));
                return;
            }
        };
        if let Some(t) = tweak {
            if let Err(e) = params.decrypt(t, &mut buf) {
                out.push(format!("{label}: расшифровать не удалось — {e}"));
                return;
            }
        }
        out.push(format!(
            "{label}: {}{}",
            hex(&buf[..16.min(buf.len())]),
            if looks_like_ntfs(&buf) { "   ← NTFS!" } else { "" }
        ));
    };

    attempt("как есть, со смещения 0 (без расшифровки)", 0, None, &mut out);
    attempt(
        &format!("как есть, с {backup} (без расшифровки)"),
        backup,
        None,
        &mut out,
    );
    attempt("с 0, ключ сектора 0", 0, Some(0), &mut out);
    attempt(
        &format!("с {backup}, ключ сектора 0"),
        backup,
        Some(0),
        &mut out,
    );
    attempt(
        &format!("с {backup}, ключ сектора {phys_index} (физического)"),
        backup,
        Some(phys_index),
        &mut out,
    );
    out
}

/// Is the whole volume actually encrypted?
///
/// The image we write is `encrypted_size` long, so a volume whose encrypted
/// region is much shorter than its partition would be copied SHORT — and a
/// restore would lay down a truncated filesystem, which is worse than
/// refusing. Checked against four real volumes (2026-08-05): on every one,
/// fully converted and "encrypt used space only", the encrypted size equals
/// the partition length exactly.
///
/// The tolerance is deliberately loose. Nothing here can tell an encryption
/// still in progress from a layout we have not met — the metadata field that
/// would say so has not been identified on a real disk — so this refuses only
/// what is unmistakably wrong, and says what it saw.
fn verify_covers_volume(params: &BitLockerParams, volume_len: u64) -> Result<()> {
    if volume_len == 0 || params.encrypted_size >= volume_len / 10 * 9 {
        return Ok(());
    }
    Err(bad(format!(
        "зашифрована лишь часть тома: {} из {} байт ({:.0}%). Похоже, шифрование ещё идёт —          дождитесь его окончания (в Windows: manage-bde -status). Копировать такой том          нельзя: образ вышел бы обрезанным.",
        params.encrypted_size,
        volume_len,
        params.encrypted_size as f64 * 100.0 / volume_len as f64
    )))
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
    // Under the tweak of where it lies, not of where it belongs — see
    // `BitLockerVolume::read_at`.
    params.decrypt(src / params.sector_size.max(1) as u64, &mut sector)?;
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
            // The tweak is the sector's PHYSICAL position, not its place in the
            // volume: a relocated boot sector was encrypted where it now lies.
            // Checked on a real volume (2026-08-05) — decrypting sector 0 under
            // tweak 0 gave noise, under the tweak of its backup address gave
            // `eb 52 90 NTFS    `.
            let physical = self.source_offset(s);
            self.inner.read_at(physical, &mut sector)?;
            self.params.decrypt(physical / ss, &mut sector)?;

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

    /// The 48 digits are eight 16-bit numbers times eleven, and the times-eleven
    /// is the format's own typo check.
    #[test]
    fn a_recovery_key_is_eight_numbers_and_a_checksum() {
        // 11 → 1, 22 → 2, … little-endian pairs.
        let key = parse_recovery_key(
            "000011-000022-000033-000044-000055-000066-000077-000088",
        )
        .unwrap();
        assert_eq!(
            &key[..],
            &[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7, 0, 8, 0],
            "each group divided by eleven, little-endian"
        );

        // The largest value a group may hold: 65535 * 11 = 720885.
        let max = parse_recovery_key(
            "720885-720885-720885-720885-720885-720885-720885-720885",
        )
        .unwrap();
        assert_eq!(&max[..2], &[0xFF, 0xFF]);

        // A typo that changes one digit almost always breaks the multiple.
        let err = parse_recovery_key(
            "000012-000022-000033-000044-000055-000066-000077-000088",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("11"), "the checksum failure should say so: {err}");

        assert!(parse_recovery_key("000011-000022").is_err(), "eight groups, not two");
        assert!(parse_recovery_key("00001a-000022-000033-000044-000055-000066-000077-000088").is_err());
        // A passphrase must not be mistaken for a recovery key.
        assert!(parse_recovery_key("hunter2").is_err());

        // The same key without its dashes — copied out of a field, or read off
        // a phone by someone who did not think the dashes counted.
        let run = parse_recovery_key("000011000022000033000044000055000066000077000088").unwrap();
        assert_eq!(&run[..], &key[..], "dashes are punctuation, not data");
        // Spaces instead of dashes, and a stray leading/trailing one.
        let spaced =
            parse_recovery_key(" 000011 000022 000033 000044 000055 000066 000077 000088 ").unwrap();
        assert_eq!(&spaced[..], &key[..]);
        // Regrouping is not a licence to ignore the checksums.
        assert!(parse_recovery_key("000012000022000033000044000055000066000077000088").is_err());
        // 48 characters, but not all digits: still not a key.
        assert!(parse_recovery_key("00001a000022000033000044000055000066000077000088").is_err());
    }

    /// A volume half-encrypted would be copied half-long, and restored broken.
    #[test]
    fn a_partly_encrypted_volume_is_refused() {
        let params = BitLockerParams {
            fvek: Zeroizing::new(vec![0u8; 32]),
            algorithm: ALG_AES_XTS_128,
            sector_size: 512,
            encrypted_size: 50 * 1024,
            nb_backup_sectors: 0,
            boot_backup_addr: 0,
            kind: VolumeEncryptionKind::BitLocker,
        };
        let err = verify_covers_volume(&params, 100 * 1024).unwrap_err().to_string();
        assert!(err.contains("50"), "the message should name what was seen: {err}");

        // The real case: encrypted size equal to the partition — and a little
        // slack, because a volume is not obliged to be exact.
        assert!(verify_covers_volume(&params, 50 * 1024).is_ok());
        assert!(verify_covers_volume(&params, 52 * 1024).is_ok());
        // Nothing to compare against is not a reason to refuse.
        assert!(verify_covers_volume(&params, 0).is_ok());
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
        // Encrypt every sector under the tweak of WHERE IT LIES — the rule a
        // real volume turned out to follow (2026-08-05). Sectors 1.. stay in
        // place, so their tweak is their index; sector 0 is relocated to the
        // backup address and is encrypted under the tweak of THAT position.
        let xts = || {
            Xts128::new(
                Aes128::new(GenericArray::from_slice(&fvek[..16])),
                Aes128::new(GenericArray::from_slice(&fvek[16..32])),
            )
        };
        let mut enc = plain.clone();
        xts().encrypt_area(&mut enc, sector_size, 0, |i| i.to_le_bytes());

        let mut relocated = plain[0..512].to_vec();
        let phys0 = boot_backup_addr / 512;
        xts().encrypt_area(&mut relocated, sector_size, phys0 as u128, |i| i.to_le_bytes());

        // --- Device: BitLocker boot sector at 0; virtual sectors 1.. in place;
        // virtual sector 0's ciphertext relocated to boot_backup_addr. ---
        let dev_len = (boot_backup_addr + 512) as usize;
        let mut dev = vec![0u8; dev_len];
        dev[512..encrypted_size as usize].copy_from_slice(&enc[512..encrypted_size as usize]);
        dev[boot_backup_addr as usize..boot_backup_addr as usize + 512]
            .copy_from_slice(&relocated);

        // A real volume's encrypted region covers the WHOLE partition — the
        // metadata blocks and the relocated boot sectors live inside it, not
        // past its end (checked on four disks, 2026-08-05). Say so, or the
        // "is the volume fully encrypted" guard rightly refuses this fixture.
        put_u64(&mut info, 0x10, dev_len as u64);

        // BitLocker boot sector at device offset 0 (its own boot code + pointers).
        dev[3..11].copy_from_slice(FVE_SIGNATURE);
        put_u16(&mut dev, 0x0B, 512);
        put_u64(&mut dev, INFO_OFFSETS_NTFS, info_off);
        // Metadata block.
        dev[info_off as usize..info_off as usize + info.len()].copy_from_slice(&info);

        // The same volume as a real one is shaped: the offsets at 0xB0 and
        // nothing at 0x170. Every test here used to describe a volume that
        // only our own parser would produce, which is why four real disks in a
        // row came back "no FVE metadata" (2026-08-05).
        let mut real_layout = dev.clone();
        for i in 0..3 {
            put_u64(&mut real_layout, INFO_OFFSETS_NTFS + i * 8, 0);
        }
        put_u64(&mut real_layout, INFO_OFFSETS_FVE, info_off);

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
        assert_eq!(
            info.encrypted_size, dev_len as u64,
            "the encrypted region covers the whole volume, as it does on a real disk"
        );
        assert_eq!(info.sector_size, 512);
        assert_eq!(
            info.protectors.iter().map(|p| p.kind).collect::<Vec<_>>(),
            vec![PROTECT_PASSWORD],
            "the volume has exactly one way in, and it is a passphrase"
        );
        assert!(info.has_password() && info.openable());
        assert!(!info.header_hex.is_empty() && !info.dataset_hex.is_empty());

        let params = super::open_bitlocker_with_iters(&mut vol, password, stretch_iters, true).unwrap();
        assert_eq!(params.algorithm, ALG_AES_XTS_128);
        assert_eq!(&params.fvek[..], &fvek[..]);

        // Decrypt sector 1 through the view and compare to the known plaintext.
        let mut view = BitLockerVolume::new(Box::new(MemVolume(vol.0)), params);
        let mut got = vec![0u8; 512];
        view.read_at(512, &mut got).unwrap();
        assert_eq!(got, &plain[512..1024]);

        // And sector 0 — the relocated one, which is the whole reason the
        // reader has two addresses and two tweaks to get right.
        let mut got0 = vec![0u8; 512];
        view.read_at(0, &mut got0).unwrap();
        assert_eq!(got0, &plain[0..512], "the relocated boot sector must come back whole");

        // Wrong password fails.
        let mut vol2 = MemVolume(view.inner_into_vec());
        assert!(super::open_bitlocker_with_iters(&mut vol2, "nope", stretch_iters, true).is_err());

        // --- The layout real volumes actually use ---
        let mut real = MemVolume(real_layout);
        let info = inspect_bitlocker(&mut real).expect("offsets at 0xB0 must be found");
        assert_eq!(info.cipher, ALG_AES_XTS_128);
        assert_eq!(info.protectors.len(), 1);
        let params = super::open_bitlocker_with_iters(&mut real, password, stretch_iters, true)
            .expect("a volume with the offsets at 0xB0 must unlock");
        assert_eq!(&params.fvek[..], &fvek[..]);
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

    /// A volume carrying nothing but the given protector kinds. No working
    /// crypto — [`inspect_bitlocker`] reads protection types and the cipher,
    /// and that is all this fixture has to be honest about.
    fn volume_with_protectors(kinds: &[u16]) -> MemVolume {
        let mut entries = Vec::new();
        for &kind in kinds {
            let mut vmk = vec![0u8; 8 + 28];
            let n = vmk.len() as u16;
            put_u16(&mut vmk, 0, n);
            put_u16(&mut vmk, 2, 0x0002); // entry_type = VMK
            put_u16(&mut vmk, 4, DATUM_VMK);
            put_u16(&mut vmk, 8 + 0x1A, kind);
            entries.extend_from_slice(&vmk);
        }
        let dataset_size = 0x30 + entries.len();
        let mut dataset = vec![0u8; dataset_size];
        put_u32(&mut dataset, 0x00, dataset_size as u32);
        put_u32(&mut dataset, 0x08, 0x30);
        put_u16(&mut dataset, 0x24, ALG_AES_XTS_128);
        dataset[0x30..].copy_from_slice(&entries);

        let info_off = 4096u64;
        let mut info = vec![0u8; 0x40 + dataset.len()];
        info[0..8].copy_from_slice(FVE_SIGNATURE);
        info[0x40..].copy_from_slice(&dataset);

        let mut dev = vec![0u8; info_off as usize + info.len() + 512];
        dev[3..11].copy_from_slice(FVE_SIGNATURE);
        put_u16(&mut dev, 0x0B, 512);
        put_u64(&mut dev, INFO_OFFSETS_FVE, info_off);
        dev[info_off as usize..info_off as usize + info.len()].copy_from_slice(&info);
        MemVolume(dev)
    }

    #[test]
    fn a_system_volume_is_opened_by_its_recovery_key() {
        // How Windows protects C:: sealed in the TPM, released against a PIN —
        // and a 48-digit recovery password beside it. There is no passphrase
        // protector, because Windows does not allow one on the OS volume. The
        // recovery key is the one way in that a person can type, and we
        // implement it (`open_bitlocker` stretches it exactly like a
        // passphrase), so the wizard must offer the field.
        let info = inspect_bitlocker(&mut volume_with_protectors(&[
            0x0500, // TPM + PIN
            PROTECT_RECOVERY,
        ]))
        .unwrap();
        assert!(!info.has_password(), "an OS volume has no passphrase protector");
        assert!(info.openable(), "…and is still openable — with the recovery key");
        let usable: Vec<u16> =
            info.protectors.iter().filter(|p| p.usable).map(|p| p.kind).collect();
        assert_eq!(usable, vec![PROTECT_RECOVERY]);
    }

    #[test]
    fn a_tpm_only_volume_has_nothing_we_can_type() {
        // No recovery password, no passphrase: the VMK exists only inside the
        // TPM, sealed to this machine's boot measurements. Nothing to ask for.
        let info = inspect_bitlocker(&mut volume_with_protectors(&[0x0100, 0x0500])).unwrap();
        assert!(!info.openable());
        assert!(info.protectors.iter().all(|p| !p.usable));
    }
}
