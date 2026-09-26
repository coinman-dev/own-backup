//! Reading what a `.wim` says about itself.
//!
//! The boot medium is built on top of an image found on the user's own machine,
//! and "found" is doing a lot of work there: `Winre.wim` lives in different
//! places on different installations, may be absent, and a file with the right
//! name is not necessarily the right file. Building a medium out of the wrong
//! image fails at boot, on the user's hardware, in the middle of a rescue — so
//! the image is read before anything is copied.
//!
//! Only the header and the XML tail are parsed. That is enough to answer every
//! question the builder has (is this a WIM, how many images, what Windows is it,
//! is it a WinPE edition) and it costs two small reads regardless of the file's
//! size — this one is 439 MB.
//!
//! WIM header layout used here (all little-endian), from the WIM format spec:
//!
//! | offset | size | field                                          |
//! |--------|------|------------------------------------------------|
//! | 0x00   | 8    | magic `MSWIM\0\0\0`                            |
//! | 0x08   | 4    | header size                                    |
//! | 0x0C   | 4    | version                                        |
//! | 0x10   | 4    | flags                                          |
//! | 0x18   | 16   | WIM GUID                                       |
//! | 0x28   | 2+2  | part number / total parts                      |
//! | 0x2C   | 4    | image count                                    |
//! | 0x48   | 24   | XML data resource: size, offset, original size |

use own_backup_common::{tr, OwnBackupError, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// `MSWIM\0\0\0` — the first eight bytes of every WIM.
pub const WIM_MAGIC: &[u8; 8] = b"MSWIM\0\0\0";

/// What a WIM says about itself, as far as the media builder cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WimInfo {
    /// Number of images inside. A base image for the medium needs at least one.
    pub image_count: u32,
    /// Split WIMs (`.swm`) carry a part number; anything but 1/1 cannot be used
    /// as a base without joining the parts first.
    pub part_number: u16,
    pub total_parts: u16,
    /// Name of the first image ("Microsoft Windows Recovery Environment (x64)").
    pub name: String,
    /// `WindowsPE` for WinRE and WinPE images — anything else is an installation
    /// image, which is a different thing wearing the same file extension.
    pub edition_id: String,
    pub architecture: String,
    /// Build number of the Windows the image came from (19041, 22621, …).
    pub build: u32,
    /// Language of the first image, e.g. `en-US` — WinPE ships one.
    pub language: String,
    /// Uncompressed size of everything inside, from the XML. Useful for telling
    /// the user how much room the build will need.
    pub total_bytes: u64,
}

impl WimInfo {
    /// A WinRE/WinPE image, as opposed to an installation image.
    pub fn is_windows_pe(&self) -> bool {
        self.edition_id.eq_ignore_ascii_case("WindowsPE")
    }

    /// x64 — the only architecture this product builds for.
    pub fn is_x64(&self) -> bool {
        let a = self.architecture.to_ascii_lowercase();
        a == "x64" || a == "amd64" || a == "9"
    }

    /// Windows releases that share a servicing branch, and therefore drivers and
    /// components: 19041/19042/…/19045 are one Windows 10, 22621/22631 one
    /// Windows 11. Same branch means a medium built on one runs happily on the
    /// other; a different branch is not a refusal, but the user should hear it.
    pub fn same_branch_as(&self, other_build: u32) -> bool {
        fn branch(b: u32) -> u32 {
            match b {
                19041..=19045 => 19041, // Windows 10 2004 … 22H2
                22000..=22999 => 22000, // Windows 11 21H2 … 23H2
                b => b,
            }
        }
        branch(self.build) == branch(other_build)
    }
}

