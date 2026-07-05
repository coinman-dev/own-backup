//! LUKS1/LUKS2 unlock: recover the volume master key from a passphrase.
//!
//! Windows has no LUKS support, so we parse the header and run the key
//! derivation ourselves. This module only *recovers the master key and cipher
//! parameters*; the on-the-fly sector decryption lives in [`crate::decrypt`].
//!
//! Supported: cipher `aes-xts-plain64` (128/256-bit), hashes sha256/sha512,
//! KDFs PBKDF2 / Argon2i / Argon2id. Anything else is an error — the caller
//! then cannot produce a plaintext (used-blocks) backup of the volume.
//!
//! Read-only: we never write the LUKS header or keyslots.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::KeyInit;
use aes::{Aes128, Aes256};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use pbkdf2::pbkdf2_hmac;
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};
use xts_mode::Xts128;
use zeroize::Zeroizing;

use own_backup_common::image::VolumeEncryptionKind;
use own_backup_common::{OwnBackupError, Result};

use crate::fat_map::RawVolume;

/// LUKS magic: `LUKS\xba\xbe` at offset 0 (both versions).
const LUKS_MAGIC: &[u8; 6] = b"LUKS\xba\xbe";
/// LUKS1 enabled-keyslot marker.
const LUKS1_KEY_ENABLED: u32 = 0x00AC_71F3;
/// LUKS2 JSON metadata always begins at this device offset.
const LUKS2_JSON_OFFSET: u64 = 4096;

/// Cipher parameters + master key recovered from a LUKS volume. Everything the
/// decrypt layer needs to read plaintext sectors.
#[derive(Clone)]
pub struct LuksParams {
    /// Byte offset where the encrypted data area starts.
    pub data_offset: u64,
    /// XTS sector size for the data area (512 or 4096).
    pub sector_size: u32,
    /// IV tweak added to every data sector index (LUKS2 `iv_tweak`; usually 0).
    pub iv_offset: u64,
    /// The volume master key (zeroized on drop).
    pub master_key: Zeroizing<Vec<u8>>,
    pub kind: VolumeEncryptionKind,
}

impl LuksParams {
    /// Decrypt `buf` in place. `data_off` is the byte offset within the data
    /// area (0 = first encrypted sector); both `data_off` and `buf.len()` must
    /// be multiples of [`Self::sector_size`].
    pub fn decrypt(&self, data_off: u64, buf: &mut [u8]) -> Result<()> {
        let ss = self.sector_size as u64;
        if data_off % ss != 0 {
            return Err(bad("decrypt offset not sector-aligned"));
        }
        // plain64 counts the XTS tweak in 512-byte units, regardless of the
        // filesystem/segment sector size. So the first unit's tweak is
        // `data_off / 512 + iv_tweak`, and consecutive `sector_size` units step
        // by `sector_size / 512` (8 for a 4096-byte-sector container).
        let first_512 = data_off / 512 + self.iv_offset;
        xts_decrypt(&self.master_key, self.sector_size as usize, first_512 as u128, buf)
    }
}

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("luks: {msg}"))
}

// ---------------------------------------------------------------------------
// Crypto primitives
// ---------------------------------------------------------------------------

/// Hashes LUKS may use for KDFs and the anti-forensic diffuser.
#[derive(Clone, Copy)]
enum Hash {
    Sha256,
    Sha512,
}

impl Hash {
    fn parse(s: &str) -> Result<Hash> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sha256" => Ok(Hash::Sha256),
            "sha512" => Ok(Hash::Sha512),
            other => Err(bad(format!("unsupported hash {other}"))),
        }
    }
    fn size(self) -> usize {
        match self {
            Hash::Sha256 => 32,
            Hash::Sha512 => 64,
        }
    }
    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha256 => Sha256::digest(data).to_vec(),
            Hash::Sha512 => Sha512::digest(data).to_vec(),
        }
    }
}

