# Backup encryption core, version 2

Status: E1 cryptographic records and portable key envelopes. This is **not yet
the complete qcow2 extension**: the image header, authenticated snapshot index,
commit protocol, Windows key store and engine integration follow in E2–E4.
The implementation is in this directory; the product requirements are in
`.ai/PLAN.md` §7.

Version 2 intentionally rejects the unshipped version 1 XTS prototype.
`SCHEME = "own-backup/v2"`. All binary integers below are unsigned big-endian.
All UUID-like identities are their 16 raw canonical bytes, not Windows GUID
structure layout. All encryption uses AES-256-GCM with 96-bit nonces and full
128-bit tags. Keys and randomness come from the operating-system CSPRNG;
there is no fallback RNG.

## Key hierarchy

Each job owns an X25519 pair. Every full backup chain gets an independent,
uniformly random 32-byte master key. Incremental and differential images inherit
the master of their chain. The master is input to HKDF, never used directly
as an AES key for record data.

The portable private-key envelope contains password and recovery slots.
Automatic work requires a **separate local protected copy of the private job
key**, owned by the service. It is not a third portable slot. DPAPI and its
access controls are E2; possession of the public job key cannot reopen a chain.

The program must keep the public key and key identities associated with the
configured job. Matching an attacker-supplied fingerprint to an
attacker-supplied public key does not establish trust.

## Authenticated record

A record consists of the following 84-byte header, followed by exactly
`payload_len` ciphertext bytes and 16 tag bytes. There is no padding or trailer
inside the record. Container alignment, if any, is outside this framing.

| Offset | Bytes | Field |
|---|---:|---|
| 0 | 4 | ASCII `OBKR` |
| 4 | 2 | Record version, 2 |
| 6 | 1 | Purpose: Data=1, Manifest=2, Index=3, Scratch=4 |
| 7 | 1 | Encoding: Raw=0, Zstd=1 |
| 8 | 16 | Chain identity |
| 24 | 8 | Logical index: guest cluster for Data/Scratch; chunk ordinal for metadata |
| 32 | 4 | Decoded length, after optional decompression |
| 36 | 4 | Payload length, before encryption / after optional compression |
| 40 | 32 | Random writer-session identity |
| 72 | 12 | Nonce |
| 84 | payload_len | Ciphertext |
| 84 + payload_len | 16 | GCM authentication tag |

The **entire header, unchanged**, is GCM AAD. A record is bounded to 1 MiB of
payload and 1 MiB decoded data. Larger metadata is split into indexed records.
Raw requires equal lengths, including zero. Zstd requires a nonempty payload
strictly smaller than the decoded length; otherwise the writer uses Raw.
Lengths, framing, version, expected chain/purpose/index and expected record
identity are checked before decryption/allocation of the work buffer.
Authentication succeeds before any decompression or disclosure of plaintext.
The caller must bound decompression to the authenticated decoded length and
reject an output whose size differs.

The session key is:

```text
salt = chain_id[16] || session_id[32]
info = UTF8("own-backup/v2/record/") || purpose_u8
session_key = HKDF-SHA256(master_chain_key, salt, info, output_length=32)
nonce = zero[4] || record_counter_u64_be
```

A `RecordWriter` is neither Clone nor deserializable. Construction always
creates a fresh random 256-bit session identity. Sharing a writer through
`Arc` is supported: a mutex allocates the counter and charges the usage budget
before encryption. An allocated number is never reclaimed after encryption,
I/O failure or cancellation. Reopening a writer, including after a crash,
creates a fresh session; there is no resume-from-counter API. RNG failure
aborts allocation. A poisoned writer refuses subsequent work.

Before either budget would be exceeded, the writer starts a fresh session:

- At most 2^20 record encryptions per session key.
- At most 2^28 authenticated 16-byte blocks per session key. Each record costs
  `ceil(84/16) + ceil(payload_len/16) + 1`, including padded AAD and GCM's
  lengths block. This is approximately 4 GiB of authenticated input.

These are conservative application limits, not claims about the maximum GCM
limits. A single plaintext is at most 1 MiB, well below GCM's per-message limit.
Purpose and chain are separated in HKDF; each independent writer uses fresh
randomness. No counter is persisted, and two writers never intentionally use
the same session.

Copying a **complete existing record** to another physical location inside
the same chain does not invoke encryption and does not consume another nonce.
No physical file offset or destination image filename is part of the key/AAD.
Changing plaintext, logical position, encoding or AAD requires a new encryption
and a new record identity. Cross-chain grafting is not supported.

### Index contract and replay

A `RecordId` is `session_id[32] || nonce[12]`. `open_record` requires this
expected identity in addition to the expected logical context. It must come
from the authenticated snapshot index, **not be copied from the same untrusted
record just to satisfy the API**. This distinguishes a current record from an
old but valid record at the same guest location.

E3 must authenticate the index, geometry, parent bindings, explicit zeros and
holes, version ownership, and all pointers used to resolve an image. A forged
header is never authority for which record belongs at a location. Metadata
root selection and interrupted commit recovery must also be authenticated.
Those structures are not implemented by E1, so E1 alone does not provide a
mountable image or whole-archive rollback protection.

## Wrapped chain key

The binary envelope is exactly 102 bytes:

