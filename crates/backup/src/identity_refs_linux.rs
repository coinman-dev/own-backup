//! Making a restored system name its own devices again after their
//! identities changed (PLAN §6, stage 3) — the "fix fstab, crypttab and the
//! boot loader" half of `identity_change_linux`.
//!
//! A system names its devices in text: `UUID=` in `/etc/fstab` and
//! `/etc/crypttab`, `root=UUID=` and `search --fs-uuid` in `grub.cfg`, the
//! `search.fs_uuid` stub GRUB leaves on the ESP, `ARRAY … UUID=` in
//! `mdadm.conf`, `/dev/mapper/<vg>-<lv>` and `rd.lvm.vg=` for a volume group,
//! `options root=` in systemd-boot entries. Once the restore has given the
//! copy new identities, every one of those still names the source's — on the
//! machine the copy came from, that is the source itself.
//!
//! Each restored filesystem is mounted in turn and the known files are
//! rewritten old → new, in place (temporary file, `fsync`, `rename`). Paths
//! are walked without following symbolic links: the files belong to another
//! system, and an absolute link in it (`/etc/fstab -> /run/…`) would resolve
//! against THIS machine's root.
//!
//! Not editable this way, and reported instead: an initramfs that carries a
//! copy of crypttab or mdadm.conf (it has to be rebuilt by the restored
//! system), and a BIOS GRUB core image that embeds the `/boot` UUID.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::identity_clash::IdentityKind;

/// One identity that changed, old and new as the subsystem's tools print
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renamed {
    pub kind: IdentityKind,
    pub old: String,
    pub new: String,
}

/// One textual replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    /// A UUID or serial: matched in any case, written in the case it was
    /// found in.
    AnyCase { old: String, new: String },
    /// Exact text (LVM names are case-sensitive).
    Exact { old: String, new: String },
    /// Exact text not followed by `-`: `/dev/mapper/ubuntu-` names VG
    /// `ubuntu`, while `/dev/mapper/ubuntu--vg-root` is VG `ubuntu-vg`
    /// (device-mapper doubles a `-` inside a name, and an LV name never
    /// starts with one).
    ExactNotDash { old: String, new: String },
}

/// device-mapper doubles a `-` inside a VG or LV name.
fn dm_escape(name: &str) -> String {
    name.replace('-', "--")
}

/// md prints a UUID as four colon-joined words; blkid as a plain dashed
/// UUID of the same 16 bytes.
fn md_dashed(colons: &str) -> Option<String> {
    let hex: String = colons.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    (hex.len() == 32).then(|| {
        format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )
    })
}

fn rules_for(renamed: &[Renamed]) -> Vec<Rule> {
    let mut out = Vec::new();
    for r in renamed {
        let any = |old: &str, new: &str| Rule::AnyCase {
            old: old.to_string(),
            new: new.to_string(),
        };
        let exact = |old: String, new: String| Rule::Exact { old, new };
        match r.kind {
            IdentityKind::Ext
            | IdentityKind::Xfs
            | IdentityKind::Btrfs
            | IdentityKind::Luks
            | IdentityKind::Fat => out.push(any(&r.old, &r.new)),
            IdentityKind::Md => {
                out.push(any(&r.old, &r.new));
                if let (Some(o), Some(n)) = (md_dashed(&r.old), md_dashed(&r.new)) {
                    out.push(any(&o, &n));
                }
            }
            IdentityKind::LvmVgName => {
                let (o, n) = (&r.old, &r.new);
                let not_dash = |old: String, new: String| Rule::ExactNotDash { old, new };
                out.push(not_dash(
                    format!("/dev/mapper/{}-", dm_escape(o)),
                    format!("/dev/mapper/{}-", dm_escape(n)),
                ));
                out.push(not_dash(
                    format!("/dev/disk/by-id/dm-name-{}-", dm_escape(o)),
                    format!("/dev/disk/by-id/dm-name-{}-", dm_escape(n)),
                ));
                out.push(exact(format!("/dev/{o}/"), format!("/dev/{n}/")));
                out.push(exact(format!("rd.lvm.lv={o}/"), format!("rd.lvm.lv={n}/")));
                // `rd.lvm.vg=<name>` ends at a space, a quote or the line.
                for end in [" ", "\"", "'", "\n"] {
                    out.push(exact(
                        format!("rd.lvm.vg={o}{end}"),
                        format!("rd.lvm.vg={n}{end}"),
                    ));
                }
            }
            // GRUB names an LV it reads /boot from as lvmid/<vg uuid>/<lv uuid>.
            IdentityKind::LvmVg => out.push(exact(
                format!("lvmid/{}/", r.old),
                format!("lvmid/{}/", r.new),
            )),
            IdentityKind::LvmPv | IdentityKind::PartUuid => {}
        }
    }
    out
}

