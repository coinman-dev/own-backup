//! Stand check for VHDX chains made in userspace (CHECKS 12).
//!
//! `make <dir>` (Linux): builds differencing chains with our writer, folds and
//! repoints one of them with `vhdx_chain`, damages the primary region table of
//! another, and checks every leaf with our reader against an in-memory model.
//!
//! `check <dir>` (Windows, elevated): the same sets as Windows sees them —
//! attached by the Windows driver and read as a raw disk; written by Windows;
//! merged and repointed by the Windows API; plus a chain written by Windows and
//! folded by our code. Every step prints `VL PASS <name>` or `VL FAIL <name>`.
//!
//! `cmp <file>` (Windows): Windows read vs our read of one image.
//! `fold <base> <child>` / `repoint <child> <parent>`: our chain operations.
//!
//! The model: every write is sector-aligned, so Windows can make it on a raw
//! disk too, and the expected guest content is known without moving files back.

use std::path::{Path, PathBuf};

use own_backup_backup::vhdx_bat::VhdxGuestReader;
use own_backup_backup::vhdx_format::VhdxWriter;
use sha2::{Digest, Sha256};

const MIB: u64 = 1024 * 1024;
const SIZE: u64 = 64 * MIB;

/// One write: offset, length, pattern tag (0 = zeros).
type Op = (u64, u64, u64);

const W_BASE: &[Op] = &[
    (0, 4 * MIB, 1),
    (10 * MIB, 512, 2),
    (20 * MIB, 2 * MIB, 3),
    (40 * MIB + 4096, 4096, 4),
];
const W_C1: &[Op] = &[
    // Part of a block the parent holds: sector-specific (state 7) in a child.
    (512, 512, 11),
    (MIB, MIB, 12),
    // Zeros over parent data must land.
    (10 * MIB, 512, 0),
    // Part of a block nobody holds.
    (30 * MIB + 4096, 8192, 13),
    // Across a block boundary.
    (21 * MIB - 512, 1024, 14),
    (63 * MIB, MIB, 15),
];
const W_C2: &[Op] = &[(1024, 512, 21), (MIB + 512, 512, 0), (50 * MIB, 512, 22)];
/// What Windows writes into a child made here.
#[allow(dead_code)]
const W_WIN: &[Op] = &[
    (2048, 512, 31),
    (3 * MIB + 512, 512, 32),
    (45 * MIB, 4096, 33),
    (63 * MIB + 512, 512, 0),
];

