//! The shared test corpus for the dynamic (root) acceptance of the exclusion
//! scanners — пункт 1 этапа T of
//! `.ai/old2/exclusions-full-repair-plan_2026-07-28.md`.
//!
//! Lesson 2 of the btrfs post-mortem was «тестовая ФС должна быть похожа на
//! настоящую»: a scanner that only ever meets three files in a flat directory
//! passes tests a real `/var` would fail on the first day. So this is ONE
//! definition of the zoo — dense directories, nested dense directories,
//! hard-link pairs inside and across the exclusion, device nodes, FIFOs,
//! sockets, fast and slow symlinks, shared and private xattr blocks, reflink
//! copies, and (for btrfs) enough big files to give the checksum tree real
//! internal nodes — built by the KERNEL through a mount, not assembled by
//! debugfs, and shared by ext4, xfs, btrfs and NTFS (through ntfs-3g) alike.
//!
//! Everything under `cache/` is meant to be excluded, everything under `keep/`
//! must come out untouched. `Corpus::verify_kept` is the half every filesystem
//! shares; what happens to the excluded half differs (btrfs and ext remove the
//! entries, xfs only zeroes the data), so those assertions stay in the tests.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// What the filesystem under test can actually do.
#[derive(Clone, Copy)]
pub struct Caps {
    /// Extended attributes big enough to need an out-of-inode block. Only ext
    /// keeps those in a block with a refcount, but setting them everywhere
    /// costs nothing and proves the other scanners walk past them safely.
    pub xattr: bool,
    /// `cp --reflink=always` — btrfs always, xfs with `reflink=1`.
    pub reflink: bool,
    /// How many 1 MiB files to add on each side. btrfs needs ~200 for its
    /// checksum tree to grow internal nodes (a flat tree has no parent key to
    /// get wrong — the C-1 regression hid behind exactly that); ext and xfs
    /// are fine with none.
    pub big_files: usize,
    /// Named data streams (NTFS ADS, `user.*` through ntfs-3g) on excluded
    /// and kept files — in clusters of their own, inside the file record, and
    /// so many that the record overflows into extension records behind an
    /// attribute list.
    pub streams: bool,
}

impl Caps {
    pub const fn ext() -> Self {
        Caps {
            xattr: true,
            reflink: false,
            big_files: 0,
            streams: false,
        }
    }
    pub const fn xfs() -> Self {
        Caps {
            xattr: true,
            reflink: true,
            big_files: 0,
            streams: false,
        }
    }
    pub const fn btrfs() -> Self {
        Caps {
            xattr: true,
            reflink: true,
            big_files: 200,
            streams: false,
        }
    }
    /// NTFS through ntfs-3g: no `trusted.*` attributes (its xattrs are named
    /// streams, `user.*` only — see `streams`) and no reflinks; device nodes,
    /// FIFOs and sockets become Interix files, links reparse points, hard
    /// links real ones.
    pub const fn ntfs() -> Self {
        Caps {
            xattr: false,
            reflink: false,
            big_files: 0,
            streams: true,
        }
    }
}

/// What was built, and what must survive the exclusion untouched.
pub struct Corpus {
    /// Files under `keep/` → their exact contents.
    pub kept: Vec<(PathBuf, Vec<u8>)>,
    /// Non-regular files under `keep/` → the `S_IFMT` bits they must keep.
    pub kept_specials: Vec<(PathBuf, u32)>,
    /// Symlinks under `keep/` → their targets.
    pub kept_links: Vec<(PathBuf, String)>,
    /// Names under `cache/` the exclusion is expected to take away.
    pub excluded: Vec<PathBuf>,
    /// The hard-linked name inside `cache/` whose twin lives in `keep/`: a
    /// removable-set invariant says it stays (and keeps `cache/` alive with it).
    pub straddling_link: PathBuf,
    /// Named streams of kept files (`user.*` attribute name) → their exact
    /// contents.
    pub kept_streams: Vec<(PathBuf, String, Vec<u8>)>,
    /// Byte strings only excluded files held — content too small for a 4 KiB
    /// run of the excluded fill (resident data, short streams): none of them
    /// may be found anywhere in the image.
    pub secrets: Vec<Vec<u8>>,
}

