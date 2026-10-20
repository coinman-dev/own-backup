//! Reading a built initramfs image back, to check what a rebuild recorded
//! (PLAN §6, owner decision 13; used by `initramfs_linux`).
//!
//! An image is what the kernel unpacks: one or more uncompressed `newc` cpio
//! archives (the early microcode part dracut and initramfs-tools put first),
//! each padded with zeros, then the main archive, usually compressed. zstd
//! and gzip are read here; xz, lzma, bzip2, lz4 and lzop through this
//! machine's own decompressor, when it has one. An image that cannot be read
//! is an error, never an image without findings.
//!
//! What is looked for: this machine's names for block devices — kernel names
//! (`/dev/vdb13`, `/dev/dm-3`), `/dev/block/<major>:<minor>`, the restore's
//! own temporary mappings (`/dev/mapper/obk-…`) — in the parts of an image
//! that make the boot wait for or mount a device: dracut's hooks
//! (`initqueue/finished/devexists-\x2fdev\x2fvdb13.sh`), the command line it
//! stores (`etc/cmdline.d`), the systemd units it adds
//! (`dev-vdb13.device.d/timeout.conf`), fstab and crypttab copies, and
//! initramfs-tools' `conf/`. On the machine the restored system boots on,
//! those names are other devices or none: dracut waits for them until it
//! gives up.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};

/// A file kept from an image: one of the files [`checked`] selects, with its
/// content (a symbolic link's content is its target).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The path inside the image, without a leading `./` or `/`.
    pub name: String,
    pub data: Vec<u8>,
}

/// Larger files are kept without their content: what is checked is small.
const KEEP_MAX: u64 = 1 << 20;

/// The parts of an image that can make its boot wait for a device or name
/// one: dracut's hooks, stored command line and units, fstab and crypttab
/// copies, initramfs-tools' configuration and cryptsetup's crypttab.
pub fn checked(name: &str) -> bool {
    name.contains("dracut/hooks/")
        || name.starts_with("etc/cmdline.d/")
        || name.starts_with("etc/systemd/system/")
        || name.starts_with("conf/")
        || name.starts_with("cryptroot/")
        || matches!(
            name,
            "etc/cmdline" | "etc/fstab" | "etc/crypttab" | "etc/block_uuid.map"
        )
}

/// A reader that can look ahead: cpio headers and compression magic are
/// recognised before they are consumed.
struct Peek<R> {
    inner: R,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
    /// Bytes consumed so far (where the next byte lies in the stream).
    offset: u64,
}

impl<R: Read> Peek<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            pos: 0,
            eof: false,
            offset: 0,
        }
    }

    /// Up to `n` bytes ahead (fewer only at the end).
    fn peek(&mut self, n: usize) -> std::io::Result<&[u8]> {
        while self.buf.len() - self.pos < n && !self.eof {
            if self.pos > 0 {
                self.buf.drain(..self.pos);
                self.pos = 0;
            }
            let have = self.buf.len();
            self.buf.resize(have + n.max(64 << 10), 0);
            let got = loop {
                match self.inner.read(&mut self.buf[have..]) {
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    r => break r,
                }
            };
            match got {
                Ok(0) => {
                    self.buf.truncate(have);
                    self.eof = true;
                }
                Ok(k) => self.buf.truncate(have + k),
                Err(e) => {
                    self.buf.truncate(have);
                    return Err(e);
                }
            }
        }
        let end = (self.pos + n).min(self.buf.len());
        Ok(&self.buf[self.pos..end])
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
        self.offset += n as u64;
    }

    /// Exactly `n` bytes, or an error naming `what`.
    fn take(&mut self, n: usize, what: &str) -> Result<Vec<u8>, String> {
        let got = self.peek(n).map_err(|e| format!("{what}: {e}"))?.to_vec();
        if got.len() < n {
            return Err(format!("{what}: the image ends early"));
        }
        self.consume(n);
        Ok(got)
    }

    fn skip(&mut self, mut n: u64, what: &str) -> Result<(), String> {
        while n > 0 {
            let step = n.min(1 << 20) as usize;
            let got = self.peek(step).map_err(|e| format!("{what}: {e}"))?.len();
            if got == 0 {
                return Err(format!("{what}: the image ends early"));
            }
            self.consume(got);
            n -= got as u64;
        }
        Ok(())
    }

    /// Past the zeros that pad one archive from the next.
    fn skip_zeros(&mut self) -> std::io::Result<()> {
        loop {
            let ahead = self.peek(4096)?;
            if ahead.is_empty() {
                return Ok(());
            }
            let zeros = ahead.iter().take_while(|b| **b == 0).count();
            let all = zeros == ahead.len();
            self.consume(zeros);
            if !all {
                return Ok(());
            }
        }
    }
}

