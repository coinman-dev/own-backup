//! The SCSI commands a Windows initiator sends to a disk, answered from an
//! image.
//!
//! Small on purpose. Windows asks a new disk who it is, how big it is, whether
//! it is write-protected, and then reads and writes blocks; everything else it
//! can live without, and a command we do not implement is refused properly —
//! CHECK CONDITION with `INVALID COMMAND OPERATION CODE` — rather than answered
//! with zeros. A disk that lies about a command it did not understand is how a
//! filesystem ends up writing into nowhere.
//!
//! Every reply here is bytes in, bytes out: no sockets, no Windows, no image —
//! which is why this file carries the tests for the whole command set.

use tracing::{debug, trace, warn};

use super::Lun;

/// SCSI status codes (SAM-5 §5.3).
pub const STATUS_GOOD: u8 = 0x00;
pub const STATUS_CHECK_CONDITION: u8 = 0x02;

/// Sense keys (SPC-4 §4.5.6).
const KEY_NO_SENSE: u8 = 0x00;
const KEY_ILLEGAL_REQUEST: u8 = 0x05;
const KEY_DATA_PROTECT: u8 = 0x07;
const KEY_MEDIUM_ERROR: u8 = 0x03;

/// Additional sense codes, as (ASC, ASCQ).
const ASC_INVALID_OPCODE: (u8, u8) = (0x20, 0x00);
const ASC_INVALID_FIELD_IN_CDB: (u8, u8) = (0x24, 0x00);
const ASC_LBA_OUT_OF_RANGE: (u8, u8) = (0x21, 0x00);
const ASC_WRITE_PROTECTED: (u8, u8) = (0x27, 0x00);
const ASC_UNRECOVERED_READ: (u8, u8) = (0x11, 0x00);
const ASC_WRITE_FAULT: (u8, u8) = (0x0C, 0x00);

/// What one command produced: data to send back, a status, and — when the
/// status is CHECK CONDITION — the sense data explaining it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub data: Vec<u8>,
    pub status: u8,
    pub sense: Vec<u8>,
}

impl Reply {
    fn good(data: Vec<u8>) -> Self {
        Self { data, status: STATUS_GOOD, sense: Vec::new() }
    }
    fn ok() -> Self {
        Self::good(Vec::new())
    }
    fn fail(key: u8, asc: (u8, u8)) -> Self {
        Self { data: Vec::new(), status: STATUS_CHECK_CONDITION, sense: sense_data(key, asc) }
    }
}

/// Fixed-format sense data (SPC-4 §4.5.3), the 18 bytes every initiator
/// understands without asking for the descriptor format.
fn sense_data(key: u8, asc: (u8, u8)) -> Vec<u8> {
    let mut s = vec![0u8; 18];
    s[0] = 0x70; // current error, fixed format
    s[2] = key & 0x0F;
    s[7] = 10; // additional sense length: the ten bytes after this one
    s[12] = asc.0;
    s[13] = asc.1;
    s
}

/// Vendor and product as they appear in INQUIRY. Eight and sixteen bytes,
/// space-padded — the field widths are fixed and Windows shows them verbatim
/// in Device Manager, so a disk from a backup says so.
const T10_VENDOR: &[u8; 8] = b"OWNBACK ";
const PRODUCT_ID: &[u8; 16] = b"Backup Image    ";
const REVISION: &[u8; 4] = b"0001";

/// Run one command. `data_out` is what the initiator already sent for a write;
/// it is empty for everything else.
pub fn execute(lun: &mut Lun, cdb: &[u8], data_out: &[u8]) -> Reply {
    let Some(&opcode) = cdb.first() else {
        return Reply::fail(KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE);
    };
    match opcode {
        0x00 => Reply::ok(),                       // TEST UNIT READY
        0x12 => inquiry(lun, cdb),                 // INQUIRY
        0x1A => mode_sense(lun, cdb, false),       // MODE SENSE(6)
        0x5A => mode_sense(lun, cdb, true),        // MODE SENSE(10)
        0x25 => read_capacity_10(lun),             // READ CAPACITY(10)
        0x9E if cdb.len() >= 2 && cdb[1] & 0x1F == 0x10 => read_capacity_16(lun),
        0x28 | 0x88 => read(lun, cdb),             // READ(10) / READ(16)
        0x2A | 0x8A => write(lun, cdb, data_out),  // WRITE(10) / WRITE(16)
        0x35 | 0x91 => Reply::ok(),                // SYNCHRONIZE CACHE: writes are already through
        0x03 => Reply::good(sense_data(KEY_NO_SENSE, (0, 0))), // REQUEST SENSE
        0xA0 => report_luns(),                     // REPORT LUNS
        0x1B => Reply::ok(),                       // START STOP UNIT
        0x1E => Reply::ok(),                       // PREVENT ALLOW MEDIUM REMOVAL
        0x2F | 0x8F => Reply::ok(),                // VERIFY: the image is what it is
        _ => {
            // Refusing is the right answer and the initiator takes it as one;
            // the only question is whether anybody wants to read about it.
            if !lun.first_refusal_of(opcode) {
                trace!(opcode = format!("{opcode:#04x}"), "iSCSI: refused again");
            } else if refusal_is_expected(opcode) {
                debug!(
                    opcode = format!("{opcode:#04x}"),
                    "iSCSI: refusing a command a backup image is not meant to answer"
                );
            } else {
                warn!(opcode = format!("{opcode:#04x}"), "iSCSI: unsupported SCSI command");
            }
            Reply::fail(KEY_ILLEGAL_REQUEST, ASC_INVALID_OPCODE)
        }
    }
}