/// Read a WIM's header and XML metadata.
///
/// Errors describe what is wrong with the FILE, because that is what the user
/// has to act on: a missing image is not the same problem as a file that is not
/// a WIM at all.
pub fn read_wim_info(path: &Path) -> Result<WimInfo> {
    let mut f = std::fs::File::open(path)
        .map_err(|e| OwnBackupError::ImageFormat(format!("{}: {e}", path.display())))?;

    let mut header = [0u8; 0x60];
    f.read_exact(&mut header).map_err(|_| {
        OwnBackupError::ImageFormat(tr!("wim_info.too_small", path = path.display()).into_owned())
    })?;
    if &header[..8] != WIM_MAGIC {
        return Err(OwnBackupError::ImageFormat(
            tr!("wim_info.not_wim", path = path.display()).into_owned(),
        ));
    }

    let u16at = |o: usize| u16::from_le_bytes([header[o], header[o + 1]]);
    let u32at = |o: usize| u32::from_le_bytes(header[o..o + 4].try_into().unwrap());
    let u64at = |o: usize| u64::from_le_bytes(header[o..o + 8].try_into().unwrap());

    let image_count = u32at(0x2C);
    let part_number = u16at(0x28);
    let total_parts = u16at(0x2A);

    // The XML resource header is 24 bytes: size (with flags in the top byte),
    // offset, uncompressed size. XML data is never compressed, so size is real.
    let xml_size = (u64at(0x48) & 0x00FF_FFFF_FFFF_FFFF) as usize;
    let xml_offset = u64at(0x50);

    if image_count == 0 {
        return Err(OwnBackupError::ImageFormat(
            tr!("wim_info.no_images", path = path.display()).into_owned(),
        ));
    }
    if xml_size == 0 || xml_size > 64 * 1024 * 1024 {
        return Err(OwnBackupError::ImageFormat(
            tr!("wim_info.no_description", path = path.display()).into_owned(),
        ));
    }

    f.seek(SeekFrom::Start(xml_offset))
        .map_err(|e| OwnBackupError::ImageFormat(format!("{}: {e}", path.display())))?;
    let mut xml_bytes = vec![0u8; xml_size];
    f.read_exact(&mut xml_bytes).map_err(|_| {
        OwnBackupError::ImageFormat(
            tr!("wim_info.description_cut_short", path = path.display()).into_owned(),
        )
    })?;

    let xml = decode_utf16le(&xml_bytes);
    Ok(WimInfo {
        image_count,
        part_number,
        total_parts,
        name: tag(&xml, "NAME").unwrap_or_default(),
        edition_id: tag(&xml, "EDITIONID").unwrap_or_default(),
        architecture: arch_name(tag(&xml, "ARCH").as_deref()),
        build: tag(&xml, "BUILD").and_then(|b| b.parse().ok()).unwrap_or(0),
        language: tag(&xml, "LANGUAGE").unwrap_or_default(),
        total_bytes: tag(&xml, "TOTALBYTES").and_then(|b| b.parse().ok()).unwrap_or(0),
    })
}

/// The XML is UTF-16LE with a BOM. Decoded by hand rather than with a crate:
/// it is a handful of ASCII tags, and a malformed byte must not abort the read —
/// the caller still wants the image count it already has.
fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .skip_while(|&u| u == 0xFEFF) // BOM
        .collect();
    String::from_utf16_lossy(&units)
}

