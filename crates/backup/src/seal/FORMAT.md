# Backup encryption core, version 2

Status: E1 cryptographic records and portable key envelopes, plus the E3
container — the `.obk` image, its authenticated index and its root record, all
specified below and implemented in `obk/`. Engine integration, the encrypted
scratch layer, consolidation and maintenance are E4; the wizard, manual opening
and the boot medium are E5. The Windows service key store is E2 (`LOCAL.md`).
The product requirements are in `.ai/PLAN.md` §7.

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

The index, geometry, parent bindings, explicit zeros and holes, and every
pointer used to resolve an image are authenticated by the container below. A
forged header is never authority for which record belongs at a location.
Whole-archive rollback — replacing every file of a chain with an older, also
valid set — is still out of scope and needs external trusted state.

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

## The `.obk` container

An encrypted image is a **qcow2 version-3 file** with incompatible feature
**bit 60** set and a feature name table naming it `own-backup encryption v2`.
Every other reader must refuse it; `qemu-img info`, `check` and `convert` do,
by that name. qcow2's own `crypt_method` stays 0 — this is our extension, not
qcow2 encryption — and internal snapshots, external data files and a header
length other than 112 are refused. The compression type field says zstd.

Guest data is stored as records (above), packed byte-to-byte and addressed by
**compressed-cluster L2 descriptors**, whose byte offset and length are exactly
what a variable-length record needs. That descriptor can address slightly less
than two clusters, and a record is one cluster plus 100 bytes of framing, so
the cluster size is at least 4 KiB (`cluster_bits` 12–21; the default stays 16).

File layout, header written last:

```text
cluster 0     qcow2 header + feature name table + OBKH open header + backing name
clusters 1..  one sealed Data record per stored guest cluster
  ...         sealed Index records (chunk ordinal = record index, from 0)
  ...         sealed Manifest records holding the sealed metadata (index 1..n)
  ...         the sealed root record (Manifest, index 0)
  ...         L2 tables, L1 table, refcount blocks, refcount table
```

Creating an image commits when its header cluster is written: until then the
first cluster is zeros, so an interrupted image is not a qcow2 file at all.
Scratch layers, merges and their transactions are E4 and are not covered here.

### Open header (`OBKH`, qcow2 header extension 0x4F424B48)

UTF-8 JSON, at most 32 KiB, rejecting unknown fields. It carries `format`
(`own-backup-obk`), `scheme`, `chain_id`, `backup_id`, `parent_backup_id`,
`role`, `created_at`, `virtual_size`, `cluster_bits`, the base64 wrapped chain
key, the portable private-job-key document, and `root` = the root record's
`offset`, `len`, `session` and `nonce`.

It exists so a listing can show a backup without any key, and it proves
nothing: every field is compared with the sealed root record before an image is
used, and a disagreement refuses the image. The service's local DPAPI envelope
is never here. The portable envelope is in **every** image of a chain, so a
different machine needs only the files and a password or the recovery key.

### Root record

Manifest record with logical index 0, payload:

| Offset | Bytes | Field |
|---|---:|---|
| 0 | 4 | ASCII `OBKT` |
| 4 | 2 | Version, 2 |
| 6 | 1 | Role: Full=1, Incremental=2, Differential=3 |
| 7 | 1 | Reserved, must be 0 |
| 8 | 16 | Chain identity |
| 24 | 16 | Backup identity |
| 40 | 16 | Parent backup identity, all zero for a full backup |
| 56 | 8 | Virtual size |
| 64 | 4 | `cluster_bits` |
| 68 | 8 | Guest cluster count |
| 76 | 8 | Clusters this image defines (stored or explicitly zero) |
| 84 | 8 | Sealed metadata length, decoded, across its chunks |
| 92 | 4 | Index chunk count |
| 96 | 4 | Metadata chunk count |
| 100 | 68 each | Index chunk refs |
| … | 56 each | Metadata chunk refs |

An index chunk ref is `session[32] || nonce[12] || offset_u64 || len_u32 ||
entries_u32 || first_cluster_u64`; a metadata chunk ref is the same without the
last two fields. Metadata chunk *k* is the Manifest record with logical index
*k+1*, so a chunk cannot be reordered even before its identity is compared.

Decoding checks that the geometry agrees with itself, that a parent is present
exactly for the roles that have one, that chunk counts and the payload length
match, that every chunk offset and length could be a record, that index chunks
are in ascending non-overlapping cluster order, and that the entry counts add
up to the declared number of defined clusters. Caps: 15 000 index chunks and
4 096 metadata chunks, both also bounded by the 1 MiB record limit.

### Authenticated index

An Index record's payload is a fixed 53-byte stride, ascending by cluster:

| Offset | Bytes | Field |
|---|---:|---|
| 0 | 8 | Guest cluster |
| 8 | 1 | State: Present=1, Zero=2 |
| 9 | 32 | Record session identity (zero for Zero) |
| 41 | 12 | Record nonce (zero for Zero) |

At most 18 000 entries per chunk. A cluster with no entry is **absent**: the
parent answers, or it reads as zeros in a full backup. A `Zero` entry is an
explicit zero that overrides the parent and costs no record and no nonce. Zero
entries must carry an all-zero identity, and a `Present` nonce must be a valid
counter nonce, or the chunk is refused.

Chunks partition the guest clusters: chunk *k* owns
`[first_cluster(k), first_cluster(k+1))`, the last one up to the disk's end, and
an entry outside its chunk's range is refused. Two chunks therefore can never
answer for one cluster.

### Reading, and what is refused

1. The qcow2 header must be exactly the shape above.
2. The root record must open under the chain key and agree with the open header
   on chain, backup and parent identity, role and geometry.
3. An index chunk must be the record the root named, in the range the root gave
   it, starting at the cluster the root claims.
4. A data record must be the record the **index** named, and the L2 entry must
   agree with the index: absent ↔ unallocated, zero ↔ zero cluster, present ↔ a
   compressed descriptor holding exactly that record. Any other pairing is an
   error, never a read.

This is what makes an edited cluster table, a dropped or invented L2 entry, two
records exchanged inside one image, and a valid record or index chunk spliced in
from an **older backup of the same chain** all fail instead of returning data.
Truncation is not promised to be detected as such — cutting the refcount tail
leaves a perfectly readable image — but it must never change what the image
reads back.

A chain is opened by following the backing file name and then checking the
sealed identities: the parent must be the backup this image names, of the same
chain and geometry; a differential sits directly on a full backup; a
differential is never a parent; the chain ends at a full backup; and a chain
that returns to a file it already opened is reported as a loop. One chain key
opens every image, so an image of another chain cannot be grafted in at all.

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
