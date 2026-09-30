//! Build a bootable ISO out of a finished medium folder (development aid).
use own_backup_backup::fat_writer::{build_fat16, FatFile};
use own_backup_backup::iso9660::{write_iso, IsoBoot, IsoFile};
use std::path::{Path, PathBuf};

fn walk(root: &Path, base: &Path, out: &mut Vec<IsoFile>) {
    for e in std::fs::read_dir(root).unwrap().flatten() {
        let p = e.path();
        let rel = p
            .strip_prefix(base)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if p.is_dir() {
            walk(&p, base, out);
        } else {
            out.push(IsoFile {
                path: rel,
                source: p,
            });
        }
    }
}

fn main() {
    let media = PathBuf::from(std::env::args().nth(1).expect("media dir"));
    let out = PathBuf::from(std::env::args().nth(2).expect("output .iso"));
    let mut files = Vec::new();
    walk(&media, &media, &mut files);

    // The El Torito image carries the loader and the BCD it looks for beside
    // itself; the rest of the medium is read from the ISO9660 tree.
    let mut boot_files = Vec::new();
    for rel in [
        "EFI/BOOT/BOOTX64.EFI",
        "EFI/Microsoft/Boot/BCD",
        "boot/boot.sdi",
    ] {
        let src = media.join(rel);
        if src.exists() {
            boot_files.push(FatFile {
                path: rel.replace('/', "\\"),
                data: std::fs::read(&src).unwrap(),
            });
        }
    }
    let efisys = out.with_extension("efisys.bin");
    std::fs::write(&efisys, build_fat16(&boot_files, "OWNBOOT").unwrap()).unwrap();
    files.push(IsoFile {
        path: "boot/efisys.bin".into(),
        source: efisys.clone(),
    });
    let len = write_iso(
        &files,
        "OWNBACKUP",
        Some(IsoBoot {
            image_path: "boot/efisys.bin".into(),
        }),
        &out,
    )
    .unwrap();
    let _ = std::fs::remove_file(&efisys);
    println!(
        "{}: {} MiB, {} files",
        out.display(),
        len / 1048576,
        files.len()
    );
}