impl<R: Read> Read for Peek<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.buf.len() {
            let n = out.len().min(self.buf.len() - self.pos);
            out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.consume(n);
            return Ok(n);
        }
        let n = self.inner.read(out)?;
        self.offset += n as u64;
        Ok(n)
    }
}

fn is_newc(head: &[u8]) -> bool {
    head == b"070701" || head == b"070702"
}

fn hex_field(h: &[u8], i: usize) -> Result<u64, String> {
    let s = std::str::from_utf8(&h[6 + 8 * i..6 + 8 * (i + 1)]).map_err(|e| e.to_string())?;
    u64::from_str_radix(s, 16).map_err(|_| format!("a cpio header field is not hex: {s:?}"))
}

fn pad4(n: u64) -> u64 {
    (4 - n % 4) % 4
}

/// One `newc` archive up to its trailer; the files [`checked`] selects are
/// added to `out`.
fn read_archive<R: Read>(p: &mut Peek<R>, out: &mut Vec<Entry>) -> Result<(), String> {
    loop {
        let h = p.take(110, "cpio header")?;
        if !is_newc(&h[..6]) {
            return Err("a cpio header without the newc magic".into());
        }
        let filesize = hex_field(&h, 6)?;
        let namesize = hex_field(&h, 11)?;
        if namesize == 0 || namesize > 4096 {
            return Err(format!("a cpio name of {namesize} bytes"));
        }
        let raw = p.take(namesize as usize, "cpio name")?;
        p.skip(pad4(110 + namesize), "cpio padding")?;
        let raw = raw.strip_suffix(&[0]).unwrap_or(&raw);
        let name = String::from_utf8_lossy(raw);
        if name == "TRAILER!!!" {
            return Ok(());
        }
        let name = name.trim_start_matches("./").trim_start_matches('/');
        if checked(name) && filesize <= KEEP_MAX {
            let data = p.take(filesize as usize, "cpio data")?;
            out.push(Entry {
                name: name.to_string(),
                data,
            });
        } else {
            if checked(name) {
                out.push(Entry {
                    name: name.to_string(),
                    data: Vec::new(),
                });
            }
            p.skip(filesize, "cpio data")?;
        }
        p.skip(pad4(filesize), "cpio padding")?;
    }
}

/// The archives of a decompressed stream. What follows the last archive —
/// more padding, a decompressor complaining about trailing bytes — is not
/// read: the image's files are all there by then.
fn read_stream<R: Read>(r: R, out: &mut Vec<Entry>) -> Result<(), String> {
    let mut p = Peek::new(r);
    let mut archives = 0;
    loop {
        let head = match p.skip_zeros().and_then(|_| p.peek(6)) {
            Ok(h) => h,
            Err(_) if archives > 0 => break,
            Err(e) => return Err(e.to_string()),
        };
        if head.is_empty() {
            break;
        }
        if !is_newc(head) {
            if archives > 0 {
                break;
            }
            return Err("the decompressed data is not a cpio archive".into());
        }
        read_archive(&mut p, out)?;
        archives += 1;
    }
    if archives == 0 {
        return Err("no cpio archive".into());
    }
    Ok(())
}

