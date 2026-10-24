#!/usr/bin/env python3
"""Validate local compatibility copies (GTK3, binrw) against their original crate files."""
import hashlib
import json
import argparse
import subprocess
import tempfile
from pathlib import Path

repo = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--audit", action="store_true", help="also audit the original registry versions for new advisories")
parser.add_argument("--cargo-audit", default="cargo-audit")
args = parser.parse_args()
manifests = []
def sha(data):
    return hashlib.sha256(data).hexdigest()

for name, version in [("glib", "0.18.5"), ("glib-macros", "0.18.5"), ("gtk3-macros", "0.18.2"), ("binrw", "0.11.3")]:
    root = repo / "vendor" / name
    manifest = json.loads((root / "OWN-BACKUP-PATCH.json").read_text())
    manifests.append(manifest)
    assert manifest["crate"] == name and manifest["version"] == version
    originals = manifest["upstream_files"]
    changed = manifest["changed_files"]
    actual = {p.relative_to(root).as_posix() for p in root.rglob("*") if p.is_file()}
    assert actual == set(originals) | {"OWN-BACKUP-PATCH.json"}, f"{name}: unexpected files"
    if name == "binrw":
        assert set(changed) == {"src/helpers.rs", "src/lib.rs"}
    if name == "glib":
        assert set(changed) == {"src/lib.rs", "src/variant_iter.rs"}
        assert manifest["fix_commit"] == "05dff0ee696f9bcd8617cd48c4b812d046d440cb"
    for file, original_hash in originals.items():
        data = (root / file).read_bytes()
        if file not in changed:
            assert sha(data) == original_hash, f"{name}/{file}: unrecorded change"
            continue
        change = changed[file]
        assert change["before"] == original_hash and sha(data) == change["after"], f"{name}/{file}: hash mismatch"
        text = data.decode()
        if name == "glib" and file == "src/variant_iter.rs":
            assert text.count("let mut p: *mut libc::c_char = std::ptr::null_mut();") == 1
            assert text.count("                &mut p,") == 1
            text = text.replace("let mut p: *mut libc::c_char", "let p: *mut libc::c_char").replace("                &mut p,", "                &p,")
        elif name == "binrw" and file == "src/helpers.rs":
            new = ("fn default_reader<'a, R: Read + Seek, T: BinRead>(\n    reader: &mut R,\n"
                   "    endian: Endian,\n    args: T::Args<'a>,\n) -> BinResult<T>")
            assert text.count(new) == 1
            text = text.replace(new, "#[binrw::parser(reader, endian)]\n"
                                "fn default_reader<'a, T: BinRead>(args: T::Args<'a>, ...) -> BinResult<T>")
        elif name == "binrw" and file == "src/lib.rs":
            assert text.startswith("#![allow(warnings)]\n")
            assert text.count("pub extern crate self as binrw;") == 1
            text = text.replace("#![allow(warnings)]\n", "", 1).replace(
                "pub extern crate self as binrw;", "extern crate self as binrw;")
        elif name == "glib" and file == "src/lib.rs":
            assert text.count("#![allow(warnings)]\n") == 1
            text = text.replace("#![allow(warnings)]\n", "", 1)
        elif file.endswith(".rs"):
            assert "proc_macro_error3::" in text
            text = text.replace("proc_macro_error3::", "proc_macro_error::")
            if name == "gtk3-macros" and file == "src/lib.rs":
                assert text.count("#![allow(warnings)]\n\n") == 1
                text = text.replace("#![allow(warnings)]\n\n", "", 1)
        elif file == "Cargo.toml":
            new = '[dependencies.proc-macro-error3]\nversion = "3.1.1"\ndefault-features = false\nfeatures = ["syn2-error"]'
            assert text.count(new) == 1
            text = text.replace(new, '[dependencies.proc-macro-error]\nversion = "1.0"')
        elif file == "Cargo.toml.orig":
            new = 'proc-macro-error3 = { version = "3.1.1", default-features = false, features = ["syn2-error"] }'
            assert text.count(new) == 1
            text = text.replace(new, 'proc-macro-error = "1.0"')
        else:
            raise AssertionError(f"{name}/{file}: unsupported patch")
        assert sha(text.encode()) == original_hash, f"{name}/{file}: change exceeds the documented patch"
    print(f"{name} {version}: {len(originals)} upstream files, {len(changed)} checked changes")

if args.audit:
    # cargo-audit excludes path packages. Audit their real upstream identities
    # separately so a future advisory cannot disappear behind the local patch.
    lock = 'version = 4\n'
    for manifest in manifests:
        lock += '\n[[package]]\n'
        for key, value in [("name", manifest["crate"]), ("version", manifest["version"]),
                           ("source", "registry+https://github.com/rust-lang/crates.io-index"),
                           ("checksum", manifest["upstream_crate_checksum"])]:
            lock += f'{key} = {json.dumps(value)}\n'
    with tempfile.TemporaryDirectory(prefix="own-backup-vendor-audit-") as folder:
        path = Path(folder) / "Cargo.lock"
        path.write_text(lock)
        result = subprocess.run([args.cargo_audit, "audit", "--file", str(path), "--json"],
                                text=True, capture_output=True, check=False)
    report = json.loads(result.stdout)
    assert not report["vulnerabilities"]["found"], "new vendor vulnerability"
    for kind, warnings in report["warnings"].items():
        for warning in warnings:
            fixed = (kind == "unsound" and warning["advisory"]["id"] == "RUSTSEC-2024-0429"
                     and warning["package"]["name"] == "glib" and warning["package"]["version"] == "0.18.5")
            assert fixed, f"unreviewed vendor advisory: {warning['advisory']['id']}"
            print("RUSTSEC-2024-0429: upstream advisory covered by the verified local fix")
    assert result.returncode == 0, result.stderr
    print("Original vendor versions: no additional advisories")
