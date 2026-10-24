//! Creating and growing filesystems in a restore plan on Linux (PLAN §6,
//! stage 3): [`RestoreOp::Format`] and [`RestoreOp::ExtendFilesystem`].
//!
//! Both run after the disk was let go, through the partition's own node, with
//! the filesystem's own tools ([`own_backup_backup::linux_tools`]):
//!
//! | filesystem | create        | grow to the partition                              |
//! |------------|---------------|----------------------------------------------------|
//! | ext2/3/4   | `mkfs.ext4`   | `e2fsck -f -p`, then `resize2fs` — offline         |
//! | XFS        | `mkfs.xfs`    | `xfs_growfs` — XFS grows only mounted              |
//! | btrfs      | `mkfs.btrfs`  | `btrfs filesystem resize <devid>:max` — mounted    |
//! | NTFS       | `mkntfs`      | `ntfsresize` — offline                             |
//! | FAT32      | `mkfs.vfat`   | —                                                  |
//! | exFAT      | `mkfs.exfat`  | —                                                  |
//!
//! ReFS has no Linux formatter; a plan asking for it is refused before the run
//! (`linux_refusals`). A "full" format means something only to Windows
//! (`FormatEx` zero-fills); the Linux tools discard the partition instead, so
//! the choice is logged and otherwise the same.
//!
//! A filesystem mounted to grow it may duplicate one this machine has mounted
//! (the copy came from here, and the person chose to keep its identities):
//! XFS takes `nouuid` for that, and a btrfs clone is only mounted where the
//! kernel gives it a temporary fsid (6.7+) — older kernels would treat it as
//! the mounted original, and the grow would act on that.

use tracing::info;

use own_backup_backup::linux_tools::{args, run_checked, run_ok, TempMount, Tools};
use own_backup_common::restore_plan::FormatFs;
use own_backup_common::types::FileSystem;
use own_backup_common::{tr, OwnBackupError, Result};

/// The `mkfs` program and its arguments for `fs` on `node`.
pub(super) fn mkfs_command(
    fs: FormatFs,
    label: &str,
    node: &str,
) -> Result<(&'static str, Vec<String>)> {
    let label = label.trim();
    let mut a: Vec<String> = Vec::new();
    let labelled = |flag: &str, a: &mut Vec<String>| {
        if !label.is_empty() {
            a.push(flag.to_string());
            a.push(label.to_string());
        }
    };
    let prog = match fs {
        FormatFs::Ext4 => {
            a.extend(args(["-F", "-q"]));
            labelled("-L", &mut a);
            "mkfs.ext4"
        }
        FormatFs::Xfs => {
            a.push("-f".into());
            labelled("-L", &mut a);
            "mkfs.xfs"
        }
        FormatFs::Btrfs => {
            a.push("-f".into());
            labelled("-L", &mut a);
            "mkfs.btrfs"
        }
        FormatFs::Fat32 => {
            a.extend(args(["-F", "32"]));
            labelled("-n", &mut a);
            "mkfs.vfat"
        }
        FormatFs::ExFat => {
            labelled("-L", &mut a);
            "mkfs.exfat"
        }
        FormatFs::Ntfs => {
            // -Q: no zero-fill, as every other format here; -F: the node may
            // not report itself as a partition (loop, NBD).
            a.extend(args(["-Q", "-F"]));
            labelled("-L", &mut a);
            "mkntfs"
        }
        FormatFs::ReFs => {
            return Err(OwnBackupError::RestoreIncompatible(
                tr!("restore_fs_linux.no_refs").into_owned(),
            ))
        }
    };
    a.push(node.to_string());
    Ok((prog, a))
}

/// Create `fs` on `node`.
pub(super) fn format(
    tools: &mut dyn Tools,
    node: &str,
    fs: FormatFs,
    label: &str,
    quick: bool,
) -> Result<String> {
    let (prog, a) = mkfs_command(fs, label, node)?;
    info!(
        node,
        fs = fs.as_str(),
        label,
        quick,
        prog,
        "creating a filesystem"
    );
    run_ok(tools, prog, &a)?;
    Ok(tr!("restore_fs_linux.formatted", fs = fs.as_str(), node = node).into_owned())
}

/// What grows the filesystem found on a partition, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Grow {
    /// `e2fsck -f -p`, then `resize2fs`.
    Ext,
    /// Mount, `xfs_growfs`, unmount.
    Xfs,
    /// Mount, `btrfs filesystem resize <devid>:max`, unmount.
    Btrfs { devid: u64 },
    /// `ntfsresize`, answering its question.
    Ntfs,
}