fn pbkdf2(hash: Hash, pw: &[u8], salt: &[u8], iters: u32, out_len: usize) -> Vec<u8> {
    let mut out = vec![0u8; out_len];
    match hash {
        Hash::Sha256 => pbkdf2_hmac::<Sha256>(pw, salt, iters, &mut out),
        Hash::Sha512 => pbkdf2_hmac::<Sha512>(pw, salt, iters, &mut out),
    }
    out
}

fn argon2_kdf(
    kind: &str,
    pw: &[u8],
    salt: &[u8],
    time: u32,
    mem_kib: u32,
    lanes: u32,
    out_len: usize,
) -> Result<Vec<u8>> {
    let alg = match kind {
        "argon2i" => Algorithm::Argon2i,
        "argon2id" => Algorithm::Argon2id,
        other => return Err(bad(format!("unsupported kdf {other}"))),
    };
    let params = Params::new(mem_kib, time, lanes, Some(out_len))
        .map_err(|e| bad(format!("argon2 params: {e}")))?;
    let mut out = vec![0u8; out_len];
    Argon2::new(alg, Version::V0x13, params)
        .hash_password_into(pw, salt, &mut out)
        .map_err(|e| bad(format!("argon2: {e}")))?;
    Ok(out)
}

/// The LUKS anti-forensic diffuser (`AF_hash` in the spec).
fn diffuse(hash: Hash, data: &[u8]) -> Vec<u8> {
    let ds = hash.size();
    let mut out = vec![0u8; data.len()];
    let full = data.len() / ds;
    let rem = data.len() % ds;
    let mut block = Vec::with_capacity(4 + ds);
    for i in 0..full {
        block.clear();
        block.extend_from_slice(&(i as u32).to_be_bytes());
        block.extend_from_slice(&data[i * ds..(i + 1) * ds]);
        out[i * ds..(i + 1) * ds].copy_from_slice(&hash.digest(&block));
    }
    if rem > 0 {
        block.clear();
        block.extend_from_slice(&(full as u32).to_be_bytes());
        block.extend_from_slice(&data[full * ds..]);
        out[full * ds..].copy_from_slice(&hash.digest(&block)[..rem]);
    }
    out
}

/// Merge an anti-forensic-split key material buffer back into the key.
fn af_merge(hash: Hash, src: &[u8], blocksize: usize, stripes: usize) -> Result<Vec<u8>> {
    if src.len() < blocksize * stripes || stripes == 0 {
        return Err(bad("AF material too small"));
    }
    let mut d = vec![0u8; blocksize];
    for i in 0..stripes - 1 {
        for j in 0..blocksize {
            d[j] ^= src[i * blocksize + j];
        }
        d = diffuse(hash, &d);
    }
    let last = (stripes - 1) * blocksize;
    for j in 0..blocksize {
        d[j] ^= src[last + j];
    }
    Ok(d)
}

/// AES-XTS-plain64 decrypt of `buf` in place. `sector_size` must be a multiple
/// of 512 and divide `buf.len()`. `first_512_unit` is the plain64 tweak of the
/// first unit, counted in 512-byte units; the k-th `sector_size` unit uses
/// tweak `first_512_unit + k * (sector_size / 512)` — plain64 always counts the
/// tweak in 512-byte units, so a 4096-byte-sector container steps by 8.
fn xts_decrypt(key: &[u8], sector_size: usize, first_512_unit: u128, buf: &mut [u8]) -> Result<()> {
    if sector_size == 0 || sector_size % 512 != 0 || buf.len() % sector_size != 0 {
        return Err(bad("XTS buffer/sector size not a valid multiple of 512"));
    }
    let step = (sector_size / 512) as u128;
    let tweak = |unit_index: u128| (first_512_unit + unit_index * step).to_le_bytes();
    match key.len() {
        32 => {
            let c1 = Aes128::new(GenericArray::from_slice(&key[..16]));
            let c2 = Aes128::new(GenericArray::from_slice(&key[16..]));
            Xts128::new(c1, c2).decrypt_area(buf, sector_size, 0, tweak);
        }
        64 => {
            let c1 = Aes256::new(GenericArray::from_slice(&key[..32]));
            let c2 = Aes256::new(GenericArray::from_slice(&key[32..]));
            Xts128::new(c1, c2).decrypt_area(buf, sector_size, 0, tweak);
        }
        n => return Err(bad(format!("unsupported AES-XTS key length {n}"))),
    }
    Ok(())
}