fn inquiry(lun: &Lun, cdb: &[u8]) -> Reply {
    let evpd = cdb.get(1).is_some_and(|b| b & 0x01 != 0);
    let page = *cdb.get(2).unwrap_or(&0);
    if !evpd {
        // A page code with EVPD off is a malformed request, not page zero.
        if page != 0 {
            return Reply::fail(KEY_ILLEGAL_REQUEST, ASC_INVALID_FIELD_IN_CDB);
        }
        let mut d = vec![0u8; 36];
        d[0] = 0x00; // direct-access block device, connected
        d[1] = 0x00; // not removable
        d[2] = 0x05; // SPC-3
        d[3] = 0x02; // response data format 2
        d[4] = 31; // additional length: everything after this byte
        d[8..16].copy_from_slice(T10_VENDOR);
        d[16..32].copy_from_slice(PRODUCT_ID);
        d[32..36].copy_from_slice(REVISION);
        return Reply::good(d);
    }
    match page {
        // Which vital product pages exist.
        0x00 => {
            let mut d = vec![0u8, 0x00, 0x00, 3];
            d.extend_from_slice(&[0x00, 0x80, 0x83]);
            Reply::good(d)
        }
        // Unit serial number.
        0x80 => {
            let serial = lun.serial().as_bytes().to_vec();
            let mut d = vec![0u8, 0x80, 0x00, serial.len() as u8];
            d.extend_from_slice(&serial);
            Reply::good(d)
        }
        // Device identification: one T10-vendor-ID designator. Windows uses it
        // to tell this disk apart from every other one it has seen, which is
        // what keeps two mounted images from being confused for each other.
        0x83 => {
            let mut id = Vec::new();
            id.extend_from_slice(T10_VENDOR);
            id.extend_from_slice(PRODUCT_ID);
            id.extend_from_slice(lun.serial().as_bytes());
            let mut desc = vec![0x02, 0x01, 0x00, id.len() as u8];
            desc.extend_from_slice(&id);
            let mut d = vec![0u8, 0x83];
            d.extend_from_slice(&(desc.len() as u16).to_be_bytes());
            d.extend_from_slice(&desc);
            Reply::good(d)
        }
        _ => Reply::fail(KEY_ILLEGAL_REQUEST, ASC_INVALID_FIELD_IN_CDB),
    }
}

/// The header alone, plus the caching page when it is asked for. The one bit
/// that matters is write protection: a read-only mount must say so here, or
/// Windows mounts the volume read-write and the first metadata write fails
/// somewhere far less obvious.
fn mode_sense(lun: &Lun, cdb: &[u8], ten: bool) -> Reply {
    let page = cdb.get(2).map(|b| b & 0x3F).unwrap_or(0);
    let wp = if lun.is_read_only() { 0x80 } else { 0x00 };

    let mut pages = Vec::new();
    if page == 0x08 || page == 0x3F {
        let mut caching = vec![0u8; 20];
        caching[0] = 0x08; // caching mode page
        caching[1] = 18; // page length
        pages.extend_from_slice(&caching);
    }

    let mut d = Vec::new();
    if ten {
        d.extend_from_slice(&[0u8; 8]);
        let len = (pages.len() + 6) as u16; // everything after the length field
        d[0..2].copy_from_slice(&len.to_be_bytes());
        d[3] = wp;
    } else {
        d.extend_from_slice(&[0u8; 4]);
        d[0] = (pages.len() + 3) as u8;
        d[2] = wp;
    }
    d.extend_from_slice(&pages);
    Reply::good(d)
}