/// btrfs keeps this device's number in the superblock's `dev_item`.
fn btrfs_devid(head: &[u8]) -> Option<u64> {
    let at = 0x10000 + 0xC9;
    head.get(at..at + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        .filter(|&d| d > 0)
}

/// How to grow what `head` (the partition's first bytes) holds; an error
/// names a filesystem that cannot be grown here.
pub(super) fn grow_for(head: &[u8], node: &str) -> Result<Grow> {
    let fs = own_backup_inventory::fs_probe::identify(head)
        .map(|p| p.fs)
        .unwrap_or(FileSystem::Unknown);
    match fs {
        FileSystem::Ext4 => Ok(Grow::Ext),
        FileSystem::Xfs => Ok(Grow::Xfs),
        FileSystem::Btrfs => Ok(Grow::Btrfs {
            devid: btrfs_devid(head).unwrap_or(1),
        }),
        FileSystem::Ntfs => Ok(Grow::Ntfs),
        other => Err(OwnBackupError::RestoreIncompatible(
            tr!(
                "restore_fs_linux.cannot_grow",
                node = node,
                fs = other.label()
            )
            .into_owned(),
        )),
    }
}

/// The partition's first bytes, enough for [`grow_for`].
fn read_head(node: &str) -> Result<Vec<u8>> {
    let dev = own_backup_backup::linux_device::LinuxRawDevice::open_read(node)?;
    let len = (own_backup_inventory::fs_probe::PROBE_LEN as u64).min(dev.length()) as usize;
    let mut head = vec![0u8; len];
    dev.read_at(0, &mut head)?;
    Ok(head)
}

/// Grow the filesystem on `node` to fill it. `btrfs_mount_safe` says whether
/// mounting this btrfs here cannot land on a mounted original (see the module
/// docs); the caller decides it from the machine's identities.
pub(super) fn extend(
    tools: &mut dyn Tools,
    node: &str,
    btrfs_mount_safe: impl FnOnce() -> bool,
) -> Result<String> {
    let grow = grow_for(&read_head(node)?, node)?;
    info!(node, ?grow, "growing the filesystem to its partition");
    match grow {
        Grow::Ext => {
            // resize2fs insists on a freshly checked filesystem; 1 and 2 mean
            // "fixed something", 4 and up that it could not.
            run_checked(tools, "e2fsck", &args(["-f", "-p", node]), None, |c| c < 4)?;
            run_ok(tools, "resize2fs", &args([node]))?;
        }
        Grow::Ntfs => {
            // ntfsresize asks before it writes; there is no flag for "yes".
            run_checked(
                tools,
                "ntfsresize",
                &args(["--no-progress-bar", node]),
                Some(b"y\n"),
                |c| c == 0,
            )?;
        }
        Grow::Xfs => {
            let m = TempMount::mount(node, &[("xfs", "rw,nouuid")])?;
            let dir = m.path().display().to_string();
            let grown = run_ok(tools, "xfs_growfs", &args([&dir]));
            m.unmount()?;
            grown?;
        }
        Grow::Btrfs { devid } => {
            if !btrfs_mount_safe() {
                return Err(OwnBackupError::RestoreIncompatible(
                    tr!("restore_fs_linux.btrfs_clone_mounted", node = node).into_owned(),
                ));
            }
            let m = TempMount::mount(node, &[("btrfs", "rw")])?;
            let dir = m.path().display().to_string();
            let grown = run_ok(
                tools,
                "btrfs",
                &args(["filesystem", "resize", &format!("{devid}:max"), &dir]),
            );
            m.unmount()?;
            grown?;
        }
    }
    Ok(tr!("restore_fs_linux.grown", node = node).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_backup::linux_tools::{SystemTools, ToolOutput};

    #[derive(Default)]
    struct Rec {
        calls: Vec<(String, Vec<String>, Option<Vec<u8>>)>,
        code: i32,
    }

    impl Tools for Rec {
        fn run(
            &mut self,
            prog: &str,
            a: &[String],
            stdin: Option<&[u8]>,
        ) -> std::io::Result<ToolOutput> {
            self.calls
                .push((prog.into(), a.to_vec(), stdin.map(<[u8]>::to_vec)));
            Ok(ToolOutput {
                code: Some(self.code),
                ..Default::default()
            })
        }
    }

    #[test]
    fn each_filesystem_gets_its_own_mkfs_and_label_flag() {
        let cmd = |fs, label| mkfs_command(fs, label, "/dev/sdz3").unwrap();
        assert_eq!(
            cmd(FormatFs::Ext4, "root"),
            ("mkfs.ext4", args(["-F", "-q", "-L", "root", "/dev/sdz3"]))
        );
        assert_eq!(
            cmd(FormatFs::Xfs, ""),
            ("mkfs.xfs", args(["-f", "/dev/sdz3"]))
        );
        assert_eq!(
            cmd(FormatFs::Btrfs, "data"),
            ("mkfs.btrfs", args(["-f", "-L", "data", "/dev/sdz3"]))
        );
        assert_eq!(
            cmd(FormatFs::Fat32, "ESP"),
            ("mkfs.vfat", args(["-F", "32", "-n", "ESP", "/dev/sdz3"]))
        );
        assert_eq!(
            cmd(FormatFs::ExFat, ""),
            ("mkfs.exfat", args(["/dev/sdz3"]))
        );
        assert_eq!(
            cmd(FormatFs::Ntfs, "Data"),
            ("mkntfs", args(["-Q", "-F", "-L", "Data", "/dev/sdz3"]))
        );
        assert!(mkfs_command(FormatFs::ReFs, "", "/dev/sdz3").is_err());
    }

    #[test]
    fn a_failing_mkfs_is_a_failed_step() {
        let mut t = Rec {
            code: 1,
            ..Default::default()
        };
        assert!(format(&mut t, "/dev/sdz3", FormatFs::Ext4, "", true).is_err());
        assert_eq!(t.calls[0].0, "mkfs.ext4");
    }

    fn head_with(fs: FileSystem) -> Vec<u8> {
        let mut h = vec![0u8; own_backup_inventory::fs_probe::PROBE_LEN];
        match fs {
            FileSystem::Ext4 => {
                h[1024 + 56..1024 + 58].copy_from_slice(&0xEF53u16.to_le_bytes());
            }
            FileSystem::Xfs => h[0..4].copy_from_slice(b"XFSB"),
            FileSystem::Btrfs => {
                h[0x10040..0x10048].copy_from_slice(b"_BHRfS_M");
                h[0x10000 + 0xC9..0x10000 + 0xD1].copy_from_slice(&3u64.to_le_bytes());
            }
            FileSystem::Ntfs => h[3..11].copy_from_slice(b"NTFS    "),
            _ => {}
        }
        h
    }

    #[test]
    fn the_grow_tool_follows_the_filesystem_on_the_partition() {
        assert_eq!(
            grow_for(&head_with(FileSystem::Ext4), "n").unwrap(),
            Grow::Ext
        );
        assert_eq!(
            grow_for(&head_with(FileSystem::Xfs), "n").unwrap(),
            Grow::Xfs
        );
        assert_eq!(
            grow_for(&head_with(FileSystem::Btrfs), "n").unwrap(),
            Grow::Btrfs { devid: 3 }
        );
        assert_eq!(
            grow_for(&head_with(FileSystem::Ntfs), "n").unwrap(),
            Grow::Ntfs
        );
        assert!(grow_for(&head_with(FileSystem::Unknown), "n").is_err());
    }

    /// ext4 grows offline: a check first, then resize2fs on the node.
    #[test]
    fn ext4_is_checked_then_resized_offline() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("p.img");
        std::fs::write(&img, head_with(FileSystem::Ext4)).unwrap();
        let node = img.to_str().unwrap();
        let mut t = Rec::default();
        extend(&mut t, node, || unreachable!()).unwrap();
        let progs: Vec<&str> = t.calls.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(progs, vec!["e2fsck", "resize2fs"]);
        assert_eq!(t.calls[0].1, args(["-f", "-p", node]));

        // e2fsck's "errors corrected" is not a failure; "left uncorrected" is.
        let mut t = Rec {
            code: 4,
            ..Default::default()
        };
        assert!(extend(&mut t, node, || unreachable!()).is_err());
        assert_eq!(t.calls.len(), 1, "nothing is resized after a failed check");
    }

    #[test]
    fn ntfs_is_resized_with_its_question_answered() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("p.img");
        std::fs::write(&img, head_with(FileSystem::Ntfs)).unwrap();
        let mut t = Rec::default();
        extend(&mut t, img.to_str().unwrap(), || unreachable!()).unwrap();
        assert_eq!(t.calls[0].0, "ntfsresize");
        assert_eq!(t.calls[0].2.as_deref(), Some(&b"y\n"[..]));
    }

    #[test]
    fn a_btrfs_clone_of_a_mounted_filesystem_is_not_mounted_on_an_old_kernel() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("p.img");
        std::fs::write(&img, head_with(FileSystem::Btrfs)).unwrap();
        let mut t = Rec::default();
        let err = extend(&mut t, img.to_str().unwrap(), || false).unwrap_err();
        assert!(
            matches!(err, OwnBackupError::RestoreIncompatible(_)),
            "{err}"
        );
        assert!(t.calls.is_empty());
    }

    /// Is `prog` installed? Tests of the real tools skip without them.
    fn have(prog: &str) -> bool {
        std::process::Command::new("sh")
            .args([
                "-c",
                &format!("command -v {prog} || test -x /usr/sbin/{prog} || test -x /sbin/{prog}"),
            ])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn sized_file(dir: &std::path::Path, mib: u64) -> std::path::PathBuf {
        let p = dir.join("part.img");
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(mib << 20).unwrap();
        p
    }

    fn grow_file(p: &std::path::Path, mib: u64) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(p)
            .unwrap()
            .set_len(mib << 20)
            .unwrap();
    }

    /// The real mkfs.ext4 and resize2fs on a partition-sized file: the
    /// filesystem made at 32 MiB fills 64 MiB after the grow, and stays clean.
    #[test]
    fn real_ext4_is_created_and_grown_when_e2fsprogs_is_installed() {
        if !(have("mkfs.ext4") && have("resize2fs")) {
            eprintln!("e2fsprogs not installed — skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = sized_file(dir.path(), 32);
        let node = img.to_str().unwrap();
        format(&mut SystemTools, node, FormatFs::Ext4, "restored", true).unwrap();
        let blocks = |node: &str| -> u64 {
            let out = SystemTools
                .run("dumpe2fs", &args(["-h", node]), None)
                .unwrap()
                .stdout;
            let count = out
                .lines()
                .find_map(|l| l.strip_prefix("Block count:"))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap();
            let size = out
                .lines()
                .find_map(|l| l.strip_prefix("Block size:"))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap();
            count * size
        };
        assert_eq!(blocks(node), 32 << 20);
        grow_file(&img, 64);
        extend(&mut SystemTools, node, || unreachable!()).unwrap();
        assert_eq!(blocks(node), 64 << 20);
        let fsck = SystemTools
            .run("e2fsck", &args(["-f", "-n", node]), None)
            .unwrap();
        assert_eq!(fsck.code, Some(0), "{}", fsck.stdout);
    }

    /// mkntfs and ntfsresize on a file: the grow answers ntfsresize's
    /// question and the volume then spans the larger file.
    #[test]
    fn real_ntfs_is_created_and_grown_when_ntfs_3g_is_installed() {
        if !(have("mkntfs") && have("ntfsresize")) {
            eprintln!("ntfs-3g not installed — skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = sized_file(dir.path(), 32);
        let node = img.to_str().unwrap();
        format(&mut SystemTools, node, FormatFs::Ntfs, "Data", true).unwrap();
        grow_file(&img, 64);
        extend(&mut SystemTools, node, || unreachable!()).unwrap();
        let info = SystemTools
            .run("ntfsresize", &args(["--info", "--force", node]), None)
            .unwrap();
        let line = info
            .stdout
            .lines()
            .find(|l| l.starts_with("Current volume size"))
            .unwrap_or_default()
            .to_string();
        let bytes: u64 = line
            .split_whitespace()
            .find_map(|w| w.parse().ok())
            .unwrap_or(0);
        assert!(bytes > 60 << 20, "{}", info.stdout);
    }

    /// XFS grows only mounted: the step mounts the file (through a loop
    /// device), grows it and lets it go. Root and an XFS kernel.
    #[test]
    #[ignore = "root: mounts a loop device"]
    fn real_xfs_is_grown_through_a_temporary_mount() {
        assert!(have("mkfs.xfs") && have("xfs_growfs"), "xfsprogs needed");
        let dir = tempfile::tempdir().unwrap();
        let img = sized_file(dir.path(), 320);
        let node = img.to_str().unwrap();
        format(&mut SystemTools, node, FormatFs::Xfs, "", true).unwrap();
        grow_file(&img, 640);
        extend(&mut SystemTools, node, || unreachable!()).unwrap();
        let out = SystemTools
            .run(
                "xfs_db",
                &args(["-r", "-c", "sb 0", "-c", "print dblocks blocksize", node]),
                None,
            )
            .unwrap()
            .stdout;
        let field = |name: &str| -> u64 {
            out.lines()
                .find_map(|l| l.strip_prefix(&format!("{name} = ")))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        assert_eq!(field("dblocks") * field("blocksize"), 640 << 20, "{out}");
        assert!(
            !std::path::Path::new("/run/own-backup/restore-mnt/part.img").exists(),
            "the temporary mount point is gone"
        );
    }
}