fn word(tag: u64, index: u64) -> u64 {
    let mut z = (tag << 40) ^ index ^ 0x9E37_79B9_7F4A_7C15;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn pattern(tag: u64, off: u64, len: u64) -> Vec<u8> {
    let mut out = vec![0u8; len as usize];
    if tag != 0 {
        for (i, chunk) in out.chunks_mut(8).enumerate() {
            chunk.copy_from_slice(&word(tag, off / 8 + i as u64).to_le_bytes());
        }
    }
    out
}

fn apply(model: &mut [u8], ops: &[Op]) {
    for &(off, len, tag) in ops {
        model[off as usize..(off + len) as usize].copy_from_slice(&pattern(tag, off, len));
    }
}

fn model(layers: &[&[Op]]) -> Vec<u8> {
    let mut m = vec![0u8; SIZE as usize];
    for l in layers {
        apply(&mut m, l);
    }
    m
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn first_diff(a: &[u8], b: &[u8]) -> String {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(p) => format!("first difference at byte {p} (MiB {})", p as u64 / MIB),
        None if a.len() != b.len() => format!("lengths {} vs {}", a.len(), b.len()),
        None => "identical".into(),
    }
}

fn ours(path: &Path) -> Result<Vec<u8>, String> {
    let mut r = VhdxGuestReader::open(path).map_err(|e| e.to_string())?;
    if r.virtual_size() != SIZE {
        return Err(format!("virtual size {}", r.virtual_size()));
    }
    let mut buf = vec![0u8; SIZE as usize];
    for (i, chunk) in buf.chunks_mut(MIB as usize).enumerate() {
        r.read_at(i as u64 * MIB, chunk)
            .map_err(|e| e.to_string())?;
    }
    Ok(buf)
}

fn report(name: &str, got: Result<Vec<u8>, String>, want: &[u8]) -> bool {
    match got {
        Ok(g) if g == want => {
            println!("VL PASS {name} {}", &hex(&g)[..16]);
            true
        }
        Ok(g) => {
            println!("VL FAIL {name}: {}", first_diff(&g, want));
            false
        }
        Err(e) => {
            println!("VL FAIL {name}: {e}");
            false
        }
    }
}

fn write_ours(w: &mut VhdxWriter, ops: &[Op]) {
    for &(off, len, tag) in ops {
        w.write_at(off, &pattern(tag, off, len)).expect("write");
    }
}

/// `<root>/base.vhdx`, `<root>/sub/c1.vhdx` (parent `..\base.vhdx`),
/// `<root>/sub/c2.vhdx` — written by our writer.
fn build_ours(root: &Path) {
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let base = root.join("base.vhdx");
    let mut w = VhdxWriter::create(&base, SIZE, 2 * MIB, 512).expect("create base");
    write_ours(&mut w, W_BASE);
    w.finish().expect("finish base");
    let c1 = sub.join("c1.vhdx");
    let mut w = VhdxWriter::create_differencing(&c1, &base).expect("create c1");
    write_ours(&mut w, W_C1);
    w.finish().expect("finish c1");
    let c2 = sub.join("c2.vhdx");
    let mut w = VhdxWriter::create_differencing(&c2, &c1).expect("create c2");
    write_ours(&mut w, W_C2);
    w.finish().expect("finish c2");
}

fn make(dir: &Path) -> bool {
    let e_base = model(&[W_BASE]);
    let e_c1 = model(&[W_BASE, W_C1]);
    let e_c2 = model(&[W_BASE, W_C1, W_C2]);
    let mut ok = true;
    for set in ["ro", "w", "m", "wm", "bad"] {
        let root = dir.join(set);
        let _ = std::fs::remove_dir_all(&root);
        build_ours(&root);
    }
    let ro = dir.join("ro");
    ok &= report("make-base", ours(&ro.join("base.vhdx")), &e_base);
    ok &= report("make-c1", ours(&ro.join("sub/c1.vhdx")), &e_c1);
    ok &= report("make-c2", ours(&ro.join("sub/c2.vhdx")), &e_c2);

    // m: c1 folded into base, c2 repointed at base — consolidation done here.
    let m = dir.join("m");
    let stats = own_backup_backup::vhdx_chain::fold(&m.join("base.vhdx"), &m.join("sub/c1.vhdx"))
        .expect("fold");
    println!("fold: {} blocks, {} bytes", stats.blocks, stats.bytes);
    own_backup_backup::vhdx_chain::repoint(&m.join("sub/c2.vhdx"), &m.join("base.vhdx"))
        .expect("repoint");
    std::fs::remove_file(m.join("sub/c1.vhdx")).unwrap();
    ok &= report("make-m-base", ours(&m.join("base.vhdx")), &e_c1);
    ok &= report("make-m-c2", ours(&m.join("sub/c2.vhdx")), &e_c2);

    // bad: the primary region table of c1 fails its checksum.
    let c1 = dir.join("bad/sub/c1.vhdx");
    let mut bytes = std::fs::read(&c1).unwrap();
    bytes[192 * 1024 + 40] ^= 0xFF;
    std::fs::write(&c1, bytes).unwrap();
    ok &= report("make-bad-c1", ours(&c1), &e_c1);
    ok
}

#[cfg(target_os = "windows")]
mod win {
    use super::*;
    use own_backup_backup::vhdx::{self, AttachMode};
    use own_backup_backup::win_device::WinRawDevice;

    /// A just-attached disk takes a moment to appear.
    fn open_retry(phys: &str, write: bool) -> Result<WinRawDevice, String> {
        let mut last = String::new();
        for _ in 0..50 {
            let r = if write {
                WinRawDevice::open_write(phys)
            } else {
                WinRawDevice::open_read(phys)
            };
            match r {
                Ok(d) => return Ok(d),
                Err(e) => last = e.to_string(),
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        Err(last)
    }

    /// Attach read-only through the Windows driver and read the whole disk.
    pub fn windows_read(path: &Path) -> Result<Vec<u8>, String> {
        let a = vhdx::attach_vhdx(path, AttachMode::ReadOnlyMounted).map_err(|e| e.to_string())?;
        let phys = a.physical_path().to_string();
        drop(a);
        let result = (|| {
            let mut d = open_retry(&phys, false)?;
            if d.length() != SIZE {
                return Err(format!("disk length {}", d.length()));
            }
            let mut buf = vec![0u8; SIZE as usize];
            for (i, chunk) in buf.chunks_mut(MIB as usize).enumerate() {
                d.read_at(i as u64 * MIB, chunk)
                    .map_err(|e| e.to_string())?;
            }
            Ok(buf)
        })();
        if let Err(e) = vhdx::detach_vhdx(path) {
            println!("detach {}: {e}", path.display());
        }
        result
    }

    /// Write through the Windows driver (attached read-write, no letter).
    pub fn windows_write(path: &Path, ops: &[Op]) -> Result<(), String> {
        let a = vhdx::attach_vhdx(path, AttachMode::RawWrite).map_err(|e| e.to_string())?;
        let mut d = open_retry(a.physical_path(), true)?;
        let _ = d.update_properties();
        d.set_online_this_session().map_err(|e| e.to_string())?;
        for &(off, len, tag) in ops {
            d.write_at(off, &pattern(tag, off, len))
                .map_err(|e| e.to_string())?;
        }
        d.flush().map_err(|e| e.to_string())?;
        drop(d);
        drop(a);
        Ok(())
    }

    fn step(name: &str, r: Result<(), String>) -> bool {
        match r {
            Ok(()) => {
                println!("VL PASS {name}");
                true
            }
            Err(e) => {
                println!("VL FAIL {name}: {e}");
                false
            }
        }
    }

    pub fn check(dir: &Path) -> bool {
        let e_base = model(&[W_BASE]);
        let e_c1 = model(&[W_BASE, W_C1]);
        let e_c2 = model(&[W_BASE, W_C1, W_C2]);
        let e_c2w = model(&[W_BASE, W_C1, W_C2, W_WIN]);
        let mut ok = true;

        // (2) Linux-made chain read by Windows; the parent sits in `..\`.
        let ro = dir.join("ro");
        ok &= report("ro-base-win", windows_read(&ro.join("base.vhdx")), &e_base);
        ok &= report("ro-c1-win", windows_read(&ro.join("sub/c1.vhdx")), &e_c1);
        ok &= report("ro-c2-win", windows_read(&ro.join("sub/c2.vhdx")), &e_c2);
        ok &= report("ro-c2-ours", ours(&ro.join("sub/c2.vhdx")), &e_c2);

        // (3) Windows writes into a Linux-made child; both readers agree.
        let w = dir.join("w");
        ok &= step("w-write", windows_write(&w.join("sub/c2.vhdx"), W_WIN));
        ok &= report("w-c2-win", windows_read(&w.join("sub/c2.vhdx")), &e_c2w);
        ok &= report("w-c2-ours", ours(&w.join("sub/c2.vhdx")), &e_c2w);
        ok &= report("w-c1-untouched", ours(&w.join("sub/c1.vhdx")), &e_c1);

        // (4) Folded and repointed here: Windows opens, writes past the new
        // metadata region, then merges with its own API.
        let m = dir.join("m");
        ok &= report("m-c2-win", windows_read(&m.join("sub/c2.vhdx")), &e_c2);
        ok &= report("m-base-win", windows_read(&m.join("base.vhdx")), &e_c1);
        ok &= step("m-write", windows_write(&m.join("sub/c2.vhdx"), W_WIN));
        ok &= report("m-c2w-win", windows_read(&m.join("sub/c2.vhdx")), &e_c2w);
        ok &= report("m-c2w-ours", ours(&m.join("sub/c2.vhdx")), &e_c2w);
        ok &= step(
            "m-merge",
            vhdx::merge_into_parent(&m.join("sub/c2.vhdx")).map_err(|e| e.to_string()),
        );
        ok &= report("m-merged-win", windows_read(&m.join("base.vhdx")), &e_c2w);
        ok &= report("m-merged-ours", ours(&m.join("base.vhdx")), &e_c2w);

        // (5) Windows consolidation over a Linux-made chain.
        let wm = dir.join("wm");
        ok &= step(
            "wm-merge-c1",
            vhdx::merge_into_parent(&wm.join("sub/c1.vhdx")).map_err(|e| e.to_string()),
        );
        ok &= step(
            "wm-setparent",
            vhdx::set_parent_path(&wm.join("sub/c2.vhdx"), &wm.join("base.vhdx"))
                .map_err(|e| e.to_string()),
        );
        ok &= report("wm-c2-win", windows_read(&wm.join("sub/c2.vhdx")), &e_c2);
        ok &= report("wm-c2-ours", ours(&wm.join("sub/c2.vhdx")), &e_c2);
        ok &= report("wm-base-ours", ours(&wm.join("base.vhdx")), &e_c1);

        // (6) Damaged primary region table.
        ok &= report(
            "bad-c1-win",
            windows_read(&dir.join("bad/sub/c1.vhdx")),
            &e_c1,
        );
        ok &= report("bad-c1-ours", ours(&dir.join("bad/sub/c1.vhdx")), &e_c1);

        // (7) A chain Windows wrote, folded and repointed by our code.
        let x = dir.join("x");
        let _ = std::fs::remove_dir_all(&x);
        std::fs::create_dir_all(x.join("sub")).unwrap();
        let (xb, x1, x2) = (
            x.join("base.vhdx"),
            x.join("sub/c1.vhdx"),
            x.join("sub/c2.vhdx"),
        );
        let made = (|| {
            vhdx::create_dynamic_vhdx(
                &xb,
                &vhdx::VhdxCreateOptions {
                    max_size: SIZE,
                    logical_sector_size: 512,
                    physical_sector_size: 4096,
                },
            )
            .map_err(|e| e.to_string())?;
            windows_write(&xb, W_BASE)?;
            vhdx::create_differencing_vhdx(&x1, &xb).map_err(|e| e.to_string())?;
            windows_write(&x1, W_C1)?;
            vhdx::create_differencing_vhdx(&x2, &x1).map_err(|e| e.to_string())?;
            windows_write(&x2, W_C2)
        })();
        ok &= step("x-make", made);
        ok &= report("x-c1-ours", ours(&x1), &e_c1);
        ok &= report("x-c2-ours", ours(&x2), &e_c2);
        ok &= report("x-c2-win", windows_read(&x2), &e_c2);
        ok &= step(
            "x-fold",
            own_backup_backup::vhdx_chain::fold(&xb, &x1)
                .map(|s| println!("x fold: {} blocks", s.blocks))
                .map_err(|e| e.to_string()),
        );
        ok &= step(
            "x-repoint",
            own_backup_backup::vhdx_chain::repoint(&x2, &xb).map_err(|e| e.to_string()),
        );
        ok &= report("x-base-win", windows_read(&xb), &e_c1);
        ok &= report("x-c2-after-win", windows_read(&x2), &e_c2);
        ok &= report("x-c2-after-ours", ours(&x2), &e_c2);
        ok
    }

    /// Windows read vs our read of any image (e.g. one with trimmed ranges).
    pub fn cmp(path: &Path) -> bool {
        let w = windows_read(path);
        let o = ours(path);
        match (w, o) {
            (Ok(w), Ok(o)) if w == o => {
                println!("VL PASS cmp {} {}", path.display(), &hex(&w)[..16]);
                true
            }
            (Ok(w), Ok(o)) => {
                println!("VL FAIL cmp {}: {}", path.display(), first_diff(&w, &o));
                false
            }
            (w, o) => {
                println!(
                    "VL FAIL cmp {}: windows {:?} ours {:?}",
                    path.display(),
                    w.err(),
                    o.err()
                );
                false
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize| PathBuf::from(args.get(i).expect("missing argument"));
    let ok = match args.get(1).map(String::as_str) {
        Some("make") => make(&arg(2)),
        #[cfg(target_os = "windows")]
        Some("check") => win::check(&arg(2)),
        #[cfg(target_os = "windows")]
        Some("cmp") => win::cmp(&arg(2)),
        Some("fold") => own_backup_backup::vhdx_chain::fold(&arg(2), &arg(3))
            .map(|s| println!("fold: {} blocks", s.blocks))
            .map_err(|e| println!("VL FAIL fold: {e}"))
            .is_ok(),
        Some("repoint") => own_backup_backup::vhdx_chain::repoint(&arg(2), &arg(3))
            .map_err(|e| println!("VL FAIL repoint: {e}"))
            .is_ok(),
        _ => {
            eprintln!("usage: vhdx_lab make|check|cmp|fold|repoint ...");
            false
        }
    };
    std::process::exit(if ok { 0 } else { 1 });
}