/// Content that says which file it came from, so a block landing in the wrong
/// file fails a comparison instead of reading as plausible.
pub fn pattern(seed: u32, len: usize) -> Vec<u8> {
    (0..len)
        .map(|j| (seed as usize).wrapping_mul(31).wrapping_add(j) as u8)
        .collect()
}

pub fn mount(img: &Path, mnt: &Path) {
    let out = std::process::Command::new("mount")
        .arg("-o")
        .arg("loop")
        .arg(img)
        .arg(mnt)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "mount: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

pub fn umount(mnt: &Path) {
    let _ = std::process::Command::new("sync").status();
    let out = std::process::Command::new("umount")
        .arg(mnt)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "umount: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Unmounts on drop: an assert firing mid-test must not leave a loop mount
/// (and an undeletable tempdir) behind.
pub struct Mounted(pub PathBuf);
impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = std::process::Command::new("sync").status();
        let _ = std::process::Command::new("umount").arg(&self.0).status();
    }
}

pub fn mounted(img: &Path, mnt: &Path) -> Mounted {
    mount(img, mnt);
    Mounted(mnt.to_path_buf())
}

fn cstr(p: &Path) -> std::ffi::CString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap()
}

/// Attributes go through libc rather than `setfattr`: the `attr` package is not
/// everywhere, and the kernel is the only thing being tested anyway.
pub fn setxattr(path: &Path, name: &str, value: &[u8]) {
    let p = cstr(path);
    let n = std::ffi::CString::new(name).unwrap();
    // SAFETY: both strings are NUL-terminated and the buffer outlives the call.
    let rc = unsafe {
        libc::setxattr(
            p.as_ptr(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    assert_eq!(
        rc,
        0,
        "setxattr {name} на {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

pub fn getxattr(path: &Path, name: &str) -> Vec<u8> {
    let p = cstr(path);
    let n = std::ffi::CString::new(name).unwrap();
    let mut buf = vec![0u8; 65536];
    // SAFETY: as above; the buffer is `buf.len()` bytes long.
    let got = unsafe { libc::getxattr(p.as_ptr(), n.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    assert!(
        got >= 0,
        "getxattr {name}: {}",
        std::io::Error::last_os_error()
    );
    buf.truncate(got as usize);
    buf
}

/// The `l` variant: `setxattr` FOLLOWS symlinks, so attaching an attribute to
/// the link itself (which is what gives a fast symlink an xattr block — the P2
/// regression) needs `lsetxattr`, or the call lands on the missing target.
pub fn lsetxattr(path: &Path, name: &str, value: &[u8]) {
    let p = cstr(path);
    let n = std::ffi::CString::new(name).unwrap();
    // SAFETY: both strings are NUL-terminated and the buffer outlives the call.
    let rc = unsafe {
        libc::lsetxattr(
            p.as_ptr(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    assert_eq!(
        rc,
        0,
        "lsetxattr {name} на {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

fn mknod(path: &Path, mode: u32, major: u32, minor: u32) {
    let p = cstr(path);
    // SAFETY: the path is NUL-terminated; mknod needs root, which the callers have.
    let rc = unsafe { libc::mknod(p.as_ptr(), mode, libc::makedev(major, minor)) };
    assert_eq!(
        rc,
        0,
        "mknod {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

fn mkfifo(path: &Path) {
    let p = cstr(path);
    // SAFETY: as above.
    let rc = unsafe { libc::mkfifo(p.as_ptr(), 0o600) };
    assert_eq!(
        rc,
        0,
        "mkfifo {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

fn socket(path: &Path) {
    let l = std::os::unix::net::UnixListener::bind(path).unwrap();
    drop(l); // the socket file stays behind, which is the point
}

fn reflink(from: &Path, to: &Path) {
    let out = std::process::Command::new("cp")
        .arg("--reflink=always")
        .arg(from)
        .arg(to)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "reflink: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A target too long to live inside an inode (ext's «slow» symlink: ≥ 60 bytes
/// means a data block, which the scanner must walk as a block map — and NOT
/// walk for the short one, whose target text sits where block pointers would).
const SLOW_TARGET: &str = "../keep/very/long/target/path/that/does/not/fit/inside/an/inode/\
                           at/all/because/it/is/way/past/sixty/bytes";

/// Build the zoo into an already-mounted filesystem. Needs root (mknod).
pub fn populate(mnt: &Path, caps: Caps) -> Corpus {
    use std::os::unix::fs::symlink;
    let keep = mnt.join("keep");
    let cache = mnt.join("cache");
    std::fs::create_dir_all(&keep).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    let mut c = Corpus {
        kept: Vec::new(),
        kept_specials: Vec::new(),
        kept_links: Vec::new(),
        excluded: Vec::new(),
        straddling_link: cache.join("hard_in.bin"),
        kept_streams: Vec::new(),
        secrets: Vec::new(),
    };
    let write_kept = |c: &mut Corpus, p: PathBuf, data: Vec<u8>| {
        std::fs::write(&p, &data).unwrap();
        c.kept.push((p, data));
    };

    // 300 ordinary kept files, and a nested dense KEPT directory (the leaves of
    // a surviving directory must not be disturbed by a neighbour's removal).
    for i in 0..300u32 {
        write_kept(
            &mut c,
            keep.join(format!("k{i:04}.bin")),
            pattern(i, 64 * 1024),
        );
    }
    std::fs::create_dir_all(keep.join("deep/one")).unwrap();
    for i in 0..400u32 {
        write_kept(
            &mut c,
            keep.join(format!("deep/one/d{i:04}.bin")),
            pattern(1000 + i, 4096),
        );
    }

    // A dense excluded directory: 600 long names, so its dirent blocks are many
    // and (on ext) htree-indexed — coalescing has to survive that.
    let long = "x".repeat(180);
    for i in 0..600u32 {
        let p = cache.join(format!("{long}-{i:04}.tmp"));
        std::fs::write(&p, [0xAB; 4096]).unwrap();
        c.excluded.push(p);
    }
    // …and NESTED dense directories inside it: removing a subtree is not the
    // same code path as removing entries from one directory.
    for sub in ["one", "two"] {
        std::fs::create_dir_all(cache.join(sub)).unwrap();
        for i in 0..400u32 {
            let p = cache.join(format!("{sub}/n{i:04}.tmp"));
            std::fs::write(&p, [0xAB; 4096]).unwrap();
            c.excluded.push(p);
        }
    }

    // Hard links: one pair straddling the exclusion (the file must live on
    // under its kept name), one pair fully inside it (both names go).
    std::fs::write(cache.join("hard_in.bin"), pattern(777, 64 * 1024)).unwrap();
    std::fs::hard_link(cache.join("hard_in.bin"), keep.join("hard_out.bin")).unwrap();
    c.kept
        .push((keep.join("hard_out.bin"), pattern(777, 64 * 1024)));
    std::fs::write(cache.join("pair_a.bin"), pattern(778, 32 * 1024)).unwrap();
    std::fs::hard_link(cache.join("pair_a.bin"), cache.join("pair_b.bin")).unwrap();
    c.excluded.push(cache.join("pair_a.bin"));
    c.excluded.push(cache.join("pair_b.bin"));

    // Non-regular files on BOTH sides. On the excluded side a device node is
    // the P1 regression in person: its i_block holds a device number, and a
    // scanner that reads it as a block map frees somebody else's block.
    for (dir, kept) in [(&keep, true), (&cache, false)] {
        let dev_c = dir.join("dev_char");
        let dev_b = dir.join("dev_block");
        let fifo = dir.join("fifo");
        let sock = dir.join("sock");
        mknod(&dev_c, libc::S_IFCHR | 0o600, 5, 1);
        mknod(&dev_b, libc::S_IFBLK | 0o600, 7, 0);
        mkfifo(&fifo);
        socket(&sock);
        if kept {
            c.kept_specials.push((dev_c, libc::S_IFCHR));
            c.kept_specials.push((dev_b, libc::S_IFBLK));
            c.kept_specials.push((fifo, libc::S_IFIFO));
            c.kept_specials.push((sock, libc::S_IFSOCK));
        } else {
            c.excluded.extend([dev_c, dev_b, fifo, sock]);
        }
    }

    // Fast and slow symlinks on both sides; on the excluded side the fast one
    // also carries an xattr block, which is what once turned its target text
    // into a list of block pointers (P2).
    symlink("keep.txt", keep.join("fast_link")).unwrap();
    symlink(SLOW_TARGET, keep.join("slow_link")).unwrap();
    c.kept_links
        .push((keep.join("fast_link"), "keep.txt".into()));
    c.kept_links
        .push((keep.join("slow_link"), SLOW_TARGET.into()));
    symlink("keep.txt", cache.join("fast_link")).unwrap();
    symlink(SLOW_TARGET, cache.join("slow_link")).unwrap();
    c.excluded.push(cache.join("fast_link"));
    c.excluded.push(cache.join("slow_link"));
    if caps.xattr {
        // user.* is refused on symlinks and specials — only trusted.* works,
        // and only for root.
        lsetxattr(&cache.join("fast_link"), "trusted.big", &vec![b'S'; 2048]);
    }

    if caps.xattr {
        // Identical attributes on a kept and an excluded file: ext4 stores ONE
        // block and shares it (mbcache), so excluding one owner must decrement
        // the refcount, not free the block (P8).
        let big = vec![b'A'; 2048];
        write_kept(&mut c, keep.join("shared_xattr.bin"), pattern(901, 8192));
        std::fs::write(cache.join("shared_xattr.bin"), [0xAB; 8192]).unwrap();
        setxattr(&keep.join("shared_xattr.bin"), "user.big", &big);
        setxattr(&cache.join("shared_xattr.bin"), "user.big", &big);
        c.excluded.push(cache.join("shared_xattr.bin"));
        // …and one attribute nobody shares, whose block IS freed with its file.
        std::fs::write(cache.join("private_xattr.bin"), [0xAB; 8192]).unwrap();
        setxattr(
            &cache.join("private_xattr.bin"),
            "user.own",
            &vec![b'B'; 2048],
        );
        c.excluded.push(cache.join("private_xattr.bin"));
    }

    if caps.streams {
        // An excluded file whose named streams are its biggest part: one in
        // clusters of its own, filled like every excluded byte, and a short
        // one inside its record. All of it goes with the file.
        let ads = cache.join("ads_big.bin");
        std::fs::write(&ads, [0xAB; 8192]).unwrap();
        setxattr(&ads, "user.big", &[0xAB; 20_000]);
        setxattr(
            &ads,
            "user.Zone.Identifier",
            b"[ZoneTransfer]\r\nZoneId=3\r\nADS-ZONE-SECRET",
        );
        c.secrets.push(b"ADS-ZONE-SECRET".to_vec());
        c.excluded.push(ads);
        // So many streams that the record overflows: ntfs-3g moves streams
        // (and the file's name) into extension records behind an attribute
        // list, where a reader that stays in the base record never looks.
        let many = cache.join("ads_many.bin");
        std::fs::write(&many, [0xAB; 4096]).unwrap();
        for i in 0..30 {
            let mut v = format!("ADS-LIST-SECRET-{i:02}-").into_bytes();
            v.resize(100, b'q');
            setxattr(&many, &format!("user.s{i:02}"), &v);
        }
        setxattr(&many, "user.bigx", &[0xAB; 40_000]);
        c.secrets.push(b"ADS-LIST-SECRET".to_vec());
        c.excluded.push(many);
        // A kept file's streams stay, byte for byte.
        let kept = keep.join("ads_kept.bin");
        write_kept(&mut c, kept.clone(), pattern(903, 8192));
        for (name, value) in [
            ("user.keepstream", pattern(904, 20_000)),
            (
                "user.Zone.Identifier",
                b"[ZoneTransfer]\r\nZoneId=3\r\n".to_vec(),
            ),
        ] {
            setxattr(&kept, name, &value);
            c.kept_streams.push((kept.clone(), name.to_string(), value));
        }
    }

    if caps.reflink {
        // Shared extents: the excluded file's data is also the kept clone's, so
        // not a byte of it may be zeroed.
        std::fs::write(cache.join("cloned.bin"), pattern(555, 128 * 1024)).unwrap();
        reflink(&cache.join("cloned.bin"), &keep.join("clone_of_cloned.bin"));
        c.kept
            .push((keep.join("clone_of_cloned.bin"), pattern(555, 128 * 1024)));
        c.excluded.push(cache.join("cloned.bin"));
    }

    for i in 0..caps.big_files as u32 {
        // Half kept, half excluded, interleaved — a checksum tree with real
        // internal nodes, and removals scattered across its leaves.
        if i % 2 == 0 {
            write_kept(
                &mut c,
                keep.join(format!("big{i:04}.bin")),
                pattern(2000 + i, 1024 * 1024),
            );
        } else {
            let p = cache.join(format!("big{i:04}.bin"));
            std::fs::write(&p, pattern(2000 + i, 1024 * 1024)).unwrap();
            c.excluded.push(p);
        }
    }

    let _ = std::process::Command::new("sync").status();
    c
}

impl Corpus {
    /// The half every filesystem shares: nothing under `keep/` may have changed.
    /// Called after mounting the patched image — and again after filling the
    /// free space, because a block freed twice only shows up once it is reused.
    pub fn verify_kept(&self, tag: &str) {
        self.verify_kept_with(tag, true);
    }

    /// The same through the kernel's ntfs3, which knows none of what ntfs-3g
    /// stores its own way: device nodes, FIFOs, sockets and symlinks come out
    /// as regular files, named streams not as `user.*` attributes — on a
    /// volume nothing was done to as well (checked 2026-10-26). Contents and
    /// the presence of every kept name are still its to judge; types, link
    /// targets and streams are ntfs-3g's.
    pub fn verify_kept_by_kernel_ntfs3(&self, tag: &str) {
        self.verify_kept_with(tag, false);
    }

    fn verify_kept_with(&self, tag: &str, ntfs3g_extras: bool) {
        for (p, want) in &self.kept {
            let got =
                std::fs::read(p).unwrap_or_else(|e| panic!("{tag}: {} нечитаем: {e}", p.display()));
            assert!(
                got == *want,
                "{tag}: {} повреждён ({} Б вместо {})",
                p.display(),
                got.len(),
                want.len()
            );
        }
        for (p, ifmt) in &self.kept_specials {
            let md = std::fs::symlink_metadata(p)
                .unwrap_or_else(|e| panic!("{tag}: {} пропал: {e}", p.display()));
            if !ntfs3g_extras {
                continue;
            }
            assert_eq!(
                md.mode() & libc::S_IFMT,
                *ifmt,
                "{tag}: {} сменил тип",
                p.display()
            );
        }
        for (p, name, want) in self.kept_streams.iter().filter(|_| ntfs3g_extras) {
            assert!(
                getxattr(p, name) == *want,
                "{tag}: поток {name} файла {} повреждён",
                p.display()
            );
        }
        for (p, target) in &self.kept_links {
            if !ntfs3g_extras {
                std::fs::symlink_metadata(p)
                    .unwrap_or_else(|e| panic!("{tag}: {} пропал: {e}", p.display()));
                continue;
            }
            let got = std::fs::read_link(p)
                .unwrap_or_else(|e| panic!("{tag}: {} пропал: {e}", p.display()));
            assert_eq!(
                got.to_string_lossy(),
                *target,
                "{tag}: цель симлинка изменилась"
            );
        }
    }

    /// Fill the free space: reallocation is where a bitmap that lied, or an
    /// extent freed under a living file, finally bites. Returns the MiB written.
    pub fn fill(mnt: &Path, limit: usize) -> usize {
        use std::io::Write;
        let chunk = vec![0x5Au8; 1024 * 1024];
        let mut written = 0;
        for i in 0..limit {
            let Ok(mut f) = std::fs::File::create(mnt.join(format!("new{i:04}.bin"))) else {
                break;
            };
            if f.write_all(&chunk).is_err() || f.sync_all().is_err() {
                break;
            }
            written += 1;
        }
        written
    }
}

/// New `dmesg` lines since `from`, filtered to what the kernel says when a
/// filesystem it mounted is wrong.
pub fn kernel_complaints(from: usize, needles: &[&str]) -> Vec<String> {
    std::process::Command::new("dmesg")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .skip(from)
                .filter(|l| needles.iter().any(|n| l.contains(n)))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

pub fn dmesg_len() -> usize {
    std::process::Command::new("dmesg")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
        .unwrap_or(0)
}

/// True when the test is running as root — every mount below needs it.
pub fn is_root() -> bool {
    // SAFETY: geteuid is always safe.
    unsafe { libc::geteuid() == 0 }
}