/// Only `aes-xts-plain64` is supported; return the master-key byte length.
fn check_cipher(spec: &str) -> Result<()> {
    if spec.trim() != "aes-xts-plain64" {
        return Err(bad(format!("unsupported cipher {spec:?} (only aes-xts-plain64)")));
    }
    Ok(())
}

/// Read `len` bytes at sector-aligned `offset` off the volume.
fn read_at(vol: &mut dyn RawVolume, offset: u64, len: usize) -> Result<Vec<u8>> {
    let padded = len.next_multiple_of(512);
    let mut buf = vec![0u8; padded];
    vol.read_at(offset, &mut buf)?;
    buf.truncate(len);
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// True if `header` (at least 8 bytes off the volume start) is a LUKS container.
pub fn is_luks(header: &[u8]) -> bool {
    header.len() >= 6 && &header[0..6] == LUKS_MAGIC
}

/// Recover the master key + cipher parameters of a LUKS volume using
/// `passphrase`. Errors on a wrong passphrase, an unsupported cipher/KDF, or a
/// malformed header.
pub fn open_luks(vol: &mut dyn RawVolume, passphrase: &[u8]) -> Result<LuksParams> {
    let header = read_at(vol, 0, 4096)?;
    if !is_luks(&header) {
        return Err(bad("not a LUKS container"));
    }
    let version = u16::from_be_bytes(header[6..8].try_into().unwrap());
    match version {
        1 => unlock_luks1(vol, &header, passphrase),
        2 => unlock_luks2(vol, &header, passphrase),
        v => Err(bad(format!("unsupported LUKS version {v}"))),
    }
}

// ---------------------------------------------------------------------------
// LUKS1 (big-endian binary header)
// ---------------------------------------------------------------------------

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

fn unlock_luks1(vol: &mut dyn RawVolume, hdr: &[u8], pass: &[u8]) -> Result<LuksParams> {
    let be32 = |o: usize| u32::from_be_bytes(hdr[o..o + 4].try_into().unwrap());

    let cipher = cstr(&hdr[8..40]);
    let mode = cstr(&hdr[40..72]);
    check_cipher(&format!("{cipher}-{mode}"))?;
    let hash = Hash::parse(&cstr(&hdr[72..104]))?;
    let payload_offset = be32(104) as u64; // sectors
    let key_bytes = be32(108) as usize;
    if !(16..=64).contains(&key_bytes) {
        return Err(bad(format!("implausible LUKS1 key length {key_bytes}")));
    }
    let mk_digest = &hdr[112..132]; // 20 bytes
    let mk_salt = &hdr[132..164];
    let mk_iter = be32(164);

    // 8 keyslots of 48 bytes each at offset 208.
    for s in 0..8 {
        let base = 208 + s * 48;
        if be32(base) != LUKS1_KEY_ENABLED {
            continue;
        }
        let iters = be32(base + 4);
        let salt = &hdr[base + 8..base + 40];
        let km_off = be32(base + 40) as u64; // sectors
        let stripes = be32(base + 44) as usize;
        if stripes == 0 || stripes > 100_000 {
            continue;
        }

        let slot_key = Zeroizing::new(pbkdf2(hash, pass, salt, iters, key_bytes));
        let material_len = key_bytes * stripes;
        let mut material = Zeroizing::new(read_at(vol, km_off * 512, material_len.next_multiple_of(512))?);
        xts_decrypt(&slot_key, 512, 0, &mut material)?;
        let mk = Zeroizing::new(af_merge(hash, &material[..material_len], key_bytes, stripes)?);

        // Verify against the master-key digest.
        if pbkdf2(hash, &mk, mk_salt, mk_iter, mk_digest.len()) == mk_digest {
            return Ok(LuksParams {
                data_offset: payload_offset * 512,
                sector_size: 512,
                iv_offset: 0,
                master_key: Zeroizing::new(mk.to_vec()),
                kind: VolumeEncryptionKind::Luks1,
            });
        }
    }
    Err(bad("passphrase did not match any LUKS1 keyslot"))
}

// ---------------------------------------------------------------------------
// LUKS2 (binary header + JSON metadata)
// ---------------------------------------------------------------------------

/// A JSON field that LUKS2 stores as either a number or a decimal string.
fn jnum(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn jstr<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}

fn b64(v: &Value, key: &str) -> Result<Vec<u8>> {
    let s = jstr(v, key).ok_or_else(|| bad(format!("missing base64 field {key}")))?;
    B64.decode(s).map_err(|e| bad(format!("bad base64 in {key}: {e}")))
}

fn unlock_luks2(vol: &mut dyn RawVolume, hdr: &[u8], pass: &[u8]) -> Result<LuksParams> {
    let hdr_size = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
    if !(LUKS2_JSON_OFFSET..=16 * 1024 * 1024).contains(&hdr_size) {
        return Err(bad(format!("implausible LUKS2 header size {hdr_size}")));
    }
    let json_raw = read_at(vol, LUKS2_JSON_OFFSET, (hdr_size - LUKS2_JSON_OFFSET) as usize)?;
    let end = json_raw.iter().position(|&b| b == 0).unwrap_or(json_raw.len());
    let meta: Value =
        serde_json::from_slice(&json_raw[..end]).map_err(|e| bad(format!("LUKS2 JSON: {e}")))?;

    // --- The (single) crypt data segment. ---
    let segments = meta.get("segments").and_then(Value::as_object).ok_or_else(|| bad("no segments"))?;
    let seg = segments
        .values()
        .find(|s| jstr(s, "type") == Some("crypt"))
        .ok_or_else(|| bad("no crypt segment"))?;
    check_cipher(jstr(seg, "encryption").unwrap_or_default())?;
    let data_offset = jnum(seg.get("offset").unwrap_or(&Value::Null)).ok_or_else(|| bad("segment offset"))?;
    let sector_size = seg.get("sector_size").and_then(jnum).unwrap_or(512) as u32;
    let iv_offset = seg.get("iv_tweak").and_then(jnum).unwrap_or(0);

    let keyslots = meta.get("keyslots").and_then(Value::as_object).ok_or_else(|| bad("no keyslots"))?;
    let digests = meta.get("digests").and_then(Value::as_object).ok_or_else(|| bad("no digests"))?;

    // --- Try each keyslot. ---
    for (id, ks) in keyslots {
        if jstr(ks, "type") != Some("luks2") {
            continue;
        }
        let key_size = ks.get("key_size").and_then(jnum).ok_or_else(|| bad("keyslot key_size"))? as usize;
        let area = ks.get("area").ok_or_else(|| bad("keyslot area"))?;
        check_cipher(jstr(area, "encryption").unwrap_or_default())?;
        let area_off = jnum(area.get("offset").unwrap_or(&Value::Null)).ok_or_else(|| bad("area offset"))?;
        let area_size = jnum(area.get("size").unwrap_or(&Value::Null)).ok_or_else(|| bad("area size"))? as usize;
        let af = ks.get("af").ok_or_else(|| bad("keyslot af"))?;
        let af_hash = Hash::parse(jstr(af, "hash").unwrap_or("sha256"))?;
        let stripes = af.get("stripes").and_then(jnum).ok_or_else(|| bad("af stripes"))? as usize;

        // Derive the keyslot key from the passphrase.
        let kdf = ks.get("kdf").ok_or_else(|| bad("keyslot kdf"))?;
        let salt = b64(kdf, "salt")?;
        let kdf_type = jstr(kdf, "type").unwrap_or_default();
        let slot_key = Zeroizing::new(if kdf_type == "pbkdf2" {
            let h = Hash::parse(jstr(kdf, "hash").unwrap_or("sha256"))?;
            let iters = kdf.get("iterations").and_then(jnum).ok_or_else(|| bad("pbkdf2 iterations"))? as u32;
            pbkdf2(h, pass, &salt, iters, key_size)
        } else {
            let time = kdf.get("time").and_then(jnum).ok_or_else(|| bad("argon2 time"))? as u32;
            let mem = kdf.get("memory").and_then(jnum).ok_or_else(|| bad("argon2 memory"))? as u32;
            let cpus = kdf.get("cpus").and_then(jnum).unwrap_or(1) as u32;
            argon2_kdf(kdf_type, pass, &salt, time, mem, cpus, key_size)?
        });

        // Decrypt the keyslot's AF area and merge it into a candidate master key.
        // Guard the split size: a malformed header must error, never panic or
        // read a huge buffer.
        let need = stripes
            .checked_mul(key_size)
            .filter(|&n| n > 0 && n <= 64 * 1024 * 1024)
            .ok_or_else(|| bad("implausible AF material size"))?;
        if area_size < need {
            return Err(bad("keyslot area is smaller than its AF key material"));
        }
        let mut material = Zeroizing::new(read_at(vol, area_off, need.next_multiple_of(512))?);
        xts_decrypt(&slot_key, 512, 0, &mut material)?;
        let mk = Zeroizing::new(af_merge(af_hash, &material[..need], key_size, stripes)?);

        // Verify against a digest that references this keyslot.
        for digest in digests.values() {
            let refs = digest.get("keyslots").and_then(Value::as_array);
            let matches_slot = refs.is_some_and(|a| a.iter().any(|x| x.as_str() == Some(id)));
            if !matches_slot || jstr(digest, "type") != Some("pbkdf2") {
                continue;
            }
            let h = Hash::parse(jstr(digest, "hash").unwrap_or("sha256"))?;
            let iters = digest.get("iterations").and_then(jnum).ok_or_else(|| bad("digest iterations"))? as u32;
            let dsalt = b64(digest, "salt")?;
            let want_digest = b64(digest, "digest")?;
            if pbkdf2(h, &mk, &dsalt, iters, want_digest.len()) == want_digest {
                return Ok(LuksParams {
                    data_offset,
                    sector_size,
                    iv_offset,
                    master_key: Zeroizing::new(mk.to_vec()),
                    kind: VolumeEncryptionKind::Luks2,
                });
            }
        }
    }
    Err(bad("passphrase did not match any LUKS2 keyslot"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use xts_mode::get_tweak_default;

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

    /// Test-only inverse of [`af_merge`]: split `mk` into `stripes` blocks whose
    /// AF-merge reproduces `mk`. The first stripes are deterministic filler.
    fn af_split(hash: Hash, mk: &[u8], stripes: usize) -> Vec<u8> {
        let bs = mk.len();
        let mut out = vec![0u8; bs * stripes];
        let mut d = vec![0u8; bs];
        for i in 0..stripes - 1 {
            for j in 0..bs {
                let v = ((i * 31 + j * 7 + 1) & 0xff) as u8;
                out[i * bs + j] = v;
                d[j] ^= v;
            }
            d = diffuse(hash, &d);
        }
        for j in 0..bs {
            out[(stripes - 1) * bs + j] = d[j] ^ mk[j];
        }
        out
    }

    #[test]
    fn af_merge_inverts_split() {
        let hash = Hash::Sha256;
        let mk: Vec<u8> = (0..64u8).collect();
        let split = af_split(hash, &mk, 8);
        assert_eq!(af_merge(hash, &split, 64, 8).unwrap(), mk);
    }

    #[test]
    fn xts_4096_sector_steps_tweak_in_512_units() {
        // plain64 counts the tweak in 512-byte units, so a 4096-byte-sector
        // volume must step the tweak by 8 per unit. Encrypt each 4096 unit at
        // data offset D with tweak D/512, then LuksParams::decrypt must recover
        // it. (This fails if decrypt steps the tweak by 1.)
        use zeroize::Zeroizing;
        let key: Vec<u8> = (0..64u8).collect();
        let ss = 4096usize;
        let units = 3usize;
        let plain: Vec<u8> = (0..(units * ss)).map(|i| (i * 7 + 1) as u8).collect();

        let c1 = Aes256::new(GenericArray::from_slice(&key[..32]));
        let c2 = Aes256::new(GenericArray::from_slice(&key[32..]));
        let xts = Xts128::new(c1, c2);
        let mut enc = plain.clone();
        for (i, unit) in enc.chunks_mut(ss).enumerate() {
            let tweak_512 = (i * (ss / 512)) as u128; // D/512 with D = i*ss
            xts.encrypt_sector(unit, tweak_512.to_le_bytes());
        }

        let params = LuksParams {
            data_offset: 0,
            sector_size: 4096,
            iv_offset: 0,
            master_key: Zeroizing::new(key),
            kind: VolumeEncryptionKind::Luks2,
        };
        let mut dec = enc.clone();
        params.decrypt(0, &mut dec).unwrap();
        assert_eq!(dec, plain);
    }

    #[test]
    fn xts_roundtrips() {
        let key: Vec<u8> = (0..64u8).collect();
        let mut buf: Vec<u8> = (0..512u32).map(|i| i as u8).collect();
        let orig = buf.clone();
        // Encrypt with xts-mode, then our decrypt must recover it.
        let c1 = Aes256::new(GenericArray::from_slice(&key[..32]));
        let c2 = Aes256::new(GenericArray::from_slice(&key[32..]));
        Xts128::new(c1, c2).encrypt_area(&mut buf, 512, 0, get_tweak_default);
        xts_decrypt(&key, 512, 0, &mut buf).unwrap();
        assert_eq!(buf, orig);
    }

    fn put_be32(b: &mut [u8], o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_be_bytes());
    }

    /// Build a self-consistent LUKS1 container (one keyslot) and prove unlock
    /// recovers the master key. Uses small iteration/stripe counts for speed.
    #[test]
    fn luks1_unlock_recovers_master_key() {
        let hash = Hash::Sha256;
        let key_bytes = 64usize;
        let stripes = 8usize;
        let pass = b"correct horse";
        let mk: Vec<u8> = (0..key_bytes as u16).map(|i| (i * 3) as u8).collect();

        let km_off_sectors = 8u32; // material at byte 4096
        let material_len = key_bytes * stripes; // 512, one sector
        let dev_len = 4096 + material_len.next_multiple_of(512) + 512;
        let mut b = vec![0u8; dev_len];

        // --- Header ---
        b[0..6].copy_from_slice(LUKS_MAGIC);
        b[6..8].copy_from_slice(&1u16.to_be_bytes()); // version
        b[8..11].copy_from_slice(b"aes");
        b[40..51].copy_from_slice(b"xts-plain64");
        b[72..78].copy_from_slice(b"sha256");
        put_be32(&mut b, 104, 4096); // payload_offset (sectors)
        put_be32(&mut b, 108, key_bytes as u32);
        let mk_salt = [7u8; 32];
        let mk_iter = 500u32;
        b[132..164].copy_from_slice(&mk_salt);
        put_be32(&mut b, 164, mk_iter);
        let mk_digest = pbkdf2(hash, &mk, &mk_salt, mk_iter, 20);
        b[112..132].copy_from_slice(&mk_digest);

        // --- Keyslot 0 ---
        let slot_salt = [9u8; 32];
        let slot_iter = 500u32;
        put_be32(&mut b, 208, LUKS1_KEY_ENABLED);
        put_be32(&mut b, 212, slot_iter);
        b[216..248].copy_from_slice(&slot_salt);
        put_be32(&mut b, 248, km_off_sectors);
        put_be32(&mut b, 252, stripes as u32);

        // --- Key material: AF-split(mk), encrypted with the slot key ---
        let slot_key = pbkdf2(hash, pass, &slot_salt, slot_iter, key_bytes);
        let mut material = af_split(hash, &mk, stripes);
        let c1 = Aes256::new(GenericArray::from_slice(&slot_key[..32]));
        let c2 = Aes256::new(GenericArray::from_slice(&slot_key[32..]));
        Xts128::new(c1, c2).encrypt_area(&mut material, 512, 0, get_tweak_default);
        let at = (km_off_sectors * 512) as usize;
        b[at..at + material.len()].copy_from_slice(&material);

        let mut vol = MemVolume(b);
        let params = open_luks(&mut vol, pass).unwrap();
        assert_eq!(&params.master_key[..], &mk[..]);
        assert_eq!(params.data_offset, 4096 * 512);
        assert_eq!(params.sector_size, 512);

        // A wrong passphrase must fail.
        assert!(open_luks(&mut vol, b"wrong").is_err());
    }

    /// Build a minimal LUKS2 container (pbkdf2 KDF for test speed) and unlock it.
    #[test]
    fn luks2_unlock_recovers_master_key() {
        let hash = Hash::Sha256;
        let key_size = 64usize;
        let stripes = 8usize;
        let pass = b"battery staple";
        let mk: Vec<u8> = (0..key_size as u16).map(|i| (i * 5 + 1) as u8).collect();

        let area_off = 32768u64;
        let data_off = 16 * 1024 * 1024u64;
        let hdr_size = 16384u64;
        let dev_len = (area_off as usize) + 512 + 512;
        let mut b = vec![0u8; dev_len.max(hdr_size as usize)];

        // Binary header.
        b[0..6].copy_from_slice(LUKS_MAGIC);
        b[6..8].copy_from_slice(&2u16.to_be_bytes());
        b[8..16].copy_from_slice(&hdr_size.to_be_bytes());

        let slot_salt = [3u8; 32];
        let slot_iter = 500u32;
        let dsalt = [4u8; 32];
        let diter = 500u32;
        let want_digest = pbkdf2(hash, &mk, &dsalt, diter, 32);

        let json = format!(
            r#"{{"keyslots":{{"0":{{"type":"luks2","key_size":{ks},"kdf":{{"type":"pbkdf2","hash":"sha256","iterations":{si},"salt":"{ss}"}},"af":{{"type":"luks1","stripes":{st},"hash":"sha256"}},"area":{{"type":"raw","offset":"{ao}","size":"512","encryption":"aes-xts-plain64","key_size":{ks}}}}}}},"segments":{{"0":{{"type":"crypt","offset":"{do_}","iv_tweak":"0","encryption":"aes-xts-plain64","sector_size":512}}}},"digests":{{"0":{{"type":"pbkdf2","keyslots":["0"],"segments":["0"],"hash":"sha256","iterations":{di},"salt":"{ds}","digest":"{dg}"}}}}}}"#,
            ks = key_size,
            si = slot_iter,
            ss = B64.encode(slot_salt),
            st = stripes,
            ao = area_off,
            do_ = data_off,
            di = diter,
            ds = B64.encode(dsalt),
            dg = B64.encode(&want_digest),
        );
        let jb = json.as_bytes();
        b[LUKS2_JSON_OFFSET as usize..LUKS2_JSON_OFFSET as usize + jb.len()].copy_from_slice(jb);

        // Keyslot AF area.
        let slot_key = pbkdf2(hash, pass, &slot_salt, slot_iter, key_size);
        let mut material = af_split(hash, &mk, stripes);
        let c1 = Aes256::new(GenericArray::from_slice(&slot_key[..32]));
        let c2 = Aes256::new(GenericArray::from_slice(&slot_key[32..]));
        Xts128::new(c1, c2).encrypt_area(&mut material, 512, 0, get_tweak_default);
        b[area_off as usize..area_off as usize + material.len()].copy_from_slice(&material);

        let mut vol = MemVolume(b);
        let params = open_luks(&mut vol, pass).unwrap();
        assert_eq!(&params.master_key[..], &mk[..]);
        assert_eq!(params.data_offset, data_off);
        assert!(open_luks(&mut vol, b"nope").is_err());
    }

    #[test]
    fn rejects_non_luks() {
        let mut vol = MemVolume(vec![0u8; 8192]);
        assert!(open_luks(&mut vol, b"x").is_err());
    }

    /// Cross-check master-key recovery against the reference implementation:
    /// `cryptsetup` formats a real LUKS1/LUKS2 container (root-free, on a file)
    /// and dumps its master key; [`open_luks`] must recover the same bytes.
    /// `cargo test -p own-backup-backup -- --ignored` (needs `cryptsetup`).
    #[test]
    #[ignore]
    fn unlock_matches_cryptsetup() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        fn cryptsetup(args: &[&str], password: &str) -> std::process::Output {
            let mut child = Command::new("cryptsetup")
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn cryptsetup");
            child.stdin.take().unwrap().write_all(password.as_bytes()).unwrap();
            child.wait_with_output().unwrap()
        }
        fn push_hex(s: &str, out: &mut Vec<u8>) {
            for tok in s.split_whitespace() {
                if let Ok(b) = u8::from_str_radix(tok, 16) {
                    out.push(b);
                }
            }
        }
        // Parse the hex bytes under a `luksDump --dump-master-key`'s "MK dump:".
        fn parse_mk(dump: &str) -> Vec<u8> {
            let mut bytes = Vec::new();
            let mut in_mk = false;
            for line in dump.lines() {
                if let Some(rest) = line.split_once("MK dump:") {
                    in_mk = true;
                    push_hex(rest.1, &mut bytes);
                } else if in_mk {
                    let t = line.trim();
                    if t.is_empty() || !t.starts_with(|c: char| c.is_ascii_hexdigit()) {
                        break;
                    }
                    push_hex(t, &mut bytes);
                }
            }
            bytes
        }

        let dir = tempfile::tempdir().unwrap();
        let pw = "correct horse battery";
        let cases: [(&str, Vec<&str>); 2] = [
            (
                "luks1",
                vec![
                    "luksFormat", "-q", "--type", "luks1", "--pbkdf", "pbkdf2",
                    "--pbkdf-force-iterations", "1000", "--key-file", "-",
                ],
            ),
            (
                "luks2-argon2id",
                vec![
                    "luksFormat", "-q", "--type", "luks2", "--pbkdf", "argon2id",
                    "--pbkdf-force-iterations", "4", "--pbkdf-memory", "32",
                    "--pbkdf-parallel", "1", "--key-file", "-",
                ],
            ),
        ];

        for (label, fmt) in cases {
            let path = dir.path().join(format!("{label}.luks"));
            std::fs::File::create(&path).unwrap().set_len(32 * 1024 * 1024).unwrap();
            let mut args = fmt.clone();
            args.push(path.to_str().unwrap());
            let out = cryptsetup(&args, pw);
            assert!(
                out.status.success(),
                "{label} luksFormat: {}",
                String::from_utf8_lossy(&out.stderr)
            );

            let dump = cryptsetup(
                &["luksDump", "--dump-master-key", "-q", "--key-file", "-", path.to_str().unwrap()],
                pw,
            );
            assert!(dump.status.success(), "{label} dump: {}", String::from_utf8_lossy(&dump.stderr));
            let mk = parse_mk(&String::from_utf8_lossy(&dump.stdout));
            assert_eq!(mk.len(), 64, "{label}: parsed MK is {} bytes", mk.len());

            let mut vol = MemVolume(std::fs::read(&path).unwrap());
            let params = open_luks(&mut vol, pw.as_bytes())
                .unwrap_or_else(|e| panic!("{label} open_luks: {e}"));
            assert_eq!(&params.master_key[..], &mk[..], "{label}: master key mismatch");
            assert!(open_luks(&mut vol, b"wrong pass").is_err(), "{label}: wrong pw must fail");
        }
    }
}