/// How the main archive is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Zstd,
    Gzip,
    Xz,
    Lzma,
    Bzip2,
    Lz4,
    Lzop,
}

impl Compression {
    /// By its magic.
    pub fn detect(head: &[u8]) -> Option<Self> {
        let starts = |m: &[u8]| head.starts_with(m);
        Some(if starts(&[0x28, 0xb5, 0x2f, 0xfd]) {
            Self::Zstd
        } else if starts(&[0x1f, 0x8b]) || starts(&[0x1f, 0x9e]) {
            Self::Gzip
        } else if starts(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
            Self::Xz
        } else if starts(&[0x5d, 0, 0]) {
            Self::Lzma
        } else if starts(b"BZh") {
            Self::Bzip2
        } else if starts(&[0x02, 0x21, 0x4c, 0x18]) || starts(&[0x04, 0x22, 0x4d, 0x18]) {
            Self::Lz4
        } else if starts(&[0x89, b'L', b'Z', b'O']) {
            Self::Lzop
        } else {
            return None;
        })
    }

    /// This machine's decompressor for the ones not read here.
    fn command(self) -> Option<(&'static str, &'static [&'static str])> {
        match self {
            Self::Zstd | Self::Gzip => None,
            Self::Xz => Some(("xz", &["-dc"])),
            Self::Lzma => Some(("xz", &["--format=lzma", "-dc"])),
            Self::Bzip2 => Some(("bzip2", &["-dc"])),
            Self::Lz4 => Some(("lz4", &["-dc"])),
            Self::Lzop => Some(("lzop", &["-dc"])),
        }
    }
}

/// The files [`checked`] selects, from the image at `path`.
pub fn read_image(path: &Path) -> Result<Vec<Entry>, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut p = Peek::new(file);
    let mut out = Vec::new();
    loop {
        p.skip_zeros().map_err(|e| e.to_string())?;
        let at = p.offset;
        let head = p.peek(6).map_err(|e| e.to_string())?;
        if head.is_empty() {
            if out.is_empty() && at == 0 {
                return Err("an empty image".into());
            }
            return Ok(out);
        }
        if is_newc(head) {
            read_archive(&mut p, &mut out)?;
            continue;
        }
        let Some(c) = Compression::detect(head) else {
            return Err(format!(
                "unknown data at byte {at} ({:02x?})",
                &head[..head.len().min(6)]
            ));
        };
        match c {
            Compression::Zstd => {
                let mut d = zstd::stream::read::Decoder::new(p).map_err(|e| e.to_string())?;
                // dracut compresses with `--long`: windows past the default.
                d.window_log_max(31).map_err(|e| e.to_string())?;
                read_stream(d, &mut out)?;
            }
            Compression::Gzip => read_stream(flate2::read::MultiGzDecoder::new(p), &mut out)?,
            _ => {
                let offset = p.offset;
                drop(p);
                read_with_command(path, offset, c, &mut out)?;
            }
        }
        return Ok(out);
    }
}

/// The main archive from `offset` on, through this machine's decompressor.
fn read_with_command(
    path: &Path,
    offset: u64,
    c: Compression,
    out: &mut Vec<Entry>,
) -> Result<(), String> {
    let Some((program, args)) = c.command() else {
        return Err(format!("{c:?}: no decompressor"));
    };
    let mut input = std::fs::File::open(path).map_err(|e| e.to_string())?;
    input
        .seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    let mut child = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::from(input))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{c:?} compression needs {program}: {e}"))?;
    let stdout = child.stdout.take().expect("piped");
    let read = read_stream(stdout, out);
    // Done reading: the decompressor may still be writing what follows.
    let _ = child.kill();
    let _ = child.wait();
    read.map_err(|e| format!("{program}: {e}"))
}