/// Replace every `old` in `text`, ignoring ASCII case, with `new` in the case
/// the match was written in.
fn replace_any_case(text: &str, old: &str, new: &str, count: &mut usize) -> String {
    if old.is_empty() {
        return text.to_string();
    }
    let lower = text.to_ascii_lowercase();
    let needle = old.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(pos) = lower[at..].find(&needle) {
        let start = at + pos;
        let end = start + needle.len();
        out.push_str(&text[at..start]);
        let found = &text[start..end];
        let upper = found.chars().any(|c| c.is_ascii_uppercase());
        out.push_str(&if upper {
            new.to_ascii_uppercase()
        } else {
            new.to_ascii_lowercase()
        });
        *count += 1;
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// Replace every `old` in `text` that is not followed by a `-`.
fn replace_not_before_dash(text: &str, old: &str, new: &str, count: &mut usize) -> String {
    if old.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(pos) = text[at..].find(old) {
        let start = at + pos;
        let end = start + old.len();
        out.push_str(&text[at..start]);
        if text[end..].starts_with('-') {
            out.push_str(old);
        } else {
            out.push_str(new);
            *count += 1;
        }
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// `text` with every renamed identity replaced, and how many replacements
/// were made.
pub fn rewrite(text: &str, renamed: &[Renamed]) -> (String, usize) {
    let mut count = 0;
    let mut text = text.to_string();
    for rule in rules_for(renamed) {
        text = match rule {
            Rule::AnyCase { old, new } => replace_any_case(&text, &old, &new, &mut count),
            Rule::ExactNotDash { old, new } => {
                replace_not_before_dash(&text, &old, &new, &mut count)
            }
            Rule::Exact { old, new } => {
                let n = text.matches(&old).count();
                count += n;
                if n > 0 {
                    text.replace(&old, &new)
                } else {
                    text
                }
            }
        };
    }
    (text, count)
}

/// Files that name devices, relative to a filesystem's root. `*` is one
/// directory level, matched without regard to case (an ESP is FAT).
const FILES: &[&str] = &[
    "etc/fstab",
    "etc/crypttab",
    "etc/mdadm/mdadm.conf",
    "etc/mdadm.conf",
    "etc/default/grub",
    "etc/kernel/cmdline",
    "etc/initramfs-tools/conf.d/resume",
    "boot/grub/grub.cfg",
    "boot/grub2/grub.cfg",
    "grub/grub.cfg",
    "grub2/grub.cfg",
    "EFI/*/grub.cfg",
    "boot/efi/EFI/*/grub.cfg",
    "loader/entries/*.conf",
    "boot/loader/entries/*.conf",
];

/// A path under `root` reached without following any symbolic link; `None`
/// when a component is missing, is a link, or is not a directory/file as
/// expected. `pattern` components: a name (matched ignoring case), `*` (every
/// directory), `*.ext` (every regular file with that extension).
fn expand(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let parts: Vec<&str> = pattern.split('/').collect();
    let mut level = vec![root.to_path_buf()];
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let mut next = Vec::new();
        for dir in &level {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let matches = if *part == "*" {
                    true
                } else if let Some(ext) = part.strip_prefix("*.") {
                    name.to_ascii_lowercase()
                        .ends_with(&format!(".{}", ext.to_ascii_lowercase()))
                } else {
                    name.eq_ignore_ascii_case(part)
                };
                if !matches {
                    continue;
                }
                // `DirEntry::file_type` does not follow links.
                let Ok(ft) = e.file_type() else {
                    continue;
                };
                if (last && ft.is_file()) || (!last && ft.is_dir()) {
                    next.push(e.path());
                }
            }
        }
        level = next;
    }
    level
}

/// Where to look in a mounted filesystem: its root, and — a btrfs mounted at
/// its top level — each top-level subvolume directory (`@`, `@rootfs`).
fn roots(mount: &Path, top_level_subvolumes: bool) -> Vec<PathBuf> {
    let mut out = vec![mount.to_path_buf()];
    if top_level_subvolumes {
        if let Ok(entries) = std::fs::read_dir(mount) {
            for e in entries.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    out.push(e.path());
                }
            }
        }
    }
    out
}

