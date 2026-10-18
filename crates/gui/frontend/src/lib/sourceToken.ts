/** The token a source volume gets in its image file names — the window's copy
 *  of `own_backup_core::source_token`, used to preview the name a copy will
 *  get. Both must give the same answer: a preview that differs from the file
 *  the service then writes is a preview of nothing.
 *
 *  - `part:<disk>:<offset>:…` → `P<disk><offset in hex>`;
 *  - `\\?\Volume{GUID}` → `V` + the first 8 hex digits of the GUID;
 *  - a Linux device path → `U`/`F`/`H` + 8 hex digits (`linuxDeviceToken`);
 *  - a drive letter → the letter; anything else → `X`. */
export function sourceToken(device: string): string {
  const pm = device.match(/^part:(\d+):(\d+):/);
  if (pm) return `P${pm[1]}${BigInt(pm[2]).toString(16).toUpperCase()}`;
  const vm = device.match(/Volume\{([0-9a-fA-F]+)/);
  if (vm) return 'V' + vm[1].slice(0, 8).toUpperCase();
  const linux = linuxDeviceToken(device);
  if (linux) return linux;
  return driveLetter(device) ?? 'X';
}

function driveLetter(s: string): string | null {
  const t = s.trim().replace(/^\\\\[.?]\\/, '');
  return t.length >= 2 && /[a-zA-Z]/.test(t[0]) && t[1] === ':' ? t[0].toUpperCase() : null;
}

/** `own_backup_backup::source_identity::unix_device_token`:
 *  - `/dev/disk/by-partuuid/<id>` → `U` + its first 8 hex digits, all 10 for
 *    the MBR form `xxxxxxxx-nn` (the first 8 are the disk's, shared by all of
 *    its partitions);
 *  - `/dev/disk/by-uuid/<id>` → `F` + its first 8 hex digits;
 *  - any other absolute path (`/dev/mapper/<name>`) → `H` + the top 8 hex
 *    digits of xxh64 of the path's UTF-8 bytes.
 *  `null` for what is not a Linux path (`//server/share` included). */
export function linuxDeviceToken(device: string): string | null {
  const d = device.trim();
  if (!d.startsWith('/') || d.startsWith('//')) return null;
  const hex = (id: string) => id.replace(/[^0-9a-fA-F]/g, '').toUpperCase();
  const partuuid = '/dev/disk/by-partuuid/';
  if (d.startsWith(partuuid)) {
    const id = d.slice(partuuid.length);
    const take = /^[0-9a-fA-F]{8}-[0-9a-fA-F]{2}$/.test(id) ? 10 : 8;
    const digits = hex(id);
    if (digits.length >= take) return 'U' + digits.slice(0, take);
  }
  const fsuuid = '/dev/disk/by-uuid/';
  if (d.startsWith(fsuuid)) {
    const digits = hex(d.slice(fsuuid.length));
    if (digits.length >= 8) return 'F' + digits.slice(0, 8);
  }
  const hash = xxh64(new TextEncoder().encode(d));
  return 'H' + (hash >> 32n).toString(16).toUpperCase().padStart(8, '0');
}

const M64 = (1n << 64n) - 1n;
const P1 = 0x9e3779b185ebca87n;
const P2 = 0xc2b2ae3d27d4eb4fn;
const P3 = 0x165667b19e3779f9n;
const P4 = 0x85ebca77c2b2ae63n;
const P5 = 0x27d4eb2f165667c5n;

const rotl = (x: bigint, r: bigint) => ((x << r) | (x >> (64n - r))) & M64;
const round = (acc: bigint, lane: bigint) => (rotl((acc + lane * P2) & M64, 31n) * P1) & M64;
const merge = (acc: bigint, v: bigint) => ((acc ^ round(0n, v)) * P1 + P4) & M64;

/** `n` bytes at `at`, little-endian. */
function le(b: Uint8Array, at: number, n: number): bigint {
  let v = 0n;
  for (let i = n - 1; i >= 0; i--) v = (v << 8n) | BigInt(b[at + i]);
  return v;
}

/** XXH64 (the reference algorithm, as the `xxhash-rust` crate computes it).
 *  BigInt arithmetic masked to 64 bits: slow, and a device path is short. */
export function xxh64(input: Uint8Array, seed = 0n): bigint {
  const len = input.length;
  let i = 0;
  let h: bigint;
  if (len >= 32) {
    let v1 = (seed + P1 + P2) & M64;
    let v2 = (seed + P2) & M64;
    let v3 = seed & M64;
    let v4 = (seed - P1) & M64;
    for (; i + 32 <= len; i += 32) {
      v1 = round(v1, le(input, i, 8));
      v2 = round(v2, le(input, i + 8, 8));
      v3 = round(v3, le(input, i + 16, 8));
      v4 = round(v4, le(input, i + 24, 8));
    }
    h = (rotl(v1, 1n) + rotl(v2, 7n) + rotl(v3, 12n) + rotl(v4, 18n)) & M64;
    h = merge(merge(merge(merge(h, v1), v2), v3), v4);
  } else {
    h = (seed + P5) & M64;
  }
  h = (h + BigInt(len)) & M64;
  for (; i + 8 <= len; i += 8) {
    h ^= round(0n, le(input, i, 8));
    h = (rotl(h, 27n) * P1 + P4) & M64;
  }
  if (i + 4 <= len) {
    h ^= (le(input, i, 4) * P1) & M64;
    h = (rotl(h, 23n) * P2 + P3) & M64;
    i += 4;
  }
  for (; i < len; i++) {
    h ^= (BigInt(input[i]) * P5) & M64;
    h = (rotl(h, 11n) * P1) & M64;
  }
  h ^= h >> 33n;
  h = (h * P2) & M64;
  h ^= h >> 29n;
  h = (h * P3) & M64;
  h ^= h >> 32n;
  return h;
}