fn read_capacity_10(lun: &Lun) -> Reply {
    let last = lun.blocks().saturating_sub(1);
    let mut d = vec![0u8; 8];
    // The 10-byte form cannot express more than 2 TiB; the all-ones answer is
    // how it tells the initiator to ask again with the 16-byte one.
    let reported = if last > u32::MAX as u64 { u32::MAX } else { last as u32 };
    d[0..4].copy_from_slice(&reported.to_be_bytes());
    d[4..8].copy_from_slice(&lun.block_size().to_be_bytes());
    Reply::good(d)
}

fn read_capacity_16(lun: &Lun) -> Reply {
    let mut d = vec![0u8; 32];
    d[0..8].copy_from_slice(&lun.blocks().saturating_sub(1).to_be_bytes());
    d[8..12].copy_from_slice(&lun.block_size().to_be_bytes());
    Reply::good(d)
}

fn report_luns() -> Reply {
    let mut d = vec![0u8; 16];
    d[0..4].copy_from_slice(&8u32.to_be_bytes()); // one entry follows
    Reply::good(d) // LUN 0, all zeros
}

/// Commands nobody should expect an image to answer, so refusing one is not
/// news.
///
/// `ATA PASS-THROUGH` (0x85 and 0xA1) asks a SCSI device to forward an ATA
/// command to the drive behind it — SMART, almost always. There is no drive
/// behind this LUN to forward anything to. The vendor-specific range
/// (0xC0-0xFF, SPC-4 §5.1) is by definition whatever the maker of a real disk
/// chose to put there, so a target that does not implement it is not missing
/// anything either.
///
/// Both arrive in bursts from whatever watches this machine's disks, about
/// once a second for as long as an image is attached. As warnings they were
/// almost every line of a run's log and buried the one message that mattered
/// (2026-09-18) — the reason this classification exists.
fn refusal_is_expected(opcode: u8) -> bool {
    matches!(opcode, 0x85 | 0xA1) || opcode >= 0xC0
}

/// LBA and block count out of a READ/WRITE CDB, 10- or 16-byte form.
fn lba_and_count(cdb: &[u8]) -> Option<(u64, u64)> {
    match cdb.first()? {
        0x28 | 0x2A if cdb.len() >= 9 => {
            let lba = u32::from_be_bytes(cdb[2..6].try_into().ok()?) as u64;
            let count = u16::from_be_bytes(cdb[7..9].try_into().ok()?) as u64;
            Some((lba, count))
        }
        0x88 | 0x8A if cdb.len() >= 14 => {
            let lba = u64::from_be_bytes(cdb[2..10].try_into().ok()?);
            let count = u32::from_be_bytes(cdb[10..14].try_into().ok()?) as u64;
            Some((lba, count))
        }
        _ => None,
    }
}

fn read(lun: &mut Lun, cdb: &[u8]) -> Reply {
    let Some((lba, count)) = lba_and_count(cdb) else {
        return Reply::fail(KEY_ILLEGAL_REQUEST, ASC_INVALID_FIELD_IN_CDB);
    };
    if count == 0 {
        return Reply::ok(); // a legal way to say "nothing"
    }
    if lba.saturating_add(count) > lun.blocks() {
        return Reply::fail(KEY_ILLEGAL_REQUEST, ASC_LBA_OUT_OF_RANGE);
    }
    let bs = lun.block_size() as u64;
    let mut buf = vec![0u8; (count * bs) as usize];
    match lun.read_at(lba * bs, &mut buf) {
        Ok(()) => Reply::good(buf),
        Err(e) => {
            warn!(lba, count, error = %e, "iSCSI read failed");
            Reply::fail(KEY_MEDIUM_ERROR, ASC_UNRECOVERED_READ)
        }
    }
}