/// First `<TAG>value</TAG>` in the document. The XML describes every image in
/// the file and we want the first one's values, which is what a plain forward
/// search gives.
fn tag(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    let v = xml[start..end].trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// `<ARCH>` is a number in the XML (the `PROCESSOR_ARCHITECTURE` value), which
/// tells the user nothing. Some images carry a name instead, so both are taken.
fn arch_name(raw: Option<&str>) -> String {
    match raw {
        Some("0") => "x86".into(),
        Some("5") => "ARM".into(),
        Some("6") => "IA64".into(),
        Some("9") => "x64".into(),
        Some("12") => "ARM64".into(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a WIM header + XML tail. Enough of a file for the reader; the image
    /// data a real WIM would carry sits between them and is never touched.
    fn fake_wim(image_count: u32, xml: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wim");
        let mut xml_bytes: Vec<u8> = vec![0xFF, 0xFE]; // BOM
        for u in xml.encode_utf16() {
            xml_bytes.extend_from_slice(&u.to_le_bytes());
        }
        let mut header = vec![0u8; 0x60];
        header[..8].copy_from_slice(WIM_MAGIC);
        header[0x28..0x2A].copy_from_slice(&1u16.to_le_bytes());
        header[0x2A..0x2C].copy_from_slice(&1u16.to_le_bytes());
        header[0x2C..0x30].copy_from_slice(&image_count.to_le_bytes());
        header[0x48..0x50].copy_from_slice(&(xml_bytes.len() as u64).to_le_bytes());
        header[0x50..0x58].copy_from_slice(&0x60u64.to_le_bytes());
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&header).unwrap();
        f.write_all(&xml_bytes).unwrap();
        (dir, path)
    }

    const WINRE_XML: &str = "<WIM><IMAGE INDEX=\"1\"><NAME>Microsoft Windows Recovery \
        Environment (x64)</NAME><TOTALBYTES>3148099812</TOTALBYTES><WINDOWS><ARCH>9</ARCH>\
        <EDITIONID>WindowsPE</EDITIONID><VERSION><MAJOR>10</MAJOR><MINOR>0</MINOR>\
        <BUILD>19041</BUILD></VERSION><LANGUAGES><LANGUAGE>en-US</LANGUAGE></LANGUAGES>\
        </WINDOWS></IMAGE></WIM>";

    #[test]
    fn reads_a_winre_image() {
        let (_d, p) = fake_wim(1, WINRE_XML);
        let info = read_wim_info(&p).unwrap();
        assert_eq!(info.image_count, 1);
        assert_eq!(info.edition_id, "WindowsPE");
        assert_eq!(info.architecture, "x64");
        assert_eq!(info.build, 19041);
        assert_eq!(info.language, "en-US");
        assert_eq!(info.total_bytes, 3_148_099_812);
        assert!(info.is_windows_pe());
        assert!(info.is_x64());
    }

    #[test]
    fn rejects_a_file_that_is_not_a_wim() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("not.wim");
        std::fs::write(&p, vec![0u8; 1024]).unwrap();
        let e = read_wim_info(&p).unwrap_err().to_string();
        assert!(e.contains("не образ WIM"), "{e}");
    }

    /// A file may be a WIM and still be useless as a base — say which of the two
    /// problems it is.
    #[test]
    fn rejects_a_wim_with_no_images() {
        let (_d, p) = fake_wim(0, WINRE_XML);
        let e = read_wim_info(&p).unwrap_err().to_string();
        assert!(e.contains("нет ни одной системы"), "{e}");
    }

    #[test]
    fn reports_a_truncated_file_rather_than_guessing() {
        let (_d, p) = fake_wim(1, WINRE_XML);
        let len = std::fs::metadata(&p).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_len(len - 40)
            .unwrap();
        let e = read_wim_info(&p).unwrap_err().to_string();
        assert!(e.contains("обрывается"), "{e}");
    }

    /// An installation image is a WIM with images and correct architecture — the
    /// edition is the only thing that tells it apart from a base we can use.
    #[test]
    fn an_install_image_is_not_windows_pe() {
        let xml = WINRE_XML.replace("WindowsPE", "Professional");
        let (_d, p) = fake_wim(2, &xml);
        let info = read_wim_info(&p).unwrap();
        assert!(!info.is_windows_pe());
        assert_eq!(info.image_count, 2);
    }

    #[test]
    fn servicing_branches_group_by_release_not_by_build() {
        let (_d, p) = fake_wim(1, WINRE_XML);
        let info = read_wim_info(&p).unwrap(); // 19041
        assert!(info.same_branch_as(19045), "19041 and 19045 are one Windows 10");
        assert!(!info.same_branch_as(22621), "Windows 11 is a different branch");
        assert!(!info.same_branch_as(17763), "1809 is a different branch");
    }
}
