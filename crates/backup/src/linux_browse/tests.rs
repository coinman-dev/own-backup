use super::*;
use own_backup_common::lvm_pv::fixtures;

struct MemVolume(Vec<u8>);

impl RawVolume for MemVolume {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let start = offset as usize;
        let src = self
            .0
            .get(start..start + buf.len())
            .ok_or_else(|| OwnBackupError::Device(format!("read past the end at {offset}")))?;
        buf.copy_from_slice(src);
        Ok(())
    }
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
}

const MIB: usize = 1 << 20;

#[test]
fn the_head_of_a_volume_says_what_it_holds() {
    let mut head = vec![1u8; PROBE_LEN];
    assert_eq!(identify(&head), Content::Other);
    head[1080] = 0x53;
    head[1081] = 0xEF;
    assert_eq!(identify(&head), Content::Ext);
    let mut swap = vec![0u8; PROBE_LEN];
    swap[4086..4096].copy_from_slice(b"SWAPSPACE2");
    assert_eq!(identify(&swap), Content::Swap);
    let mut luks = head.clone();
    luks[..6].copy_from_slice(b"LUKS\xba\xbe");
    assert_eq!(identify(&luks), Content::Luks, "before anything inside it");
    let pv = fixtures::label(64 << 20, 1 << 20, &[(4096, 1 << 16)]);
    assert_eq!(identify(&pv), Content::Lvm);
    assert_eq!(identify(&[]), Content::Other, "an empty volume");
    let mut big = vec![1u8; PROBE_LEN];
    big[0x10040..0x10048].copy_from_slice(b"_BHRfS_M");
    assert_eq!(identify(&big), Content::Btrfs);
    let mut zfs = vec![1u8; PROBE_LEN];
    zfs[128 * 1024..128 * 1024 + 8].copy_from_slice(&0x00ba_b10c_u64.to_le_bytes());
    assert_eq!(identify(&zfs), Content::Zfs);
    assert_eq!(identify(b"XFSB rest"), Content::Xfs);
    assert_eq!(identify(&vec![0u8; PROBE_LEN]), Content::Empty);
}

/// A logical volume made of two segments, the second lying before the first
/// on the PV, reads in its own order.
#[test]
fn a_logical_volume_reads_its_segments_in_its_own_order() {
    let mut pv = vec![0u8; 8 * 4096];
    pv[4 * 4096..6 * 4096].fill(1);
    pv[4096..2 * 4096].fill(2);
    let base: Shared = Rc::new(RefCell::new(Box::new(MemVolume(pv))));
    let segments = vec![
        PvSegment {
            pv_offset: 4 * 4096,
            lv_offset: 0,
            length: 2 * 4096,
        },
        PvSegment {
            pv_offset: 4096,
            lv_offset: 2 * 4096,
            length: 4096,
        },
    ];
    let mut lv = LvView::new(base, segments, 3 * 4096);
    assert_eq!(lv.len(), 3 * 4096);
    let mut buf = vec![0u8; 3 * 4096];
    lv.read_at(0, &mut buf).unwrap();
    assert!(buf[..2 * 4096].iter().all(|&b| b == 1));
    assert!(buf[2 * 4096..].iter().all(|&b| b == 2));
    // Across the seam.
    let mut seam = vec![0u8; 10];
    lv.read_at(2 * 4096 - 5, &mut seam).unwrap();
    assert_eq!(seam, [1, 1, 1, 1, 1, 2, 2, 2, 2, 2]);
    assert!(
        lv.read_at(3 * 4096 - 1, &mut [0u8; 2]).is_err(),
        "past its end"
    );
}

#[test]
fn what_is_not_a_linux_filesystem_says_so() {
    let vol = MemVolume(vec![7u8; MIB]);
    match open(Box::new(vol), None) {
        Err(OpenError::Failed(why)) => assert!(why.contains("ext2/3/4"), "{why}"),
        _ => panic!("opened"),
    }
}

// --- with real filesystems (mke2fs) ---------------------------------------------

#[cfg(target_os = "linux")]
mod real {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::process::Command;