/// The extended attribute names of `path` (not following a link); none
/// where the filesystem has none (FAT).
fn xattr_names(path: &Path) -> std::io::Result<Vec<std::ffi::CString>> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: `buf` is writable for its length; `c` is NUL-terminated.
        let n = unsafe { libc::llistxattr(c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            buf.truncate(n as usize);
            break;
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ENOTSUP) => return Ok(Vec::new()),
            Some(libc::ERANGE) if buf.len() < 1 << 20 => buf.resize(buf.len() * 4, 0),
            _ => return Err(e),
        }
    }
    Ok(buf
        .split(|&b| b == 0)
        .filter(|n| !n.is_empty())
        .filter_map(|n| std::ffi::CString::new(n).ok())
        .collect())
}

/// The value of attribute `name` of `path` (not following a link).
fn xattr_value(path: &Path, name: &std::ffi::CStr) -> std::io::Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut buf = vec![0u8; 256];
    loop {
        // SAFETY: as in `xattr_names`.
        let n = unsafe {
            libc::lgetxattr(
                c.as_ptr(),
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if n >= 0 {
            buf.truncate(n as usize);
            return Ok(buf);
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ERANGE) && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        return Err(e);
    }
}

/// Carry every extended attribute of `from` (the SELinux label among them)
/// over to the open file `to`; the names that could not be, with why.
fn copy_xattrs(from: &Path, to: &std::fs::File) -> Vec<String> {
    use std::os::unix::io::AsRawFd;
    let names = match xattr_names(from) {
        Ok(n) => n,
        Err(e) => return vec![format!("*: {e}")],
    };
    let mut lost = Vec::new();
    for name in names {
        let shown = name.to_string_lossy().into_owned();
        let value = match xattr_value(from, &name) {
            Ok(v) => v,
            Err(e) => {
                lost.push(format!("{shown}: {e}"));
                continue;
            }
        };
        // SAFETY: `name` is NUL-terminated, `value` readable for its length,
        // and the descriptor is open for the call.
        let r = unsafe {
            libc::fsetxattr(
                to.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        if r != 0 {
            lost.push(format!("{shown}: {}", std::io::Error::last_os_error()));
        }
    }
    lost
}

/// Replace `path`'s content, keeping its owner, mode and extended attributes:
/// a temporary file next to it, given those, flushed, renamed over it, and
/// the directory flushed. Returns the extended attributes that could not be
/// kept (each with its error) — the caller decides what that means for the
/// restored system.
fn replace_file(path: &Path, text: &str) -> std::io::Result<Vec<String>> {
    use std::os::unix::fs::MetadataExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.own-backup-tmp"));
    let meta = std::fs::symlink_metadata(path)?;
    let lost = {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        let kept = (|| -> std::io::Result<Vec<String>> {
            f.write_all(text.as_bytes())?;
            // Owner before mode: a change of owner clears set-id bits. FAT
            // gives every file the mount's owner, so nothing differs there.
            let now = f.metadata()?;
            if (now.uid(), now.gid()) != (meta.uid(), meta.gid()) {
                std::os::unix::fs::fchown(&f, Some(meta.uid()), Some(meta.gid()))?;
            }
            // FAT has no Unix permissions; failing to set them there is expected.
            let _ = f.set_permissions(meta.permissions());
            let lost = copy_xattrs(path, &f);
            f.sync_all()?;
            Ok(lost)
        })();
        match kept {
            Ok(lost) => lost,
            Err(e) => {
                drop(f);
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
    };
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(lost)
}

/// After a file under `root` was rewritten without some of its extended
/// attributes: a system with an SELinux configuration at `root` relabels
/// itself at its next boot when `/.autorelabel` exists, which this creates.
/// Returns that file when it is there; `None` when the system is not known
/// to use SELinux (or the file could not be made) and the loss has to be
/// reported instead.
fn arrange_relabel(root: &Path) -> Option<PathBuf> {
    let selinux = std::fs::symlink_metadata(root.join("etc/selinux/config"))
        .is_ok_and(|m| m.file_type().is_file());
    if !selinux {
        return None;
    }
    let flag = root.join(".autorelabel");
    let there = match std::fs::symlink_metadata(&flag) {
        Ok(m) => m.file_type().is_file(),
        Err(_) => std::fs::File::create(&flag)
            .and_then(|f| f.sync_all())
            .is_ok(),
    };
    there.then_some(flag)
}

/// What editing one filesystem found and did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Edited {
    /// Files rewritten, relative to the filesystem root.
    pub files: Vec<String>,
    /// A BIOS GRUB (`i386-pc` modules) lives here and a `grub.cfg` next to it
    /// was rewritten: its core image may embed an old UUID.
    pub bios_grub: bool,
    /// A crypttab or mdadm.conf here was rewritten: an initramfs built from
    /// it still has the old one.
    pub initramfs_config: bool,
    /// Rewritten files whose extended attributes could not all be kept, and
    /// no relabel was arranged for (`<file> (<attribute>: <error>, …)`).
    pub xattrs_lost: Vec<String>,
    /// `/.autorelabel` files created (relative to the filesystem) because a
    /// rewritten file of an SELinux system lost its label.
    pub autorelabel: Vec<String>,
}

/// Rewrite the known files under the filesystem mounted at `mount`.
pub fn fix_tree(
    mount: &Path,
    top_level_subvolumes: bool,
    renamed: &[Renamed],
) -> std::io::Result<Edited> {
    let mut edited = Edited::default();
    for root in roots(mount, top_level_subvolumes) {
        for pattern in FILES {
            for path in expand(&root, pattern) {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let (new, n) = rewrite(&text, renamed);
                if n == 0 {
                    continue;
                }
                let lost = replace_file(&path, &new)?;
                let rel = path
                    .strip_prefix(mount)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                if !lost.is_empty() {
                    tracing::warn!(file = %rel, ?lost, "extended attributes not kept on a rewritten file");
                    match arrange_relabel(&root) {
                        Some(flag) => {
                            let flag = flag
                                .strip_prefix(mount)
                                .unwrap_or(&flag)
                                .display()
                                .to_string();
                            if !edited.autorelabel.contains(&flag) {
                                edited.autorelabel.push(flag);
                            }
                        }
                        None => edited
                            .xattrs_lost
                            .push(format!("{rel} ({})", lost.join(", "))),
                    }
                }
                if rel.ends_with("crypttab") || rel.ends_with("mdadm.conf") {
                    edited.initramfs_config = true;
                }
                if rel.ends_with("grub.cfg") {
                    let grub_dir = path.parent().unwrap_or(&root);
                    if grub_dir.join("i386-pc").is_dir() {
                        edited.bios_grub = true;
                    }
                }
                edited.files.push(rel);
            }
        }
    }
    Ok(edited)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    const NEW: &str = "99999999-8888-4777-8666-555555555555";

    fn renamed(kind: IdentityKind, old: &str, new: &str) -> Renamed {
        Renamed {
            kind,
            old: old.into(),
            new: new.into(),
        }
    }

    #[test]
    fn fstab_crypttab_and_grub_lines_follow_a_new_uuid_in_their_own_case() {
        let r = [
            renamed(IdentityKind::Ext, OLD, NEW),
            renamed(IdentityKind::Fat, "ABCD-1234", "0F0E-0D0C"),
        ];
        let fstab = format!(
            "# <file system> <mount point>\nUUID={OLD} / ext4 errors=remount-ro 0 1\nUUID=ABCD-1234 /boot/efi vfat umask=0077 0 1\n"
        );
        let (out, n) = rewrite(&fstab, &r);
        assert_eq!(n, 2);
        assert!(out.contains(&format!("UUID={NEW} / ext4")));
        assert!(out.contains("UUID=0F0E-0D0C /boot/efi"));

        let grub = format!(
            "search --no-floppy --fs-uuid --set=root {}\nlinux /vmlinuz root=UUID={OLD} ro\n",
            OLD.to_ascii_uppercase()
        );
        let (out, n) = rewrite(&grub, &r);
        assert_eq!(n, 2);
        assert!(out.contains(&NEW.to_ascii_uppercase()), "case kept: {out}");
        assert!(out.contains(&format!("root=UUID={NEW}")));
        assert!(!out.to_ascii_lowercase().contains(OLD));
    }

    #[test]
    fn a_renamed_volume_group_moves_every_way_a_system_names_it() {
        let r = [
            renamed(IdentityKind::LvmVgName, "ubuntu-vg", "ubuntu-vg_restored"),
            renamed(
                IdentityKind::LvmVg,
                "aaaaaa-1111-2222-3333-4444-5555-bbbbbb",
                "cccccc-1111-2222-3333-4444-5555-dddddd",
            ),
        ];
        let text = "/dev/mapper/ubuntu--vg-ubuntu--lv / ext4 defaults 0 1\n\
                    /dev/ubuntu-vg/swap none swap sw 0 0\n\
                    linux /vmlinuz root=/dev/mapper/ubuntu--vg-ubuntu--lv rd.lvm.vg=ubuntu-vg rd.lvm.lv=ubuntu-vg/ubuntu-lv\n\
                    set root='lvmid/aaaaaa-1111-2222-3333-4444-5555-bbbbbb/xyz'\n\
                    /dev/mapper/other--vg-lv /data ext4 defaults 0 2\n";
        let (out, n) = rewrite(text, &r);
        assert_eq!(n, 6, "{out}");
        assert!(out.contains("/dev/mapper/ubuntu--vg_restored-ubuntu--lv / ext4"));
        assert!(out.contains("/dev/ubuntu-vg_restored/swap"));
        assert!(out.contains("rd.lvm.vg=ubuntu-vg_restored "));
        assert!(out.contains("rd.lvm.lv=ubuntu-vg_restored/ubuntu-lv"));
        assert!(out.contains("lvmid/cccccc-1111-2222-3333-4444-5555-dddddd/xyz"));
        assert!(
            out.contains("/dev/mapper/other--vg-lv"),
            "other VGs untouched"
        );
    }

    #[test]
    fn a_vg_name_that_prefixes_another_vg_name_moves_only_its_own_lines() {
        let r = [renamed(
            IdentityKind::LvmVgName,
            "ubuntu",
            "ubuntu_restored",
        )];
        let text = "/dev/mapper/ubuntu-root / ext4 defaults 0 1\n\
                    /dev/mapper/ubuntu--vg-root /mnt ext4 defaults 0 2\n\
                    /dev/disk/by-id/dm-name-ubuntu--vg-swap none swap sw 0 0\n\
                    /dev/disk/by-id/dm-name-ubuntu-swap none swap sw 0 0\n\
                    /dev/ubuntu-vg/data /srv ext4 defaults 0 2\n";
        let (out, n) = rewrite(text, &r);
        assert_eq!(n, 2, "{out}");
        assert!(out.contains("/dev/mapper/ubuntu_restored-root / ext4"));
        assert!(out.contains("/dev/mapper/ubuntu--vg-root /mnt"));
        assert!(out.contains("dm-name-ubuntu--vg-swap"));
        assert!(out.contains("dm-name-ubuntu_restored-swap"));
        assert!(out.contains("/dev/ubuntu-vg/data"));
    }

    /// The rewritten file keeps its owner, its mode and its extended
    /// attributes (a `user.` one here: setting `security.selinux` needs root
    /// or SELinux).
    #[test]
    fn a_rewritten_file_keeps_its_mode_owner_and_extended_attributes() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "etc/crypttab", &format!("c UUID={OLD} none luks\n"));
        let path = root.join("etc/crypttab");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let value = b"system_u:object_r:etc_t:s0";
        // SAFETY: NUL-terminated names, `value` readable for its length.
        let set = unsafe {
            libc::setxattr(
                c.as_ptr(),
                c"user.own_backup_test".as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        let xattrs = set == 0;
        if !xattrs {
            eprintln!(
                "no user xattrs here ({}) — only mode and owner checked",
                std::io::Error::last_os_error()
            );
        }
        let before = std::fs::metadata(&path).unwrap();
        let e = fix_tree(root, false, &[renamed(IdentityKind::Luks, OLD, NEW)]).unwrap();
        assert_eq!(e.files, vec!["etc/crypttab"]);
        assert!(
            e.xattrs_lost.is_empty() && e.autorelabel.is_empty(),
            "{e:?}"
        );
        let after = std::fs::metadata(&path).unwrap();
        assert_ne!(before.ino(), after.ino(), "replaced, not written in place");
        assert_eq!(after.mode() & 0o7777, 0o600);
        assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
        if xattrs {
            assert_eq!(
                xattr_value(&path, c"user.own_backup_test").unwrap(),
                value.to_vec()
            );
        }
    }

    #[test]
    fn a_lost_label_on_an_selinux_system_asks_it_to_relabel_itself() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(arrange_relabel(root), None, "no SELinux, nothing arranged");
        assert!(!root.join(".autorelabel").exists());
        write(root, "etc/selinux/config", "SELINUX=enforcing\n");
        assert_eq!(arrange_relabel(root), Some(root.join(".autorelabel")));
        assert!(root.join(".autorelabel").is_file());
        // Twice is fine.
        assert_eq!(arrange_relabel(root), Some(root.join(".autorelabel")));
    }

    #[test]
    fn an_md_array_is_found_in_both_spellings() {
        let r = [renamed(
            IdentityKind::Md,
            "1a2b3c4d:5e6f4a7b:8c9d0e1f:2a3b4c5d",
            "11111111:22222222:33333333:44444444",
        )];
        let text = format!(
            "ARRAY /dev/md0 metadata=1.2 UUID=1a2b3c4d:5e6f4a7b:8c9d0e1f:2a3b4c5d\nUUID={OLD} /srv ext4\n"
        );
        let (out, n) = rewrite(&text, &r);
        assert_eq!(n, 2);
        assert!(out.contains("UUID=11111111:22222222:33333333:44444444"));
        assert!(out.contains("UUID=11111111-2222-2222-3333-333344444444"));
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn a_mounted_system_is_edited_in_place_and_reports_what_needs_rebuilding() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "etc/fstab",
            &format!("UUID={OLD} / ext4 defaults 0 1\n"),
        );
        write(
            root,
            "etc/crypttab",
            &format!("root_crypt UUID={OLD} none luks\n"),
        );
        write(root, "etc/hostname", OLD);
        write(root, "boot/grub/grub.cfg", &format!("root=UUID={OLD}\n"));
        std::fs::create_dir_all(root.join("boot/grub/i386-pc")).unwrap();
        let r = [renamed(IdentityKind::Ext, OLD, NEW)];
        let e = fix_tree(root, false, &r).unwrap();
        assert_eq!(
            e.files,
            vec!["etc/fstab", "etc/crypttab", "boot/grub/grub.cfg"]
        );
        assert!(e.bios_grub && e.initramfs_config);
        assert!(std::fs::read_to_string(root.join("etc/fstab"))
            .unwrap()
            .contains(NEW));
        assert_eq!(
            std::fs::read_to_string(root.join("etc/hostname")).unwrap(),
            OLD,
            "only the known files"
        );
        assert!(std::fs::read_dir(root.join("etc"))
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().contains("own-backup-tmp")));
    }

    #[test]
    fn an_esp_stub_is_found_whatever_its_case_and_a_btrfs_subvolume_is_searched() {
        let dir = tempfile::tempdir().unwrap();
        let esp = dir.path().join("esp");
        write(
            &esp,
            "EFI/ubuntu/grub.cfg",
            &format!("search.fs_uuid {OLD} root hd0,gpt2\n"),
        );
        write(
            &esp,
            "efi/debian/GRUB.CFG",
            &format!("search.fs_uuid {OLD} root\n"),
        );
        let r = [renamed(IdentityKind::Btrfs, OLD, NEW)];
        let e = fix_tree(&esp, false, &r).unwrap();
        assert_eq!(e.files.len(), 2, "{e:?}");

        let top = dir.path().join("btrfs");
        write(
            &top,
            "@/etc/fstab",
            &format!("UUID={OLD} / btrfs subvol=@ 0 0\n"),
        );
        let e = fix_tree(&top, true, &r).unwrap();
        assert_eq!(e.files, vec!["@/etc/fstab"]);
    }

    #[test]
    fn a_symbolic_link_is_never_followed_out_of_the_restored_system() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("host-fstab");
        std::fs::write(&outside, format!("UUID={OLD} / ext4\n")).unwrap();
        let root = dir.path().join("restored");
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("etc/fstab")).unwrap();
        // A linked directory too.
        std::fs::create_dir_all(dir.path().join("hostboot/grub")).unwrap();
        std::fs::write(
            dir.path().join("hostboot/grub/grub.cfg"),
            format!("root=UUID={OLD}\n"),
        )
        .unwrap();
        std::os::unix::fs::symlink(dir.path().join("hostboot"), root.join("boot")).unwrap();
        let e = fix_tree(&root, false, &[renamed(IdentityKind::Ext, OLD, NEW)]).unwrap();
        assert!(e.files.is_empty(), "{e:?}");
        assert!(std::fs::read_to_string(&outside).unwrap().contains(OLD));
        assert!(
            std::fs::read_to_string(dir.path().join("hostboot/grub/grub.cfg"))
                .unwrap()
                .contains(OLD)
        );
    }
}