/// Names this machine gives block devices, as device paths: `/dev/<kernel
/// name>`, `/dev/block/<major>:<minor>`, and `/dev/mapper/<name>` of the
/// restore's own temporary mappings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachineDevices {
    pub paths: Vec<String>,
}

/// The restore's temporary device-mapper names start with this.
const OWN_MAPPING_PREFIX: &str = "obk-";

impl MachineDevices {
    /// The block devices `/sys/class/block` lists now.
    pub fn read() -> Self {
        let mut paths = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/sys/class/block") {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Ok(dev) = std::fs::read_to_string(e.path().join("dev")) {
                    paths.push(format!("/dev/block/{}", dev.trim()));
                }
                if let Ok(dm) = std::fs::read_to_string(e.path().join("dm/name")) {
                    let dm = dm.trim();
                    if dm.starts_with(OWN_MAPPING_PREFIX) {
                        paths.push(format!("/dev/mapper/{dm}"));
                    }
                }
                paths.push(format!("/dev/{name}"));
            }
        }
        paths.sort();
        paths.dedup();
        Self { paths }
    }
}

/// systemd's escaping of a path into a unit name (`systemd-escape -p`):
/// `/dev/dm-3` is `dev-dm\x2d3`.
fn unit_escape(path: &str) -> String {
    let mut out = String::new();
    for (i, c) in path.trim_matches('/').chars().enumerate() {
        match c {
            '/' => out.push('-'),
            '.' if i == 0 => out.push_str("\\x2e"),
            c if c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.') => out.push(c),
            c => {
                let mut b = [0u8; 4];
                for byte in c.encode_utf8(&mut b).bytes() {
                    out.push_str(&format!("\\x{byte:02x}"));
                }
            }
        }
    }
    out
}

/// Does `text` name `path` — as written, as dracut's hook names escape it
/// (`\x2fdev\x2fvdb13`), or as a systemd unit (`dev-vdb13.device`)? The
/// name must end there: `/dev/vdb1` is not in `/dev/vdb13`.
fn names(text: &str, path: &str) -> bool {
    let ends = |rest: &str| !rest.starts_with(|c: char| c.is_ascii_alphanumeric());
    let found = |needle: &str, unit: bool| {
        text.match_indices(needle).any(|(i, _)| {
            let before_ok = !unit
                || !text[..i]
                    .ends_with(|c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '\\'));
            before_ok && ends(&text[i + needle.len()..])
        })
    };
    found(path, false)
        || found(&path.replace('/', "\\x2f"), false)
        || found(&unit_escape(path), true)
}

/// The machine's device paths each entry names, in its name or its content:
/// `(entry, device path)`.
pub fn device_refs(entries: &[Entry], devices: &MachineDevices) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for e in entries {
        let text = String::from_utf8_lossy(&e.data);
        for p in &devices.paths {
            if names(&e.name, p) || names(&text, p) {
                out.push((e.name.clone(), p.clone()));
            }
        }
    }
    out
}

/// What a rebuilt image names of the machine's devices that the image it
/// replaced did not (`previous`: `None` when there was none): those names
/// came from this machine. A name the previous image had already is the
/// restored system's own (its crypttab naming `/dev/sda2` on purpose).
pub fn new_device_refs(
    rebuilt: &[Entry],
    previous: Option<&[Entry]>,
    devices: &MachineDevices,
) -> Vec<(String, String)> {
    let before: BTreeSet<String> = previous
        .map(|p| {
            device_refs(p, devices)
                .into_iter()
                .map(|(_, d)| d)
                .collect()
        })
        .unwrap_or_default();
    device_refs(rebuilt, devices)
        .into_iter()
        .filter(|(_, d)| !before.contains(d))
        .collect()
}

