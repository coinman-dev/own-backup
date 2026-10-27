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
    let mut head = vec![0u8; PROBE_LEN];
    assert_eq!(identify(&head), Content::Other);
    head[1080] = 0x53;
    head[1081] = 0xEF;
    assert_eq!(identify(&head), Content::Ext);
    let mut swap = vec![0u8; PROBE_LEN];
    swap[4086..].copy_from_slice(b"SWAPSPACE2");
    assert_eq!(identify(&swap), Content::Swap);
    let mut luks = head.clone();
    luks[..6].copy_from_slice(b"LUKS\xba\xbe");
    assert_eq!(identify(&luks), Content::Luks, "before anything inside it");
    let pv = fixtures::label(64 << 20, 1 << 20, &[(4096, 1 << 16)]);
    assert_eq!(identify(&pv), Content::Lvm);
    assert_eq!(identify(&[]), Content::Other, "an empty volume");
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
    assert!(lv.read_at(3 * 4096 - 1, &mut [0u8; 2]).is_err(), "past its end");
}

#[test]
fn what_is_not_a_linux_filesystem_says_so() {
    let vol = MemVolume(vec![0u8; MIB]);
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
    }

    /// What every test reads back from the sample tree, whatever it is in.
    pub(super) fn check_sample(fs: &mut Fs) {
        let names = |p: &str| -> Vec<String> {
            fs.list(&names::path_from_display(p))
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect()
        };
        let root = names("\\");
        assert!(root.contains(&"home".to_string()) && root.contains(&"etc".to_string()), "{root:?}");
        let home = names("\\home");
        // Links shown as what they point to; the one to nowhere left out.
        assert!(home.contains(&"userlink".to_string()), "{home:?}");
        assert!(home.contains(&"link.bin".to_string()), "{home:?}");
        assert!(!home.contains(&"broken".to_string()), "{home:?}");

        let user = fs.list(&names::path_from_display("\\home\\user")).unwrap();
        assert!(user[0].dir, "directories first: {user:?}");
        let colon = user
            .iter()
            .find(|e| e.name.starts_with("a\u{F03A}b"))
            .expect("the name Windows cannot hold, moved");
        assert_eq!(colon.size, 5);
        let koi = user.iter().find(|e| e.name.ends_with(".txt") && e.name.starts_with('\u{F7D0}'));
        let koi = koi.expect("the KOI8 name").name.clone();
        let p = format!("\\home\\user\\{koi}");
        assert_eq!(fs.read(&names::path_from_display(&p), 0, 100).unwrap(), b"koi8");

        let report = names::path_from_display("\\home\\user\\Документы\\отчёт.txt");
        assert_eq!(fs.read(&report, 0, 100).unwrap(), "привет\n".as_bytes());
        assert_eq!(fs.stat(&report).unwrap().size, "привет\n".len() as u64);

        // A big file in pieces, out of order, past its end.
        let big = names::path_from_display("\\home\\userlink\\big.bin");
        let want: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(fs.read(&big, 100_000, 50_000).unwrap(), &want[100_000..150_000]);
        assert_eq!(fs.read(&big, 0, 10).unwrap(), &want[..10]);
        assert_eq!(fs.read(&big, 299_990, 100).unwrap(), &want[299_990..]);
        assert!(fs.read(&big, 400_000, 100).unwrap().is_empty());
        let linked = names::path_from_display("\\home\\link.bin");
        assert_eq!(fs.read(&linked, 0, 300_000).unwrap(), want);

        assert!(fs.list(&names::path_from_display("\\nothing")).is_err());
        assert!(fs.read(&names::path_from_display("\\home"), 0, 1).is_err(), "a directory");
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
        check_sample(fs);
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
                    vec![("striped", vec![("pv0", 6)], 2), ("striped", vec![("pv0", 1)], 2)],
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
        assert!(info[1].error.as_deref().unwrap_or("").contains("raid1"), "{info:?}");
        check_sample(p.roots[0].fs.as_mut().unwrap());
    }

    /// The whole of an encrypted Ubuntu: LUKS2 as cryptsetup writes it
    /// (root-free, on a file), LVM inside, ext4 inside that. The data area is
    /// encrypted with the key cryptsetup made. Needs `cryptsetup`:
    /// `cargo test -p own-backup-backup linux_browse -- --ignored`.
    #[test]
    #[ignore]
    fn an_encrypted_ubuntu_partition_opens_with_its_passphrase() {
        use std::io::{Seek, SeekFrom, Write};
        let Some(img) = ext4_image(16 * MIB, sample_tree) else {
            return;
        };
        let mut plain = pv_image(&img);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("luks.img");
        let data_offset = 16 * MIB;
        std::fs::File::create(&file)
            .unwrap()
            .set_len((data_offset + plain.len()) as u64)
            .unwrap();
        let pw = "correct horse battery";
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
        let params = crate::luks::open_luks(&mut MemVolume(disk.clone()), pw.as_bytes()).unwrap();
        assert_eq!(params.data_offset, data_offset as u64);
        params.encrypt(0, &mut plain).unwrap();
        let mut f = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        f.seek(SeekFrom::Start(data_offset as u64)).unwrap();
        f.write_all(&plain).unwrap();
        drop(f);
        let disk = std::fs::read(&file).unwrap();

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
        check_sample(p.roots[0].fs.as_mut().unwrap());
    }
}