fn write(lun: &mut Lun, cdb: &[u8], data_out: &[u8]) -> Reply {
    let Some((lba, count)) = lba_and_count(cdb) else {
        return Reply::fail(KEY_ILLEGAL_REQUEST, ASC_INVALID_FIELD_IN_CDB);
    };
    if lun.is_read_only() {
        return Reply::fail(KEY_DATA_PROTECT, ASC_WRITE_PROTECTED);
    }
    if count == 0 {
        return Reply::ok();
    }
    if lba.saturating_add(count) > lun.blocks() {
        return Reply::fail(KEY_ILLEGAL_REQUEST, ASC_LBA_OUT_OF_RANGE);
    }
    let bs = lun.block_size() as u64;
    let want = (count * bs) as usize;
    // Short of the data the command promised: writing the part that arrived
    // would leave the rest of those blocks as whatever was there before.
    if data_out.len() < want {
        warn!(lba, count, got = data_out.len(), want, "iSCSI: write data short of the command");
        return Reply::fail(KEY_ILLEGAL_REQUEST, ASC_INVALID_FIELD_IN_CDB);
    }
    match lun.write_at(lba * bs, &data_out[..want]) {
        Ok(()) => Reply::ok(),
        Err(e) => {
            warn!(lba, count, error = %e, "iSCSI write failed");
            Reply::fail(KEY_MEDIUM_ERROR, ASC_WRITE_FAULT)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::block_source::{BlockSource, WritableBlockSource};
    use own_backup_common::Result;

    /// An image in memory, so the command set can be tested without one.
    pub(crate) struct MemSource {
        pub bytes: Vec<u8>,
    }

    impl MemSource {
        pub(crate) fn new(size: usize) -> Self {
            Self { bytes: vec![0u8; size] }
        }
    }

    impl BlockSource for MemSource {
        fn size(&self) -> u64 {
            self.bytes.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let at = offset as usize;
            buf.copy_from_slice(&self.bytes[at..at + buf.len()]);
            Ok(())
        }
    }

    impl WritableBlockSource for MemSource {
        fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
            let at = offset as usize;
            self.bytes[at..at + data.len()].copy_from_slice(data);
            Ok(())
        }
    }

    fn rw_lun(size: usize) -> Lun {
        Lun::read_write(Box::new(MemSource::new(size)))
    }

    #[test]
    fn inquiry_says_what_this_disk_is() {
        let mut lun = rw_lun(1 << 20);
        let r = execute(&mut lun, &[0x12, 0, 0, 0, 36, 0], &[]);
        assert_eq!(r.status, STATUS_GOOD);
        assert_eq!(r.data.len(), 36);
        assert_eq!(r.data[0], 0x00, "direct-access block device");
        assert_eq!(&r.data[8..16], T10_VENDOR);
        assert_eq!(&r.data[16..32], PRODUCT_ID);
    }

    #[test]
    fn the_vital_pages_are_the_ones_page_zero_promises() {
        let mut lun = rw_lun(1 << 20);
        let listed = execute(&mut lun, &[0x12, 0x01, 0x00, 0, 255, 0], &[]);
        assert_eq!(&listed.data[4..], &[0x00, 0x80, 0x83]);
        for page in [0x80u8, 0x83] {
            let r = execute(&mut lun, &[0x12, 0x01, page, 0, 255, 0], &[]);
            assert_eq!(r.status, STATUS_GOOD, "page {page:#04x}");
            assert_eq!(r.data[1], page);
        }
        // A page nobody announced is refused, not invented.
        let r = execute(&mut lun, &[0x12, 0x01, 0x99, 0, 255, 0], &[]);
        assert_eq!(r.status, STATUS_CHECK_CONDITION);
    }

    #[test]
    fn capacity_is_the_last_block_not_the_count() {
        let mut lun = rw_lun(4096); // 8 blocks of 512
        let r = execute(&mut lun, &[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], &[]);
        assert_eq!(u32::from_be_bytes(r.data[0..4].try_into().unwrap()), 7);
        assert_eq!(u32::from_be_bytes(r.data[4..8].try_into().unwrap()), 512);
        let r16 = execute(&mut lun, &[0x9E, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 0, 0], &[]);
        assert_eq!(u64::from_be_bytes(r16.data[0..8].try_into().unwrap()), 7);
    }

    #[test]
    fn blocks_go_out_and_come_back_where_the_lba_says() {
        let mut lun = rw_lun(4096);
        let payload = vec![0xAB; 1024]; // two blocks
        let w = execute(&mut lun, &[0x2A, 0, 0, 0, 0, 2, 0, 0, 2, 0], &payload);
        assert_eq!(w.status, STATUS_GOOD);
        let r = execute(&mut lun, &[0x28, 0, 0, 0, 0, 2, 0, 0, 2, 0], &[]);
        assert_eq!(r.data, payload);
        // ...and nothing landed in the block before it.
        let before = execute(&mut lun, &[0x28, 0, 0, 0, 0, 1, 0, 0, 1, 0], &[]);
        assert_eq!(before.data, vec![0u8; 512]);
    }

    #[test]
    fn the_sixteen_byte_forms_address_the_same_blocks() {
        let mut lun = rw_lun(4096);
        let payload = vec![0xCD; 512];
        let mut w = vec![0x8A, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 1, 0, 0];
        assert_eq!(execute(&mut lun, &w, &payload).status, STATUS_GOOD);
        w[0] = 0x88;
        assert_eq!(execute(&mut lun, &w, &[]).data, payload);
    }

    /// Past the end of the image is a refusal with a reason, never a short read
    /// dressed up as success.
    #[test]
    fn reads_past_the_end_are_refused() {
        let mut lun = rw_lun(4096);
        let r = execute(&mut lun, &[0x28, 0, 0, 0, 0, 7, 0, 0, 4, 0], &[]);
        assert_eq!(r.status, STATUS_CHECK_CONDITION);
        assert_eq!(r.sense[2] & 0x0F, KEY_ILLEGAL_REQUEST);
        assert_eq!((r.sense[12], r.sense[13]), ASC_LBA_OUT_OF_RANGE);
    }

    /// A read-only mount must say so in MODE SENSE and refuse the write with
    /// DATA PROTECT — the two halves of the same promise.
    #[test]
    fn a_read_only_lun_is_write_protected_everywhere_it_is_asked() {
        let mut lun = Lun::read_only(Box::new(MemSource::new(4096)));
        let six = execute(&mut lun, &[0x1A, 0, 0x3F, 0, 255, 0], &[]);
        assert_eq!(six.data[2] & 0x80, 0x80, "write-protect bit in MODE SENSE(6)");
        let ten = execute(&mut lun, &[0x5A, 0, 0x3F, 0, 0, 0, 0, 0, 255, 0], &[]);
        assert_eq!(ten.data[3] & 0x80, 0x80, "write-protect bit in MODE SENSE(10)");
        let w = execute(&mut lun, &[0x2A, 0, 0, 0, 0, 0, 0, 0, 1, 0], &vec![0xFF; 512]);
        assert_eq!(w.status, STATUS_CHECK_CONDITION);
        assert_eq!(w.sense[2] & 0x0F, KEY_DATA_PROTECT);
    }

    /// Data short of what the command promised would leave part of those blocks
    /// holding whatever was there before.
    #[test]
    fn a_write_whose_data_did_not_all_arrive_is_refused() {
        let mut lun = rw_lun(4096);
        let r = execute(&mut lun, &[0x2A, 0, 0, 0, 0, 0, 0, 0, 2, 0], &vec![0x11; 512]);
        assert_eq!(r.status, STATUS_CHECK_CONDITION);
        assert_eq!(execute(&mut lun, &[0x28, 0, 0, 0, 0, 0, 0, 0, 1, 0], &[]).data, vec![0u8; 512]);
    }

    #[test]
    fn a_command_we_do_not_know_is_refused_not_answered() {
        let mut lun = rw_lun(4096);
        let r = execute(&mut lun, &[0xDE, 0, 0, 0, 0, 0], &[]);
        assert_eq!(r.status, STATUS_CHECK_CONDITION);
        assert_eq!((r.sense[12], r.sense[13]), ASC_INVALID_OPCODE);
    }

    /// Which refusals are worth a word. ATA pass-through and the
    /// vendor-specific range come from whatever polls the machine's disks and
    /// mean nothing here; a real SCSI command we have not implemented is a gap
    /// somebody should hear about.
    #[test]
    fn a_refusal_nobody_can_help_is_told_apart_from_a_gap() {
        for op in [0x85u8, 0xA1, 0xC0, 0xD8, 0xDF, 0xE0, 0xE4, 0xE6, 0xF8, 0xFF] {
            assert!(refusal_is_expected(op), "{op:#04x} is nothing to warn about");
        }
        for op in [0x24u8, 0x41, 0x42, 0x93] {
            assert!(!refusal_is_expected(op), "{op:#04x} is a command we could have");
        }
    }

    /// The same opcode a second time is not news either way, and a poller
    /// sends it hundreds of times per attach. The REFUSAL itself never changes.
    #[test]
    fn the_same_refusal_is_reported_once_but_answered_every_time() {
        let mut lun = rw_lun(4096);
        assert!(lun.first_refusal_of(0xDF));
        assert!(!lun.first_refusal_of(0xDF), "the second one is the same line");
        assert!(lun.first_refusal_of(0xE4), "another opcode has its own first time");
        for _ in 0..3 {
            let r = execute(&mut lun, &[0xDF, 0, 0, 0, 0, 0], &[]);
            assert_eq!(r.status, STATUS_CHECK_CONDITION);
            assert_eq!((r.sense[12], r.sense[13]), ASC_INVALID_OPCODE);
        }
    }

    #[test]
    fn report_luns_lists_the_one_lun_there_is() {
        let mut lun = rw_lun(4096);
        let r = execute(&mut lun, &[0xA0, 0, 0, 0, 0, 0, 0, 0, 16, 0, 0, 0], &[]);
        assert_eq!(u32::from_be_bytes(r.data[0..4].try_into().unwrap()), 8);
        assert_eq!(r.data.len(), 16);
    }
}