| Offset | Bytes | Field |
|---|---:|---|
| 0 | 4 | ASCII `OBKW` |
| 4 | 2 | Version, 2 |
| 6 | 16 | Chain identity |
| 22 | 32 | Ephemeral X25519 public key E |
| 54 | 48 | Encrypted 32-byte chain master + 16-byte tag |

For each wrapping operation generate a fresh ephemeral secret e:

```text
shared = X25519(e, recipient_public_key)
salt = E[32] || recipient_public_key[32]
info = UTF8("own-backup/v2/wrap")
wrap_key = HKDF-SHA256(shared, salt, info, output_length=32)
AAD = envelope_bytes[0..54] || recipient_public_key[32]
sealed = AES-256-GCM(wrap_key, nonce=zero[12], chain_master, AAD)
```

A non-contributory (all-zero) X25519 shared secret is rejected on both paths.
The fixed nonce is used only with a newly derived, single-use ephemeral wrap
key. It must never be reused for an additional encryption. The complete public
key, chain identity and version are authenticated, not just a short fingerprint.
Unwrap requires the expected chain identity separately. The construction uses
standard X25519/HKDF/AEAD components but is not an interoperable age file.

## Portable private-job-key file

UTF-8 JSON, no more than 16 KiB. Fields: `format`, `scheme`, `job_id`,
`public`, `fingerprint`, `slots`. Format is `own-backup-job-key`;
scheme is `own-backup/v2`. Unknown fields/versions are rejected.
The public key is padded standard base64 of exactly 32 bytes; fingerprint
is the complete lowercase SHA-256 hex string (64 characters).
Job identity is 1–128 UTF-8 bytes without control characters.

Exactly two slots, one Password and one Recovery; order has no meaning.
Each slot contains `kind`, `kdf`, `salt`, `nonce`, `sealed`.
Kinds serialize as `password` / `recovery`. Each salt is 16 random bytes;
nonce is 12 random bytes; sealed data is 32 bytes plus a 16-byte GCM tag.
Byte fields use canonical padded base64, checked for exact lengths.

Both slots use Argon2id v1.3 with a 32-byte output. The recovery slot deliberately
keeps the same KDF for v2; a different recovery KDF needs a new explicit format.

Production KDF bounds, validated before running either slot:

- Algorithm must be `argon2id`.
- Memory 64–512 MiB, time 3–6 passes, parallelism 1–8.
- `m_kib * t <= 1536 * 1024`.
- Initial write profile: 256 MiB, 3 passes, 4 lanes.
- Password input 1–1024 UTF-8 bytes; product password-strength policy is E5.
- Recovery input is exactly 32 random bytes.

The unit-test-only 1 MiB profile is not available or accepted in normal
library builds; an integration test checks that boundary. Latency of the write
profile on Windows and WinPE has not yet been measured.

Slot AAD is the following concatenation, with no JSON serialization involved:

```text
ASCII("OBKS") || u16(2)
|| u16(job_id_utf8_length) || UTF8(job_id)
|| recipient_public_key[32]
|| slot_kind_u8                      # Password=1, Recovery=2
|| u32(m_kib) || u32(t) || u32(p)
|| salt[16] || nonce[12]
```

A document must pass complete structural validation before KDF execution,
including its unselected slot. Argon2 memory is wiped when released.
The derived key encrypts the 32-byte private key with AES-256-GCM.
The opened private key must reproduce the declared public key.
An authentication failure reports a wrong secret **or a damaged file**;
a tag mismatch alone cannot distinguish those cases.

Local service authorization, password changes, export and recovery policies
are outside this portable format. Neither the user's plaintext password nor
a reusable password-derived key is a scheduler credential.

## Recovery code and memory

Recovery codes encode 32 CSPRNG bytes with **Bech32m** (not legacy Bech32),
HRP `obkr`. The compact form is 63 characters. Input may use whitespace
and either case, with a 256-byte input cap; decoding must round-trip to the
canonical lowercase form to reject noncanonical padding.

Secret keys, KDF output and work memory, recovery strings and plaintext
decryption buffers use Zeroizing. AES/GCM/GHASH/POLYVAL zeroization features
are enabled. This reduces residual process memory, but does not protect
against privileged access to a live process, paging, crash dumps or a full
VM memory snapshot. A VM clone/restored memory snapshot containing an active
writer must not continue its old encryption session. A forked child must create
a new writer as well; external lifecycle integration remains a caller obligation.

## Acceptance and sources

Tests cover NIST AES-256-GCM known answers, RFC 5869 HKDF, RFC 7748 X25519 and
RFC 9106 Argon2id; malformed framing and KDF limits; role/job/chain binding;
tampering, truncation, replay with a trusted expected identity; concurrent
nonce allocation and session budget rotation.

- [NIST SP 800-38D](https://csrc.nist.gov/pubs/sp/800/38/d/final)
- [RFC 5116 nonce requirements](https://www.rfc-editor.org/rfc/rfc5116.html#section-3.1)
- [RFC 5869](https://www.rfc-editor.org/rfc/rfc5869.html)
- [RFC 7748](https://www.rfc-editor.org/rfc/rfc7748.html)
- [RFC 9106](https://www.rfc-editor.org/rfc/rfc9106.html)