/// Images for tests here and in `initramfs_linux`.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::pad4;

    /// A `newc` archive of `(name, mode, data)`, as the kernel reads it.
    pub(crate) fn cpio(files: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut add = |name: &str, mode: u32, data: &[u8]| {
            let n = name.len() + 1;
            out.extend_from_slice(
                format!(
                    "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
                    1, mode, 0, 0, 1, 0, data.len(), 0, 0, 0, 0, n, 0
                )
                .as_bytes(),
            );
            out.extend_from_slice(name.as_bytes());
            out.push(0);
            out.resize(out.len() + pad4(110 + n as u64) as usize, 0);
            out.extend_from_slice(data);
            out.resize(out.len() + pad4(data.len() as u64) as usize, 0);
        };
        for (name, mode, data) in files {
            add(name, *mode, data);
        }
        add("TRAILER!!!", 0, b"");
        out
    }

    /// A zstd image waiting for `devices` the way dracut writes it
    /// (`initqueue/finished/devexists-…`).
    pub(crate) fn waiting_for(devices: &[&str]) -> Vec<u8> {
        let files: Vec<(String, String)> = devices
            .iter()
            .map(|d| {
                (
                    format!(
                        "var/lib/dracut/hooks/initqueue/finished/devexists-{}.sh",
                        d.replace('/', "\\x2f")
                    ),
                    format!("[ -e \"{d}\" ]\n"),
                )
            })
            .collect();
        let files: Vec<(&str, u32, &[u8])> = files
            .iter()
            .map(|(n, t)| (n.as_str(), 0o100644, t.as_bytes()))
            .collect();
        zstd::encode_all(&cpio(&files)[..], 3).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::cpio;
    use super::*;
    use std::io::Write;

    const HOOK: &str = "var/lib/dracut/hooks/initqueue/finished/devexists-\\x2fdev\\x2fvdb13.sh";

    /// The image dracut 110 built for the stand's restored Ubuntu under the
    /// restore's hold: waits for `/dev/vdb13` and `/dev/vdb15`.
    fn main_archive(boot: &str, esp: &str) -> Vec<u8> {
        let boot_hook = format!(
            "var/lib/dracut/hooks/initqueue/finished/devexists-{}.sh",
            boot.replace('/', "\\x2f")
        );
        let unit = format!(
            "etc/systemd/system/initrd.target.wants/{}.device",
            unit_escape(esp)
        );
        let boot_test = format!("[ -e \"{boot}\" ]\n");
        let root = "root=UUID=99999999-8888-4777-8666-555555555555 rootfstype=ext4\n";
        let big = vec![b'x'; 3 << 20];
        cpio(&[
            ("usr", 0o40755, b""),
            ("usr/lib/modules/7.0.0/big.ko", 0o100644, &big),
            (&boot_hook, 0o100644, boot_test.as_bytes()),
            (&unit, 0o120777, b"../dev.device"),
            ("etc/cmdline.d/20-root-dev.conf", 0o100644, root.as_bytes()),
            ("etc/os-release", 0o100644, b"ID=ubuntu\n"),
        ])
    }

    fn early() -> Vec<u8> {
        let mut e = cpio(&[
            ("kernel", 0o40755, b""),
            (
                "kernel/x86/microcode/GenuineIntel.bin",
                0o100644,
                &[7u8; 1000],
            ),
            ("early_cpio", 0o100644, b"1\n"),
        ]);
        // Padded to the next 512 bytes, as dracut does.
        e.resize(e.len().div_ceil(512) * 512, 0);
        e
    }

    fn stand_devices() -> MachineDevices {
        MachineDevices {
            paths: [
                "/dev/vda",
                "/dev/vda1",
                "/dev/vda13",
                "/dev/vda15",
                "/dev/vdb",
                "/dev/vdb1",
                "/dev/vdb13",
                "/dev/vdb15",
                "/dev/dm-0",
                "/dev/block/252:29",
                "/dev/mapper/obk-reid-77-1",
            ]
            .map(String::from)
            .to_vec(),
        }
    }

    fn write(dir: &Path, name: &str, data: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, data).unwrap();
        p
    }

    fn refs(img: &Path) -> Vec<String> {
        let entries = read_image(img).unwrap();
        let mut d: Vec<String> = device_refs(&entries, &stand_devices())
            .into_iter()
            .map(|(_, d)| d)
            .collect();
        d.sort();
        d.dedup();
        d
    }

    #[test]
    fn an_early_cpio_then_a_zstd_or_gzip_main_archive_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let main = main_archive("/dev/vdb13", "/dev/vdb15");

        let mut zst = early();
        zst.extend(zstd::encode_all(&main[..], 3).unwrap());
        let img = write(dir.path(), "initrd.zst", &zst);
        let entries = read_image(&img).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&HOOK), "{names:?}");
        assert!(names.contains(&"etc/cmdline.d/20-root-dev.conf"));
        assert!(!names
            .iter()
            .any(|n| n.contains("microcode") || n.ends_with(".ko")));
        assert_eq!(refs(&img), vec!["/dev/vdb13", "/dev/vdb15"]);

        let mut gz = early();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&main).unwrap();
        gz.extend(enc.finish().unwrap());
        let img = write(dir.path(), "initrd.gz", &gz);
        assert_eq!(refs(&img), vec!["/dev/vdb13", "/dev/vdb15"]);

        // Uncompressed, without an early part.
        let img = write(dir.path(), "initrd.cpio", &main);
        assert_eq!(refs(&img), vec!["/dev/vdb13", "/dev/vdb15"]);
    }

    /// The decompressors read through this machine's programs, when it has
    /// them (xz is in every Debian and Ubuntu).
    #[test]
    fn xz_goes_through_this_machines_decompressor() {
        if Command::new("xz").arg("--version").output().is_err() {
            eprintln!("no xz here: skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let main = write(
            dir.path(),
            "main",
            &main_archive("/dev/vdb13", "/dev/vdb15"),
        );
        let ok = Command::new("xz")
            .args(["-k", "--check=crc32", "-0"])
            .arg(&main)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let mut img = early();
        img.extend(std::fs::read(dir.path().join("main.xz")).unwrap());
        let img = write(dir.path(), "initrd.xz", &img);
        assert_eq!(refs(&img), vec!["/dev/vdb13", "/dev/vdb15"]);
    }

    /// An archive GNU cpio wrote, compressed by the zstd program with a long
    /// window (dracut's `--long`), behind an early part.
    #[test]
    fn gnu_cpio_and_zstd_long_as_the_tools_write_them() {
        let have = |p: &str| Command::new(p).arg("--version").output().is_ok();
        if !have("cpio") || !have("zstd") {
            eprintln!("no cpio or zstd here: skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        let hooks = tree.join("var/lib/dracut/hooks/initqueue/finished");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::create_dir_all(tree.join("etc/cmdline.d")).unwrap();
        std::fs::write(
            hooks.join("devexists-\\x2fdev\\x2fvdb15.sh"),
            "[ -e \"/dev/vdb15\" ]\n",
        )
        .unwrap();
        std::fs::write(
            tree.join("etc/cmdline.d/20-root-dev.conf"),
            " root=/dev/block/252:29\n",
        )
        .unwrap();
        std::fs::write(tree.join("odd-size"), [1u8; 1001]).unwrap();
        let script = "find . | LC_ALL=C sort | cpio -o -H newc --quiet | zstd -q -3 --long=31 -c > ../main.zst";
        let ok = Command::new("sh")
            .args(["-c", script])
            .current_dir(&tree)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let mut img = early();
        img.extend(std::fs::read(dir.path().join("main.zst")).unwrap());
        let img = write(dir.path(), "initrd", &img);
        assert_eq!(refs(&img), vec!["/dev/block/252:29", "/dev/vdb15"]);
    }

    #[test]
    fn what_cannot_be_read_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let img = write(dir.path(), "junk", b"\x7fELF not an image");
        assert!(read_image(&img).is_err());
        let img = write(dir.path(), "empty", b"");
        assert!(read_image(&img).is_err());
        // Cut short inside the main archive.
        let mut cut = early();
        let main = zstd::encode_all(&main_archive("/dev/vdb13", "/dev/vdb15")[..], 3).unwrap();
        cut.extend(&main[..main.len() / 2]);
        let img = write(dir.path(), "cut", &cut);
        assert!(read_image(&img).is_err());
        // An early part alone is an image (the kernel unpacks it), with
        // nothing to check in it.
        let img = write(dir.path(), "early", &early());
        assert_eq!(read_image(&img).unwrap(), Vec::new());
    }

    #[test]
    fn names_are_whole_and_found_in_every_spelling() {
        let d = |p: &str| MachineDevices {
            paths: vec![p.to_string()],
        };
        let found = |text: &str, p: &str| {
            !device_refs(
                &[Entry {
                    name: "etc/cmdline.d/x.conf".into(),
                    data: text.as_bytes().to_vec(),
                }],
                &d(p),
            )
            .is_empty()
        };
        assert!(found("[ -e \"/dev/vdb13\" ]", "/dev/vdb13"));
        assert!(
            !found("[ -e \"/dev/vdb13\" ]", "/dev/vdb1"),
            "a prefix only"
        );
        assert!(found("root=/dev/vdb1 ro", "/dev/vdb1"));
        assert!(found("root=/dev/block/252:29\n", "/dev/block/252:29"));
        assert!(!found("root=/dev/block/252:291\n", "/dev/block/252:29"));
        assert!(found("dev-dm\\x2d0.device", "/dev/dm-0"));
        assert!(!found("dev-dm\\x2d01.device", "/dev/dm-0"));
        assert!(found(
            "root=/dev/mapper/obk-reid-77-1",
            "/dev/mapper/obk-reid-77-1"
        ));
        assert!(found("x \\x2fdev\\x2fnvme0n1p2.sh", "/dev/nvme0n1p2"));
        assert!(!found("/dev/disk/by-uuid/0F0E-0D0C", "/dev/vdb"));
        assert!(!found(
            "dev-disk-by\\x2duuid-0F0E\\x2d0D0C.device",
            "/dev/vdb"
        ));
        assert!(!found("xdev-vdb.device", "/dev/vdb"), "inside another name");
        assert_eq!(unit_escape("/dev/block/252:29"), "dev-block-252:29");
        assert_eq!(unit_escape("/dev/mapper/obk-1"), "dev-mapper-obk\\x2d1");
    }

    /// The stand's failure: the rebuilt image waits for the kernel names the
    /// restored /boot and ESP had here; the same image built with
    /// /dev/disk/by-uuid links names none. A name the replaced image already
    /// had is the system's own.
    #[test]
    fn only_names_the_rebuild_brought_in_count() {
        let dir = tempfile::tempdir().unwrap();
        let zst = |a: Vec<u8>| {
            let mut i = early();
            i.extend(zstd::encode_all(&a[..], 3).unwrap());
            i
        };
        let bad = write(
            dir.path(),
            "bad",
            &zst(main_archive("/dev/vdb13", "/dev/vdb15")),
        );
        let good = write(
            dir.path(),
            "good",
            &zst(main_archive(
                "/dev/disk/by-uuid/77777777-1111-4222-8333-444444444444",
                "/dev/disk/by-uuid/0F0E-0D0C",
            )),
        );
        let devices = stand_devices();
        let bad = read_image(&bad).unwrap();
        let good = read_image(&good).unwrap();
        let found = new_device_refs(&bad, Some(&good), &devices);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.contains(&(HOOK.to_string(), "/dev/vdb13".to_string())));
        assert!(new_device_refs(&good, Some(&bad), &devices).is_empty());
        assert!(new_device_refs(&good, None, &devices).is_empty());
        assert!(
            new_device_refs(&bad, Some(&bad), &devices).is_empty(),
            "the replaced image named them already"
        );
        assert_eq!(new_device_refs(&bad, None, &devices).len(), 2);
    }
}
