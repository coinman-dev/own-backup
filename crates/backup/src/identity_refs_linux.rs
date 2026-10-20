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
                out.push(exact(
                    format!("/dev/mapper/{}-", dm_escape(o)),
                    format!("/dev/mapper/{}-", dm_escape(n)),
                ));
                out.push(exact(
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

/// `text` with every renamed identity replaced, and how many replacements
/// were made.
pub fn rewrite(text: &str, renamed: &[Renamed]) -> (String, usize) {
    let mut count = 0;
    let mut text = text.to_string();
    for rule in rules_for(renamed) {
        text = match rule {
            Rule::AnyCase { old, new } => replace_any_case(&text, &old, &new, &mut count),
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

/// Replace `path`'s content, keeping its permissions: a temporary file next
/// to it, flushed, renamed over it, and the directory flushed.
fn replace_file(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.own-backup-tmp"));
    let perms = std::fs::metadata(path)?.permissions();
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    // FAT has no Unix permissions; failing to set them there is expected.
    let _ = std::fs::set_permissions(&tmp, perms);
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
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
                replace_file(&path, &new)?;
                let rel = path
                    .strip_prefix(mount)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
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