    /// An ext4 image of `size` bytes holding `tree`, or `None` without mke2fs.
    pub(super) fn ext4_image(size: usize, populate: impl FnOnce(&Path)) -> Option<Vec<u8>> {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        populate(&src);
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(size as u64)
            .unwrap();
        let ok = Command::new("mke2fs")
            .args(["-q", "-F", "-t", "ext4", "-L", "obk-test", "-d"])
            .arg(&src)
            .arg(&img)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("no mke2fs here; skipped");
            return None;
        }
        Some(std::fs::read(&img).unwrap())
    }

    pub(super) fn sample_tree(src: &Path) {
        std::fs::create_dir_all(src.join("home/user/Документы")).unwrap();
        std::fs::write(src.join("home/user/Документы/отчёт.txt"), "привет\n").unwrap();
        std::fs::write(src.join("home/user/a:b?.txt"), "colon").unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(src.join("home/user/big.bin"), &big).unwrap();
        std::fs::write(
            src.join(std::ffi::OsStr::from_bytes(b"home/user/\xd0\xd2\xc9.txt")),
            "koi8",
        )
        .unwrap();
        std::os::unix::fs::symlink("user/big.bin", src.join("home/link.bin")).unwrap();
        std::os::unix::fs::symlink("user", src.join("home/userlink")).unwrap();
        std::os::unix::fs::symlink("/nowhere", src.join("home/broken")).unwrap();
        std::fs::create_dir(src.join("etc")).unwrap();
        std::fs::write(src.join("etc/hostname"), "own-backup\n").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", src.join("etc/abs")).unwrap();
        let sparse = std::fs::File::create(src.join("home/user/sparse.bin")).unwrap();
        sparse.set_len(2 * MIB as u64).unwrap();
        use std::os::unix::fs::FileExt;
        sparse.write_at(b"end", 2 * MIB as u64 - 3).unwrap();
    }

    /// What every test reads back from the sample tree, whatever it is in;
    /// `pre` is the folder holding it (`\@` on btrfs, nothing elsewhere).
    pub(super) fn check_sample(fs: &mut Fs, pre: &str) {
        let at = |s: &str| names::path_from_display(&format!("{pre}{s}"));
        let mut names = |s: &str| -> Vec<String> {
            fs.list(&at(s))
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect()
        };
        let root = names("\\");
        assert!(
            root.contains(&"home".to_string()) && root.contains(&"etc".to_string()),
            "{root:?}"
        );
        let home = names("\\home");
        // Links shown as what they point to; the one to nowhere left out.
        assert!(home.contains(&"userlink".to_string()), "{home:?}");
        assert!(home.contains(&"link.bin".to_string()), "{home:?}");
        assert!(!home.contains(&"broken".to_string()), "{home:?}");

        let user = fs.list(&at("\\home\\user")).unwrap();
        assert!(user[0].dir, "directories first: {user:?}");
        let colon = user
            .iter()
            .find(|e| e.name.starts_with("a\u{F03A}b"))
            .expect("the name Windows cannot hold, moved");
        assert_eq!(colon.size, 5);
        let koi = user
            .iter()
            .find(|e| e.name.ends_with(".txt") && e.name.starts_with('\u{F7D0}'));
        let koi = koi.expect("the KOI8 name").name.clone();
        assert_eq!(
            fs.read(&at(&format!("\\home\\user\\{koi}")), 0, 100).unwrap(),
            b"koi8"
        );

        let report = at("\\home\\user\\Документы\\отчёт.txt");
        assert_eq!(fs.read(&report, 0, 100).unwrap(), "привет\n".as_bytes());
        assert_eq!(fs.stat(&report).unwrap().size, "привет\n".len() as u64);

        // A big file in pieces, out of order, past its end.
        let big = at("\\home\\userlink\\big.bin");
        let want: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(
            fs.read(&big, 100_000, 50_000).unwrap(),
            &want[100_000..150_000]
        );
        assert_eq!(fs.read(&big, 0, 10).unwrap(), &want[..10]);
        assert_eq!(fs.read(&big, 299_990, 100).unwrap(), &want[299_990..]);
        assert!(fs.read(&big, 400_000, 100).unwrap().is_empty());
        assert_eq!(fs.read(&at("\\home\\link.bin"), 0, 300_000).unwrap(), want);

        // A link to an absolute path is read from the root the tree is in.
        assert_eq!(
            fs.read(&at("\\etc\\abs"), 0, 100).unwrap(),
            b"own-backup\n"
        );
        // A file with a hole reads zeros in it.
        let sparse = at("\\home\\user\\sparse.bin");
        assert_eq!(fs.stat(&sparse).unwrap().size, 2 * MIB as u64);
        assert!(fs.read(&sparse, MIB as u64, 4096).unwrap().iter().all(|&b| b == 0));
        assert_eq!(fs.read(&sparse, 2 * MIB as u64 - 3, 10).unwrap(), b"end");

        assert!(fs.list(&at("\\nothing")).is_err());
        assert!(fs.read(&at("\\home"), 0, 1).is_err(), "a directory");
    }

    #[test]
    fn a_plain_ext4_partition_shows_its_files() {
        let Some(img) = ext4_image(16 * MIB, sample_tree) else {
            return;
        };
        let mut p = open(Box::new(MemVolume(img)), None).unwrap();
        assert!(!p.encrypted);
        let info = p.info();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].name, "", "the partition itself");
        assert_eq!(info[0].label, "obk-test");
        let fs = p.roots[0].fs.as_mut().unwrap();
        check_sample(fs, "");
    }

    /// The PV of the fixtures: extent 4 MiB, the first extent at 1 MiB. The
    /// LV `root` is the ext image in two pieces, the second before the first
    /// on the PV; `swap_1` is swap and is not shown; `mirror` is listed with
    /// why it cannot be read.
    pub(super) fn pv_image(ext: &[u8]) -> Vec<u8> {
        const EXT: usize = 4 * MIB;
        assert_eq!(ext.len(), 4 * EXT);
        let text = fixtures::vg_text(
            12,
            &[
                (
                    "root",
                    vec![
                        ("striped", vec![("pv0", 6)], 2),
                        ("striped", vec![("pv0", 1)], 2),
                    ],
                ),
                ("swap_1", vec![("striped", vec![("pv0", 10)], 1)]),
                ("mirror", vec![("raid1", vec![("pv0", 11)], 1)]),
            ],
        );
        let size = MIB + 12 * EXT;
        let mut pv = vec![0u8; size];
        let head = fixtures::label(size as u64, MIB as u64, &[(4096, (MIB - 4096) as u64)]);
        pv[..head.len()].copy_from_slice(&head);
        let mda = fixtures::mda((MIB - 4096) as u64, 8192, &text);
        pv[4096..4096 + mda.len()].copy_from_slice(&mda);
        let at = |extent: usize| MIB + extent * EXT;
        pv[at(6)..at(8)].copy_from_slice(&ext[..2 * EXT]);
        pv[at(1)..at(3)].copy_from_slice(&ext[2 * EXT..]);
        pv[at(10) + 4086..at(10) + 4096].copy_from_slice(b"SWAPSPACE2");
        pv
    }

    #[test]
    fn the_logical_volumes_of_a_pv_show_their_files() {
        let Some(img) = ext4_image(16 * MIB, sample_tree) else {
            return;
        };
        let mut p = open(Box::new(MemVolume(pv_image(&img))), None).unwrap();
        let info = p.info();
        let names: Vec<&str> = info.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["vg0-root", "vg0-mirror"], "swap is not shown");
        assert_eq!(info[0].size, 16 * MIB as u64);
        assert_eq!(info[0].error, None);
        assert!(
            info[1].error.as_deref().unwrap_or("").contains("raid1"),
            "{info:?}"
        );
        check_sample(p.roots[0].fs.as_mut().unwrap(), "");
    }

    /// btrfs as Ubuntu lays it out — the system in the subvolume `@`, homes in
    /// `@home` — made by mkfs.btrfs from a folder, compressed with
    /// `compress` (`no`, `zlib`, `lzo`, `zstd`), or `None` without it.
    pub(super) fn btrfs_image(compress: &str) -> Option<Vec<u8>> {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("@")).unwrap();
        std::fs::create_dir_all(src.join("@home/user")).unwrap();
        sample_tree(&src.join("@"));
        std::fs::write(src.join("@home/user/note.txt"), "home subvolume\n").unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(512 * MIB as u64)
            .unwrap();
        let ok = Command::new("mkfs.btrfs")
            .args(["-q", "-f", "-L", "obk-btrfs", "--rootdir"])
            .arg(&src)
            .args(["--subvol", "rw:@", "--subvol", "rw:@home", "--compress", compress, "--shrink"])
            .arg(&img)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("no mkfs.btrfs that can do this here; skipped");
            return None;
        }
        Some(std::fs::read(&img).unwrap())
    }

    pub(super) fn check_btrfs(p: &mut Partition) {
        let info = p.info();
        assert_eq!(info.len(), 1, "one filesystem: {info:?}");
        assert_eq!(info[0].label, "obk-btrfs");
        let fs = p.roots[0].fs.as_mut().unwrap();
        let top: Vec<String> = fs.list(b"/").unwrap().into_iter().map(|e| e.name).collect();
        assert!(top.contains(&"@".to_string()) && top.contains(&"@home".to_string()), "{top:?}");
        check_sample(fs, "\\@");
        let note = names::path_from_display("\\@home\\user\\note.txt");
        assert_eq!(fs.read(&note, 0, 100).unwrap(), b"home subvolume\n");
    }

    /// Every compression btrfs has, inline and regular extents, holes,
    /// links across the tree, the subvolumes entered like folders.
    #[test]
    fn btrfs_shows_ubuntus_subvolumes_and_reads_every_compression() {
        for compress in ["no", "zlib", "lzo", "zstd"] {
            let Some(img) = btrfs_image(compress) else {
                return;
            };
            let mut p = open(Box::new(MemVolume(img)), None)
                .unwrap_or_else(|e| panic!("{compress}: {e}"));
            check_btrfs(&mut p);
        }
    }

    /// `plain` inside a LUKS2 container as cryptsetup makes it (root-free, on
    /// a file), its data encrypted with the key cryptsetup chose.
    pub(super) fn luks_wrap(plain: &[u8], pw: &str) -> Vec<u8> {
        use std::io::{Seek, SeekFrom, Write};
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("luks.img");
        let data_offset = 16 * MIB;
        std::fs::File::create(&file)
            .unwrap()
            .set_len((data_offset + plain.len()) as u64)
            .unwrap();
        let mut child = Command::new("cryptsetup")
            .args([
                "luksFormat",
                "-q",
                "--type",
                "luks2",
                "--pbkdf",
                "argon2id",
                "--pbkdf-memory",
                "65536",
                "--iter-time",
                "100",
                "--key-file",
                "-",
            ])
            .arg(&file)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("cryptsetup");
        child.stdin.take().unwrap().write_all(pw.as_bytes()).unwrap();
        assert!(child.wait().unwrap().success(), "luksFormat");
        let disk = std::fs::read(&file).unwrap();
        let params = crate::luks::open_luks(&mut MemVolume(disk), pw.as_bytes()).unwrap();
        assert_eq!(params.data_offset, data_offset as u64);
        let mut data = plain.to_vec();
        params.encrypt(0, &mut data).unwrap();
        let mut f = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        f.seek(SeekFrom::Start(data_offset as u64)).unwrap();
        f.write_all(&data).unwrap();
        drop(f);
        std::fs::read(&file).unwrap()
    }

    /// btrfs straight inside LUKS, no LVM: what the user's Ubuntu turned out
    /// to be. Needs `cryptsetup`: `--ignored`.
    #[test]
    #[ignore]
    fn btrfs_inside_luks_opens_with_its_passphrase() {
        let Some(img) = btrfs_image("zstd") else {
            return;
        };
        let pw = "correct horse battery";
        let disk = luks_wrap(&img, pw);
        assert_eq!(
            open(Box::new(MemVolume(disk.clone())), None).err(),
            Some(OpenError::NeedsPassword)
        );
        let mut p = open(Box::new(MemVolume(disk)), Some(pw.as_bytes())).unwrap();
        assert!(p.encrypted);
        check_btrfs(&mut p);
    }

    /// The whole of an encrypted Ubuntu: LUKS2 as cryptsetup writes it
    /// (root-free, on a file), LVM inside, ext4 inside that. The data area is
    /// encrypted with the key cryptsetup made. Needs `cryptsetup`:
    /// `cargo test -p own-backup-backup linux_browse -- --ignored`.
    #[test]
    #[ignore]
    fn an_encrypted_ubuntu_partition_opens_with_its_passphrase() {
        let Some(img) = ext4_image(16 * MIB, sample_tree) else {
            return;
        };
        let pw = "correct horse battery";
        let disk = luks_wrap(&pv_image(&img), pw);

        assert_eq!(
            open(Box::new(MemVolume(disk.clone())), None).err(),
            Some(OpenError::NeedsPassword)
        );
        assert_eq!(
            open(Box::new(MemVolume(disk.clone())), Some(b"wrong")).err(),
            Some(OpenError::WrongPassword)
        );
        let mut p = open(Box::new(MemVolume(disk)), Some(pw.as_bytes())).unwrap();
        assert!(p.encrypted);
        assert_eq!(p.roots[0].name, "vg0-root");
        check_sample(p.roots[0].fs.as_mut().unwrap(), "");
    }
}
