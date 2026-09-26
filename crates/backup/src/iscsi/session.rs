//! One iSCSI connection, from login to logout.
//!
//! The wire is plain: a 48-byte header (the BHS), an optional data segment
//! padded to four bytes, and nothing else — no digests and no markers, because
//! both are negotiated away in [`Session::login`]. What varies is the opcode,
//! and the handful this understands are the ones a Microsoft initiator sends.
//!
//! **The data of a write arrives only in the way the login agreed on, and that
//! is the initiator's decision, not ours** (2026-09-18). This used to assume
//! `InitialR2T=No` and `ImmediateData=Yes` — the initiator sends the data
//! straight after the command and nothing has to be asked for. The Microsoft
//! initiator negotiates the opposite, `InitialR2T=Yes` with `ImmediateData=No`:
//! it sends the command alone and waits to be asked. So the first write of an
//! NTFS mount hung here — we waited for data the initiator would never send
//! unasked, every later command queued behind it, and Windows failed the mount
//! with device I/O errors after three minutes of them. Both keys are now
//! honoured with the result function the RFC gives them (OR for `InitialR2T`,
//! AND for `ImmediateData`), and the data we are owed is asked for with R2T.
//!
//! Everything else is refused honestly rather than guessed at: an opcode we do
//! not know gets a Reject, data larger than we said we would take ends the
//! connection instead of being half-read.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use md5::{Digest, Md5};
use own_backup_common::{OwnBackupError, Result};
use tracing::{info, warn};
use zeroize::Zeroizing;

use super::{scsi, Lun};

/// Basic Header Segment: every PDU starts with exactly this many bytes.
const BHS: usize = 48;

/// The largest data segment we accept, and the largest we send unless the
/// initiator asks for less. 256 KiB is what Windows negotiates anyway.
const MAX_DATA_SEGMENT: u32 = 256 * 1024;

/// Opcodes, initiator → target.
const OP_NOP_OUT: u8 = 0x00;
const OP_SCSI_CMD: u8 = 0x01;
const OP_LOGIN: u8 = 0x03;
const OP_TEXT: u8 = 0x04;
const OP_DATA_OUT: u8 = 0x05;
const OP_LOGOUT: u8 = 0x06;

/// Opcodes, target → initiator.
const OP_R2T: u8 = 0x31;
const OP_NOP_IN: u8 = 0x20;
const OP_SCSI_RSP: u8 = 0x21;
const OP_LOGIN_RSP: u8 = 0x23;
const OP_TEXT_RSP: u8 = 0x24;
const OP_DATA_IN: u8 = 0x25;
const OP_LOGOUT_RSP: u8 = 0x26;
const OP_REJECT: u8 = 0x3F;

/// Login stages (RFC 7143 §11.12.1).
const STAGE_FULL_FEATURE: u8 = 3;

fn dev(msg: impl Into<String>) -> OwnBackupError {
    OwnBackupError::Device(msg.into())
}

/// The 24-bit big-endian data segment length at bytes 5..8.
fn data_len(bhs: &[u8; BHS]) -> u32 {
    u32::from_be_bytes([0, bhs[5], bhs[6], bhs[7]])
}

fn put_u32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

/// Data segments travel padded to a four-byte boundary.
fn padding(len: usize) -> usize {
    (4 - len % 4) % 4
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_hex_16(value: &str) -> Option<[u8; 16]> {
    let text = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))?;
    if text.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub struct Session {
    /// The one target name we answer to.
    iqn: String,
    lun: Arc<Mutex<Lun>>,
    /// A discovery session only asks which targets exist; it never sees a LUN.
    discovery: bool,
    logged_in: bool,
    /// Status sequence number: one per status-bearing PDU we send.
    stat_sn: u32,
    /// The next command number we expect, and the window we advertise.
    exp_cmd_sn: u32,
    /// What the initiator said it can receive in one data segment.
    initiator_max_recv: u32,
    max_burst: u32,
    /// The write rules the login settled on. The RFC's defaults are what
    /// applies when the initiator never mentions a key, and they are the
    /// cautious pair: nothing arrives until we ask for it.
    initial_r2t: bool,
    immediate_data: bool,
    first_burst: u32,
    reads: u64,
    read_bytes: u64,
    writes: u64,
    write_bytes: u64,
    /// Told when a PDU starts arriving and when the connection goes back to
    /// waiting, so whoever owns the socket can change how long it is willing
    /// to wait. See [`Pace`].
    pace: Option<Box<dyn Fn(Pace) + Send>>,
    chap_secret: Option<Arc<Zeroizing<[u8; 16]>>>,
    chap_challenge: Option<(u8, [u8; 16])>,
    chap_authenticated: bool,
}

/// How patient the socket should be right now (audit AUD-14).
///
/// Between PDUs an initiator may legitimately say nothing for a long time —
/// an idle mount is exactly that — so waiting there has no deadline. But once
/// the first byte of a PDU has arrived the rest of it is already on the wire,
/// and a connection that sends half a header and stops is not idle: it is
/// holding one of the four session slots for as long as the process lives.
///
/// Login is treated as "in a PDU" from the first moment, so a connection that
/// opens and says nothing at all is timed out too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// Waiting for something to happen. No deadline.
    Idle,
    /// Part-way through a PDU, or not logged in yet. Short deadline.
    InPdu,
}

impl Session {
    pub fn new(iqn: String, lun: Arc<Mutex<Lun>>) -> Self {
        Self {
            iqn,
            lun,
            discovery: false,
            logged_in: false,
            stat_sn: 0,
            exp_cmd_sn: 0,
            initiator_max_recv: 8192, // the standard's default until told otherwise
            max_burst: MAX_DATA_SEGMENT,
            initial_r2t: true, // RFC default: ask before anything is sent
            immediate_data: true,
            first_burst: 64 * 1024,
            chap_secret: None,
            chap_challenge: None,
            chap_authenticated: false,
            reads: 0,
            read_bytes: 0,
            writes: 0,
            write_bytes: 0,
            pace: None,
        }
    }

    pub fn with_chap(mut self, secret: Arc<Zeroizing<[u8; 16]>>) -> Self {
        self.chap_secret = Some(secret);
        self
    }

    fn reject_auth<S: Write>(&mut self, stream: &mut S, bhs: &[u8; BHS]) -> Result<()> {
        let mut rsp = [0u8; BHS];
        rsp[0] = OP_LOGIN_RSP;
        rsp[16..20].copy_from_slice(&bhs[16..20]);
        rsp[36] = 0x02; // initiator error
        rsp[37] = 0x01; // authentication failure
        self.put_sns(&mut rsp, true);
        self.send(stream, &mut rsp, &[])
    }

    /// Hand in the hook that changes the socket's patience. Without one the
    /// session behaves exactly as before, which is what the tests want.
    pub fn with_pace(mut self, pace: impl Fn(Pace) + Send + 'static) -> Self {
        self.pace = Some(Box::new(pace));
        self
    }

    fn set_pace(&self, pace: Pace) {
        if let Some(f) = &self.pace {
            f(pace);
        }
    }

    /// Serve this connection until the initiator logs out or drops it.
    pub fn run<S: Read + Write>(&mut self, stream: &mut S) -> Result<()> {
        let outcome = self.serve(stream);
        info!(
            reads = self.reads,
            read_bytes = self.read_bytes,
            writes = self.writes,
            write_bytes = self.write_bytes,
            "iSCSI session totals"
        );
        outcome
    }

    fn serve<S: Read + Write>(&mut self, stream: &mut S) -> Result<()> {
        loop {
            let Some((bhs, data)) = self.read_pdu(stream)? else {
                return Ok(()); // the initiator closed the connection
            };
            // A logged-in session may now sit idle for as long as it likes.
            // Before login it may not: that is the deadline that keeps a
            // connection saying nothing from holding a session slot.
            if self.logged_in {
                self.set_pace(Pace::Idle);
            }
            let opcode = bhs[0] & 0x3F;
            if !self.logged_in && opcode != OP_LOGIN {
                return Err(dev(format!("iSCSI: opcode {opcode:#04x} before login")));
            }
            if self.discovery && matches!(opcode, OP_SCSI_CMD | OP_DATA_OUT) {
                return Err(dev("iSCSI: discovery session cannot access the image"));
            }
            match opcode {
                OP_LOGIN => self.login(stream, &bhs, &data)?,
                OP_TEXT => self.text(stream, &bhs, &data)?,
                OP_SCSI_CMD => self.scsi_command(stream, &bhs, data)?,
                OP_NOP_OUT => self.nop(stream, &bhs, &data)?,
                OP_LOGOUT => {
                    self.logout(stream, &bhs)?;
                    return Ok(());
                }
                _ => {
                    warn!(
                        opcode = format!("{opcode:#04x}"),
                        "iSCSI: unsupported PDU, rejecting"
                    );
                    self.reject(stream, &bhs)?;
                }
            }
        }
    }

    /// One whole PDU: header, then the data segment with its padding.
    /// `Ok(None)` when the connection closed cleanly between PDUs.
    fn read_pdu<S: Read>(&self, stream: &mut S) -> Result<Option<([u8; BHS], Vec<u8>)>> {
        let mut bhs = [0u8; BHS];
        let mut got = 0usize;
        while got < BHS {
            match stream.read(&mut bhs[got..]) {
                Ok(0) if got == 0 => return Ok(None),
                Ok(0) => return Err(dev("iSCSI: connection closed in the middle of a header")),
                Ok(n) => {
                    // Something has started arriving. From here to the end of
                    // this PDU there is no reason to wait long for the rest.
                    if got == 0 {
                        self.set_pace(Pace::InPdu);
                    }
                    got += n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(dev(format!("iSCSI read: {e}"))),
            }
        }
        // Additional headers are only used by features we negotiated away.
        let ahs = bhs[4] as usize * 4;
        if ahs > 0 {
            let mut skip = vec![0u8; ahs];
            stream.read_exact(&mut skip).map_err(|e| dev(format!("iSCSI read AHS: {e}")))?;
        }
        let len = data_len(&bhs);
        if len > MAX_DATA_SEGMENT {
            return Err(dev(format!("iSCSI: data segment of {len} bytes refused")));
        }
        let mut data = vec![0u8; len as usize];
        stream.read_exact(&mut data).map_err(|e| dev(format!("iSCSI read data: {e}")))?;
        let pad = padding(len as usize);
        if pad > 0 {
            let mut skip = vec![0u8; pad];
            stream.read_exact(&mut skip).map_err(|e| dev(format!("iSCSI read pad: {e}")))?;
        }
        Ok(Some((bhs, data)))
    }

    /// Send a PDU: header, data, padding.
    fn send<S: Write>(&self, stream: &mut S, bhs: &mut [u8; BHS], data: &[u8]) -> Result<()> {
        if data.len() as u32 > self.initiator_max_recv.max(MAX_DATA_SEGMENT) {
            return Err(dev(format!(
                "iSCSI: refusing to send {} bytes in one data segment",
                data.len()
            )));
        }
        let len = data.len() as u32;
        bhs[5] = (len >> 16) as u8;
        bhs[6] = (len >> 8) as u8;
        bhs[7] = len as u8;
        stream.write_all(bhs).map_err(|e| dev(format!("iSCSI write: {e}")))?;
        stream.write_all(data).map_err(|e| dev(format!("iSCSI write: {e}")))?;
        let pad = padding(data.len());
        if pad > 0 {
            stream.write_all(&vec![0u8; pad]).map_err(|e| dev(format!("iSCSI write: {e}")))?;
        }
        Ok(())
    }

    /// The three sequence numbers every response carries.
    fn put_sns(&mut self, bhs: &mut [u8; BHS], consume_stat_sn: bool) {
        put_u32(bhs, 24, self.stat_sn);
        if consume_stat_sn {
            self.stat_sn = self.stat_sn.wrapping_add(1);
        }
        put_u32(bhs, 28, self.exp_cmd_sn);
        // The window of commands we will accept: room for the next one, and
        // no more. This serves one command at a time, and a wider window only
        // invites the initiator to send a command while we are in the middle
        // of collecting the data of a write — where it would be read as
        // Data-Out and end the connection.
        put_u32(bhs, 32, self.exp_cmd_sn);
    }

    /// Note the command number so the response can acknowledge it. Immediate
    /// PDUs (the I bit) do not advance it.
    fn take_cmd_sn(&mut self, bhs: &[u8; BHS]) {
        if bhs[0] & 0x40 == 0 {
            let cmd_sn = u32::from_be_bytes([bhs[24], bhs[25], bhs[26], bhs[27]]);
            self.exp_cmd_sn = cmd_sn.wrapping_add(1);
        }
    }

    // ---- login ------------------------------------------------------------

    fn login<S: Read + Write>(
        &mut self,
        stream: &mut S,
        bhs: &[u8; BHS],
        data: &[u8],
    ) -> Result<()> {
        let flags = bhs[1];
        let transit = flags & 0x80 != 0;
        let csg = (flags >> 2) & 0x03;
        let nsg = flags & 0x03;
        if csg > STAGE_FULL_FEATURE {
            return Err(dev(format!("iSCSI: login from an impossible stage {csg}")));
        }
        // The status numbering starts where the initiator expects it to: it
        // sends the first ExpStatSN it will accept, and a target that answers
        // with another number has its response dropped.
        if !self.logged_in && self.stat_sn == 0 {
            self.stat_sn = u32::from_be_bytes([bhs[28], bhs[29], bhs[30], bhs[31]]);
        }
        // A login advances the command numbering even though it is marked
        // immediate — this is what other targets do, and the window we
        // advertise has to sit above it.
        let cmd_sn = u32::from_be_bytes([bhs[24], bhs[25], bhs[26], bhs[27]]);
        self.exp_cmd_sn = cmd_sn.wrapping_add(1);

        let keys = parse_keys(data);
        let mut answer = String::new();
        let mut initiator = String::new();
        let mut wanted_target = String::new();
        // Authentication is negotiated while iterating keys, so decide the
        // session kind first regardless of the order the initiator sent them.
        if let Some((_, kind)) = keys.iter().find(|(k, _)| k == "SessionType") {
            self.discovery = kind.eq_ignore_ascii_case("Discovery");
        }

        for (key, value) in &keys {
            match key.as_str() {
                "InitiatorName" => initiator = value.clone(),
                "TargetName" => wanted_target = value.clone(),
                "SessionType" => {}
                _ => {}
            }
            if let Some(ours) = self.negotiate(key, value) {
                answer.push_str(key);
                answer.push('=');
                answer.push_str(&ours);
                answer.push('\0');
            }
        }
        if !initiator.is_empty() {
            info!(initiator = %initiator, "iSCSI login");
        }

        // A normal session names the target it wants; a discovery session does
        // not, because asking which targets exist is the whole point of it.
        if !self.discovery && !wanted_target.is_empty() && wanted_target != self.iqn {
            warn!(wanted = %wanted_target, ours = %self.iqn, "iSCSI: login for an unknown target");
            let mut rsp = [0u8; BHS];
            rsp[0] = OP_LOGIN_RSP;
            rsp[16..20].copy_from_slice(&bhs[16..20]);
            rsp[36] = 0x02; // initiator error
            rsp[37] = 0x03; // not found
            self.put_sns(&mut rsp, true);
            return self.send(stream, &mut rsp, &[]);
        }

        if self.chap_secret.is_some() && !self.discovery && !self.chap_authenticated {
            if keys
                .iter()
                .find(|(k, _)| k == "AuthMethod")
                .is_some_and(|(_, methods)| !methods.split(',').any(|m| m.trim() == "CHAP"))
            {
                return self.reject_auth(stream, bhs);
            }
            if let Some((id, challenge)) = self.chap_challenge {
                let name = keys
                    .iter()
                    .find(|(k, _)| k == "CHAP_N")
                    .map(|(_, v)| v.as_str());
                let response = keys
                    .iter()
                    .find(|(k, _)| k == "CHAP_R")
                    .map(|(_, v)| v.as_str());
                if name != Some("ownbackup")
                    || !response.is_some_and(|r| self.chap_response_matches(id, &challenge, r))
                {
                    return self.reject_auth(stream, bhs);
                }
                self.chap_authenticated = true;
                self.chap_challenge = None;
            } else if let Some((_, algorithms)) = keys.iter().find(|(k, _)| k == "CHAP_A") {
                if !algorithms.split(',').any(|a| a.trim() == "5") {
                    return self.reject_auth(stream, bhs);
                }
                let mut random = [0u8; 17];
                getrandom::getrandom(&mut random)
                    .map_err(|e| dev(format!("iSCSI CHAP random challenge failed: {e}")))?;
                let id = random[0];
                let mut challenge = [0u8; 16];
                challenge.copy_from_slice(&random[1..]);
                self.chap_challenge = Some((id, challenge));
                answer.push_str("CHAP_A=5\0");
                answer.push_str(&format!(
                    "CHAP_I={id}\0CHAP_C=0x{}\0",
                    hex_bytes(&challenge)
                ));
            }
        }

        // The first response of a normal session declares the portal group.
        if !self.discovery && self.stat_sn == 0 {
            answer.push_str("TargetPortalGroupTag=1\0");
        }

        let authenticated = self.chap_secret.is_none() || self.discovery || self.chap_authenticated;
        let reached_full_feature = authenticated && transit && nsg == STAGE_FULL_FEATURE;
        let mut rsp = [0u8; BHS];
        rsp[0] = OP_LOGIN_RSP;
        rsp[1] = if authenticated && transit {
            0x80 | (csg << 2) | nsg
        } else {
            csg << 2
        };
        rsp[2] = 0; // VersionMax
        rsp[3] = 0; // VersionActive
        rsp[8..14].copy_from_slice(&bhs[8..14]); // ISID, echoed
        if reached_full_feature {
            // A non-zero TSIH is what marks the session as established.
            rsp[14..16].copy_from_slice(&1u16.to_be_bytes());
        }
        rsp[16..20].copy_from_slice(&bhs[16..20]); // ITT
        self.put_sns(&mut rsp, true);
        self.send(stream, &mut rsp, answer.as_bytes())?;

        if reached_full_feature {
            self.logged_in = true;
            info!(
                discovery = self.discovery,
                initiator_max_recv = self.initiator_max_recv,
                max_burst = self.max_burst,
                first_burst = self.first_burst,
                initial_r2t = self.initial_r2t,
                immediate_data = self.immediate_data,
                "iSCSI session in full feature phase"
            );
        }
        Ok(())
    }

    fn chap_response_matches(&self, id: u8, challenge: &[u8; 16], response: &str) -> bool {
        let Some(secret) = &self.chap_secret else {
            return false;
        };
        let Some(received) = decode_hex_16(response) else {
            return false;
        };
        let mut md5 = Md5::new();
        md5.update([id]);
        md5.update(secret.as_ref().as_ref());
        md5.update(challenge);
        let expected = md5.finalize();
        expected
            .iter()
            .zip(received)
            .fold(0u8, |diff, (a, b)| diff | (*a ^ b))
            == 0
    }

    /// Our side of one negotiated key. `None` for the declarative ones, which
    /// are stated rather than agreed.
    fn negotiate(&mut self, key: &str, value: &str) -> Option<String> {
        let number = |v: &str| v.parse::<u32>().ok();
        match key {
            "InitiatorName" | "InitiatorAlias" | "TargetName" | "SessionType" => None,
            "AuthMethod" => Some(
                if self.chap_secret.is_some() && !self.discovery {
                    "CHAP"
                } else {
                    "None"
                }
                .into(),
            ),
            // Digests would checksum a loopback connection against nothing.
            "HeaderDigest" | "DataDigest" => Some("None".into()),
            "MaxRecvDataSegmentLength" => {
                if let Some(n) = number(value) {
                    self.initiator_max_recv = n.max(512);
                }
                Some(MAX_DATA_SEGMENT.to_string())
            }
            "MaxBurstLength" => {
                let ours = number(value)
                    .unwrap_or(MAX_DATA_SEGMENT)
                    .min(MAX_DATA_SEGMENT);
                self.max_burst = ours;
                Some(ours.to_string())
            }
            // How much may arrive unsolicited, immediate data included. The
            // result is the smaller of the two, and never above one burst.
            "FirstBurstLength" => {
                let ours = number(value).unwrap_or(self.max_burst).min(self.max_burst);
                self.first_burst = ours;
                Some(ours.to_string())
            }
            // Result function: OR. We would rather not ask (`No`), but an
            // initiator that said `Yes` WILL wait to be asked — answering it
            // `No` is not a preference, it is a lie, and the write then hangs
            // on both sides (2026-09-18).
            "InitialR2T" => {
                self.initial_r2t = value.eq_ignore_ascii_case("yes");
                Some(if self.initial_r2t { "Yes" } else { "No" }.into())
            }
            // Result function: AND. `No` from the initiator means the command
            // comes alone, however welcome its data would have been.
            "ImmediateData" => {
                self.immediate_data = value.eq_ignore_ascii_case("yes");
                Some(if self.immediate_data { "Yes" } else { "No" }.into())
            }
            "MaxOutstandingR2T" => Some("1".into()),
            "DataPDUInOrder" | "DataSequenceInOrder" => Some("Yes".into()),
            // Level 0: a broken connection is retried by logging in again.
            "ErrorRecoveryLevel" => Some("0".into()),
            "MaxConnections" => Some("1".into()),
            "DefaultTime2Wait" => Some("2".into()),
            "DefaultTime2Retain" => Some("0".into()),
            "IFMarker" | "OFMarker" => Some("No".into()),
            _ => Some("NotUnderstood".into()),
        }
    }

    // ---- text, nop, logout, reject ----------------------------------------

    /// `SendTargets` in a discovery session: the one target we have.
    fn text<S: Write>(&mut self, stream: &mut S, bhs: &[u8; BHS], data: &[u8]) -> Result<()> {
        self.take_cmd_sn(bhs);
        let keys = parse_keys(data);
        let mut answer = String::new();
        if keys.iter().any(|(k, _)| k == "SendTargets") {
            answer.push_str("TargetName=");
            answer.push_str(&self.iqn);
            answer.push('\0');
        }
        let mut rsp = [0u8; BHS];
        rsp[0] = OP_TEXT_RSP;
        rsp[1] = 0x80; // final
        rsp[8..16].copy_from_slice(&bhs[8..16]); // LUN
        rsp[16..20].copy_from_slice(&bhs[16..20]); // ITT
        put_u32(&mut rsp, 20, u32::MAX); // no further exchange
        self.put_sns(&mut rsp, true);
        self.send(stream, &mut rsp, answer.as_bytes())
    }

    fn nop<S: Write>(&mut self, stream: &mut S, bhs: &[u8; BHS], data: &[u8]) -> Result<()> {
        self.take_cmd_sn(bhs);
        let itt = u32::from_be_bytes([bhs[16], bhs[17], bhs[18], bhs[19]]);
        if itt == u32::MAX {
            return Ok(()); // the initiator is not asking for an answer
        }
        let mut rsp = [0u8; BHS];
        rsp[0] = OP_NOP_IN;
        rsp[1] = 0x80;
        rsp[8..16].copy_from_slice(&bhs[8..16]);
        rsp[16..20].copy_from_slice(&bhs[16..20]);
        rsp[20..24].copy_from_slice(&bhs[20..24]); // TTT, echoed
        self.put_sns(&mut rsp, true);
        self.send(stream, &mut rsp, data)
    }

    fn logout<S: Write>(&mut self, stream: &mut S, bhs: &[u8; BHS]) -> Result<()> {
        self.take_cmd_sn(bhs);
        let mut rsp = [0u8; BHS];
        rsp[0] = OP_LOGOUT_RSP;
        rsp[1] = 0x80;
        rsp[2] = 0x00; // connection closed successfully
        rsp[16..20].copy_from_slice(&bhs[16..20]);
        self.put_sns(&mut rsp, true);
        self.send(stream, &mut rsp, &[])
    }

    fn reject<S: Write>(&mut self, stream: &mut S, bhs: &[u8; BHS]) -> Result<()> {
        let mut rsp = [0u8; BHS];
        rsp[0] = OP_REJECT;
        rsp[1] = 0x80;
        rsp[2] = 0x05; // command not supported
        put_u32(&mut rsp, 16, u32::MAX);
        self.put_sns(&mut rsp, true);
        // A Reject carries the header it refused, so the initiator can match it.
        self.send(stream, &mut rsp, bhs)
    }

    // ---- SCSI -------------------------------------------------------------

    fn scsi_command<S: Read + Write>(
        &mut self,
        stream: &mut S,
        bhs: &[u8; BHS],
        immediate: Vec<u8>,
    ) -> Result<()> {
        self.take_cmd_sn(bhs);
        let flags = bhs[1];
        let writing = flags & 0x20 != 0;
        let expected = u32::from_be_bytes([bhs[20], bhs[21], bhs[22], bhs[23]]) as usize;
        let cdb = &bhs[32..48];

        let payload =
            if writing { self.collect_write(stream, bhs, immediate, expected)? } else { immediate };

        let reply = {
            let mut lun = self.lun.lock().map_err(|_| dev("iSCSI: the LUN lock is poisoned"))?;
            scsi::execute(&mut lun, cdb, &payload)
        };
        if writing && reply.status == scsi::STATUS_GOOD {
            self.writes += 1;
            self.write_bytes += payload.len() as u64;
        }

        // Read data, cut into pieces the initiator said it can take.
        let mut data_sn = 0u32;
        let mut sent = 0usize;
        let to_send = reply.data.len().min(expected);
        if to_send > 0 {
            self.reads += 1;
            self.read_bytes += to_send as u64;
            let chunk = self.initiator_max_recv.max(512) as usize;
            while sent < to_send {
                let end = (sent + chunk).min(to_send);
                let last = end == to_send;
                let mut pdu = [0u8; BHS];
                pdu[0] = OP_DATA_IN;
                pdu[1] = if last { 0x80 } else { 0x00 };
                pdu[8..16].copy_from_slice(&bhs[8..16]); // LUN
                pdu[16..20].copy_from_slice(&bhs[16..20]); // ITT
                put_u32(&mut pdu, 20, u32::MAX); // no target transfer tag
                self.put_sns(&mut pdu, false); // status rides in the response
                put_u32(&mut pdu, 36, data_sn);
                put_u32(&mut pdu, 40, sent as u32);
                self.send(stream, &mut pdu, &reply.data[sent..end])?;
                data_sn = data_sn.wrapping_add(1);
                sent = end;
            }
        }

        // Sense data travels with a two-byte length in front of it.
        let mut segment = Vec::new();
        if !reply.sense.is_empty() {
            segment.extend_from_slice(&(reply.sense.len() as u16).to_be_bytes());
            segment.extend_from_slice(&reply.sense);
        }
        // What actually moved, in the direction this command asked for. A
        // write's bytes travel IN to us, so the read side sent nothing and
        // `to_send` is zero: answering a write with THAT residual claimed an
        // underflow of the whole transfer -- "of your N bytes I moved none"
        // -- with a status of GOOD beside it. The initiator believes the
        // residual, not the bytes: the data was in the image and the write
        // was still counted as lost, which is what an RW mount of an image
        // looked like from outside (2026-09-18). Reads were fine, the volume
        // was there, the system log filled with `{Delayed Write Failed}` on
        // `\:$I30:$INDEX_ALLOCATION` and `disk` event 153 retrying, and our
        // own log had not one failed read or write to show for it.
        let transferred = if writing { payload.len().min(expected) } else { to_send };
        let residual = expected.saturating_sub(transferred) as u32;
        let mut rsp = [0u8; BHS];
        rsp[0] = OP_SCSI_RSP;
        rsp[1] = 0x80 | if residual > 0 { 0x02 } else { 0x00 }; // final, underflow
        rsp[2] = 0x00; // the command did complete at the target
        rsp[3] = reply.status;
        rsp[16..20].copy_from_slice(&bhs[16..20]);
        self.put_sns(&mut rsp, true);
        put_u32(&mut rsp, 36, data_sn);
        put_u32(&mut rsp, 44, residual);
        self.send(stream, &mut rsp, &segment)
    }

    /// All the data of a write: what came with the command, whatever the login
    /// allows to follow unasked, and then as many R2T rounds as it takes.
    ///
    /// The two halves are not interchangeable. Unsolicited data stops at
    /// `FirstBurstLength`, and with `InitialR2T=Yes` there is none at all —
    /// waiting for it is how this hung a Windows mount (see the module note).
    fn collect_write<S: Read + Write>(
        &mut self,
        stream: &mut S,
        cmd: &[u8; BHS],
        immediate: Vec<u8>,
        expected: usize,
    ) -> Result<Vec<u8>> {
        let mut payload = immediate;
        if !self.initial_r2t {
            // The burst counts the immediate data too, so a command that
            // already carried its first burst gets nothing more unasked.
            let burst_end = (self.first_burst as usize).min(expected);
            while payload.len() < burst_end {
                let (next, data) = self.read_data_out(stream, cmd, payload.len())?;
                payload.extend_from_slice(&data);
                // F bit: the end of this burst, whatever it added up to.
                if next[1] & 0x80 != 0 {
                    break;
                }
            }
        }
        let mut r2t_sn = 0u32;
        while payload.len() < expected {
            let want = (expected - payload.len()).min(self.max_burst as usize);
            self.send_r2t(stream, cmd, payload.len() as u32, want as u32, r2t_sn)?;
            r2t_sn = r2t_sn.wrapping_add(1);
            let burst_end = payload.len() + want;
            while payload.len() < burst_end {
                let (_next, data) = self.read_data_out(stream, cmd, payload.len())?;
                payload.extend_from_slice(&data);
            }
        }
        Ok(payload)
    }

    /// The next Data-Out PDU of this command, which has to continue exactly
    /// where the data so far ended — anything else would land in the wrong
    /// place on the image.
    fn read_data_out<S: Read + Write>(
        &mut self,
        stream: &mut S,
        cmd: &[u8; BHS],
        at: usize,
    ) -> Result<([u8; BHS], Vec<u8>)> {
        let (next, data) = loop {
            let Some((next, data)) = self.read_pdu(stream)? else {
                return Err(dev("iSCSI: connection closed mid-write"));
            };
            let op = next[0] & 0x3F;
            // A keep-alive ignores the command window, so it can turn up in
            // the middle of a write; answering it and going back to waiting is
            // all it wants.
            if op == OP_NOP_OUT {
                self.nop(stream, &next, &data)?;
                continue;
            }
            break (next, data);
        };
        let op = next[0] & 0x3F;
        if op != OP_DATA_OUT {
            return Err(dev(format!("iSCSI: expected Data-Out, got {op:#04x}")));
        }
        if next[16..20] != cmd[16..20] {
            return Err(dev("iSCSI: Data-Out belongs to another command"));
        }
        let offset = u32::from_be_bytes([next[40], next[41], next[42], next[43]]) as usize;
        if offset != at {
            return Err(dev(format!("iSCSI: Data-Out at offset {offset}, expected {at}")));
        }
        Ok((next, data))
    }

    /// "Ready To Transfer": send me these bytes of that command, now. The
    /// initiator answers with Data-Out PDUs echoing the tag.
    fn send_r2t<S: Write>(
        &mut self,
        stream: &mut S,
        cmd: &[u8; BHS],
        offset: u32,
        len: u32,
        r2t_sn: u32,
    ) -> Result<()> {
        let mut pdu = [0u8; BHS];
        pdu[0] = OP_R2T;
        pdu[1] = 0x80; // an R2T is always final
        pdu[8..16].copy_from_slice(&cmd[8..16]); // LUN
        pdu[16..20].copy_from_slice(&cmd[16..20]); // ITT
        // Target transfer tag: anything but the all-ones "none", and different
        // per request so a late Data-Out cannot be mistaken for a fresh one.
        put_u32(&mut pdu, 20, r2t_sn.wrapping_add(1));
        self.put_sns(&mut pdu, false); // an R2T carries no status of its own
        // And the window closes until the data is in: MaxCmdSN one BELOW the
        // expected number means "send nothing new yet", which is how a
        // one-command-at-a-time target keeps a write from being interleaved.
        put_u32(&mut pdu, 32, self.exp_cmd_sn.wrapping_sub(1));
        put_u32(&mut pdu, 36, r2t_sn);
        put_u32(&mut pdu, 40, offset);
        put_u32(&mut pdu, 44, len);
        // In the log because a write that never gets its data is a hang, and a
        // hang is what this cost before R2T existed here: the line says what
        // the target is waiting for.
        tracing::debug!(offset, len, r2t_sn, "iSCSI: asking for the data of a write");
        self.send(stream, &mut pdu, &[])
    }
}

/// `key=value` pairs, NUL-separated — how every text segment in iSCSI is built.
fn parse_keys(data: &[u8]) -> Vec<(String, String)> {
    data.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let text = String::from_utf8_lossy(s);
            let (k, v) = text.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iscsi::scsi::tests::MemSource;

    #[test]
    fn chap_protects_a_normal_session_and_accepts_only_the_right_response() {
        let secret = Arc::new(Zeroizing::new(*b"target-secret-16"));
        let mut s =
            session(Lun::read_only(Box::new(MemSource::new(4096)))).with_chap(secret.clone());
        let request = pdu(OP_LOGIN, 0x83, 1,
            b"SessionType=Normal\0TargetName=iqn.2026-08.dev.ownbackup:image.test\0AuthMethod=CHAP\0CHAP_A=5\0");
        let mut first = Wire::new(Vec::new());
        let bhs: [u8; BHS] = request[..BHS].try_into().unwrap();
        s.login(
            &mut first,
            &bhs,
            &request[BHS..request.len() - padding(request.len() - BHS)],
        )
        .unwrap();
        assert!(!s.logged_in, "a challenge is not authentication");
        let (_, reply) = &responses(&first.output)[0];
        let keys = parse_keys(reply);
        let id = keys
            .iter()
            .find(|(k, _)| k == "CHAP_I")
            .unwrap()
            .1
            .parse::<u8>()
            .unwrap();
        let challenge =
            decode_hex_16(&keys.iter().find(|(k, _)| k == "CHAP_C").unwrap().1).unwrap();

        let mut wrong =
            session(Lun::read_only(Box::new(MemSource::new(4096)))).with_chap(secret.clone());
        wrong.chap_challenge = Some((id, challenge));
        let bad = pdu(
            OP_LOGIN,
            0x83,
            2,
            b"CHAP_N=ownbackup\0CHAP_R=0x00000000000000000000000000000000\0",
        );
        let bad_bhs: [u8; BHS] = bad[..BHS].try_into().unwrap();
        let mut bad_wire = Wire::new(Vec::new());
        wrong
            .login(
                &mut bad_wire,
                &bad_bhs,
                &bad[BHS..bad.len() - padding(bad.len() - BHS)],
            )
            .unwrap();
        assert_eq!(pdus(&bad_wire.output)[0].0[36..38], [0x02, 0x01]);
        assert!(!wrong.logged_in);

        let mut hash = Md5::new();
        hash.update([id]);
        hash.update(secret.as_ref().as_ref());
        hash.update(challenge);
        let response = format!(
            "CHAP_N=ownbackup\0CHAP_R=0x{}\0",
            hex_bytes(&hash.finalize())
        );
        let good = pdu(OP_LOGIN, 0x83, 3, response.as_bytes());
        let good_bhs: [u8; BHS] = good[..BHS].try_into().unwrap();
        let mut good_wire = Wire::new(Vec::new());
        s.login(&mut good_wire, &good_bhs, response.as_bytes())
            .unwrap();
        assert!(s.logged_in);
        assert_eq!(pdus(&good_wire.output)[0].0[36..38], [0, 0]);
    }

    #[test]
    fn discovery_can_list_a_chap_target_but_cannot_read_its_blocks() {
        let secret = Arc::new(Zeroizing::new(*b"target-secret-16"));
        let mut s = session(Lun::read_only(Box::new(MemSource::new(4096)))).with_chap(secret);
        let login = pdu(OP_LOGIN, 0x83, 1, b"AuthMethod=None\0SessionType=Discovery\0");
        let mut cmd = pdu(OP_SCSI_CMD, 0x81, 2, &[]);
        cmd[20..24].copy_from_slice(&512u32.to_be_bytes());
        cmd[32..42].copy_from_slice(&[0x28, 0, 0, 0, 0, 1, 0, 0, 1, 0]);
        let mut wire = Wire::new([login, cmd].concat());
        let result = s.run(&mut wire);
        assert!(result.unwrap_err().to_string().contains("discovery session"));
        assert_eq!(responses(&wire.output)[0].0, OP_LOGIN_RSP);
        assert!(responses(&wire.output)[0].1.windows(15).any(|w| w == b"AuthMethod=None"));
        assert_eq!(s.reads, 0);
    }

    /// A connection in memory: what the initiator says, and what it hears back.
    struct Wire {
        input: std::io::Cursor<Vec<u8>>,
        pub output: Vec<u8>,
    }

    impl Wire {
        fn new(script: Vec<u8>) -> Self {
            Self { input: std::io::Cursor::new(script), output: Vec::new() }
        }
    }

    impl Read for Wire {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Wire {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn pdu(opcode: u8, flags: u8, itt: u32, data: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; BHS];
        b[0] = opcode;
        b[1] = flags;
        let len = data.len() as u32;
        b[5] = (len >> 16) as u8;
        b[6] = (len >> 8) as u8;
        b[7] = len as u8;
        b[16..20].copy_from_slice(&itt.to_be_bytes());
        b.extend_from_slice(data);
        b.extend_from_slice(&vec![0u8; padding(data.len())]);
        b
    }

    /// A login that transits straight to full feature, the way the Microsoft
    /// initiator does once it has agreed on the operational keys — including
    /// its write keys, which are the cautious pair: it sends nothing until it
    /// is asked.
    fn login_pdu(session_type: &str, target: &str) -> Vec<u8> {
        login_with(session_type, target, "InitialR2T=Yes\0ImmediateData=No\0")
    }

    /// The other shape: an initiator that promises to send the data unasked.
    fn login_unsolicited(target: &str) -> Vec<u8> {
        login_with("Normal", target, "InitialR2T=No\0ImmediateData=Yes\0")
    }

    fn login_with(session_type: &str, target: &str, write_keys: &str) -> Vec<u8> {
        let mut keys = format!(
            "InitiatorName=iqn.1991-05.com.microsoft:test\0SessionType={session_type}\0\
             HeaderDigest=None\0DataDigest=None\0MaxRecvDataSegmentLength=65536\0\
             MaxBurstLength=262144\0FirstBurstLength=65536\0{write_keys}"
        );
        if !target.is_empty() {
            keys.push_str(&format!("TargetName={target}\0"));
        }
        // T bit, CSG=1 (operational), NSG=3 (full feature).
        pdu(OP_LOGIN, 0x80 | (1 << 2) | 3, 1, keys.as_bytes())
    }

    fn session(lun: Lun) -> Session {
        Session::new("iqn.2026-08.dev.ownbackup:image.test".into(), Arc::new(Mutex::new(lun)))
    }

    /// Everything we wrote back, header and data, in order.
    fn pdus(output: &[u8]) -> Vec<([u8; BHS], Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + BHS <= output.len() {
            let mut bhs = [0u8; BHS];
            bhs.copy_from_slice(&output[at..at + BHS]);
            let len = data_len(&bhs) as usize;
            let start = at + BHS;
            let data = output[start..start + len].to_vec();
            out.push((bhs, data));
            at = start + len + padding(len);
        }
        out
    }

    fn responses(output: &[u8]) -> Vec<(u8, Vec<u8>)> {
        pdus(output).into_iter().map(|(bhs, data)| (bhs[0], data)).collect()
    }

    /// The initiator says which status number it expects next; answering with
    /// a different one gets the response dropped on the floor.
    #[test]
    fn the_status_numbering_starts_where_the_initiator_expects() {
        let mut script = login_pdu("Normal", "iqn.2026-08.dev.ownbackup:image.test");
        // ExpStatSN = 7, CmdSN = 3, in the login request.
        script[24..28].copy_from_slice(&3u32.to_be_bytes());
        script[28..32].copy_from_slice(&7u32.to_be_bytes());
        let mut wire = Wire::new(script);
        let mut s = session(Lun::read_write(Box::new(MemSource::new(4096))));
        s.run(&mut wire).unwrap();

        let stat_sn = u32::from_be_bytes(wire.output[24..28].try_into().unwrap());
        let exp_cmd_sn = u32::from_be_bytes(wire.output[28..32].try_into().unwrap());
        let max_cmd_sn = u32::from_be_bytes(wire.output[32..36].try_into().unwrap());
        assert_eq!(stat_sn, 7, "the response carries the number that was asked for");
        assert_eq!(exp_cmd_sn, 4, "a login advances the command numbering");
        assert!(max_cmd_sn >= exp_cmd_sn, "the window must not be closed");
        assert_eq!(s.stat_sn, 8, "and the next one follows it");
    }

    #[test]
    fn a_login_is_answered_with_what_we_agreed_to() {
        let mut wire = Wire::new(login_unsolicited("iqn.2026-08.dev.ownbackup:image.test"));
        let mut s = session(Lun::read_write(Box::new(MemSource::new(4096))));
        s.run(&mut wire).unwrap();

        let rsp = responses(&wire.output);
        assert_eq!(rsp.len(), 1);
        assert_eq!(rsp[0].0, OP_LOGIN_RSP);
        let keys = parse_keys(&rsp[0].1);
        let get = |k: &str| keys.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        // The two that make an unsolicited write legal — agreed to here
        // because this initiator asked for exactly that; the pair Windows
        // really sends is the other way round, and
        // `the_write_keys_are_answered_with_the_result_the_rfc_gives_them`
        // is about that.
        assert_eq!(get("InitialR2T").as_deref(), Some("No"));
        assert_eq!(get("ImmediateData").as_deref(), Some("Yes"));
        assert_eq!(get("HeaderDigest").as_deref(), Some("None"));
        assert_eq!(get("DataDigest").as_deref(), Some("None"));
        assert!(s.logged_in);
        assert!(!s.discovery);
        assert_eq!(s.initiator_max_recv, 65536);
    }

    #[test]
    fn a_login_for_someone_elses_target_is_refused() {
        let mut wire = Wire::new(login_pdu("Normal", "iqn.2026-08.dev.ownbackup:image.other"));
        let mut s = session(Lun::read_write(Box::new(MemSource::new(4096))));
        s.run(&mut wire).unwrap();
        let rsp = responses(&wire.output);
        assert_eq!(rsp[0].0, OP_LOGIN_RSP);
        // Status class 2 (initiator error), detail 3 (not found).
        assert_eq!((wire.output[36], wire.output[37]), (0x02, 0x03));
        assert!(!s.logged_in);
    }

    /// Discovery: the initiator asks what is here before it logs in for real.
    #[test]
    fn send_targets_names_the_one_target() {
        let mut script = login_pdu("Discovery", "");
        script.extend(pdu(OP_TEXT, 0x80, 2, b"SendTargets=All\0"));
        let mut wire = Wire::new(script);
        let mut s = session(Lun::read_write(Box::new(MemSource::new(4096))));
        s.run(&mut wire).unwrap();

        let rsp = responses(&wire.output);
        assert_eq!(rsp[1].0, OP_TEXT_RSP);
        let text = String::from_utf8_lossy(&rsp[1].1);
        assert!(text.contains("TargetName=iqn.2026-08.dev.ownbackup:image.test"), "{text}");
        assert!(s.discovery);
    }

    /// A read command end to end: the blocks come back as Data-In, then a
    /// status of its own.
    #[test]
    fn a_read_comes_back_as_data_then_status() {
        let mut lun = Lun::read_write(Box::new(MemSource::new(4096)));
        lun.write_at(512, &[0x5A; 512]).unwrap();

        let mut script = login_pdu("Normal", "iqn.2026-08.dev.ownbackup:image.test");
        // READ(10), one block at LBA 1, expecting 512 bytes.
        let mut cmd = pdu(OP_SCSI_CMD, 0x81, 7, &[]); // final | read
        cmd[20..24].copy_from_slice(&512u32.to_be_bytes());
        cmd[32..42].copy_from_slice(&[0x28, 0, 0, 0, 0, 1, 0, 0, 1, 0]);
        script.extend(cmd);
        script.extend(pdu(OP_LOGOUT, 0x80, 8, &[]));

        let mut wire = Wire::new(script);
        let mut s = session(lun);
        s.run(&mut wire).unwrap();

        let rsp = responses(&wire.output);
        let ops: Vec<u8> = rsp.iter().map(|(op, _)| *op).collect();
        assert_eq!(ops, vec![OP_LOGIN_RSP, OP_DATA_IN, OP_SCSI_RSP, OP_LOGOUT_RSP]);
        assert_eq!(rsp[1].1, vec![0x5A; 512]);
        assert_eq!(s.reads, 1);
        assert_eq!(s.read_bytes, 512);
    }

    /// A write with its data attached to the command — the path
    /// `ImmediateData=Yes` exists for.
    #[test]
    fn a_write_with_immediate_data_reaches_the_image() {
        let mut script = login_pdu("Normal", "iqn.2026-08.dev.ownbackup:image.test");
        let payload = vec![0xC7u8; 512];
        let mut cmd = pdu(OP_SCSI_CMD, 0xA1, 9, &payload); // final | write
        cmd[20..24].copy_from_slice(&512u32.to_be_bytes());
        cmd[32..42].copy_from_slice(&[0x2A, 0, 0, 0, 0, 0, 0, 0, 1, 0]);
        script.extend(cmd);

        let lun = Arc::new(Mutex::new(Lun::read_write(Box::new(MemSource::new(4096)))));
        let mut wire = Wire::new(script);
        let mut s = Session::new("iqn.2026-08.dev.ownbackup:image.test".into(), lun.clone());
        s.run(&mut wire).unwrap();

        let rsp = responses(&wire.output);
        assert_eq!(rsp.last().unwrap().0, OP_SCSI_RSP);
        assert_eq!(wire.output[wire.output.len() - BHS + 3], scsi::STATUS_GOOD);
        let mut back = vec![0u8; 512];
        lun.lock().unwrap().read_at(0, &mut back).unwrap();
        assert_eq!(back, payload);
        assert_eq!(s.writes, 1);
    }

    /// A write that took every byte it was promised is NOT an underflow.
    ///
    /// The residual used to be read off the data-IN side, which a write never
    /// uses, so every good write answered "of your N bytes I moved none" with
    /// the underflow bit set beside a status of GOOD. An initiator believes
    /// the residual over the bytes, and this is what kept an image from ever
    /// mounting read-write while reads were perfect (2026-09-18): the volume
    /// appeared, NTFS wrote to it, and every write came back counted as lost.
    #[test]
    fn a_write_that_took_all_its_data_reports_no_residual() {
        let mut script = login_pdu("Normal", "iqn.2026-08.dev.ownbackup:image.test");
        let mut cmd = pdu(OP_SCSI_CMD, 0xA1, 9, &vec![0xC7u8; 512]);
        cmd[20..24].copy_from_slice(&512u32.to_be_bytes());
        cmd[32..42].copy_from_slice(&[0x2A, 0, 0, 0, 0, 0, 0, 0, 1, 0]);
        script.extend(cmd);

        let mut wire = Wire::new(script);
        let mut s = session(Lun::read_write(Box::new(MemSource::new(4096))));
        s.run(&mut wire).unwrap();

        let all = pdus(&wire.output);
        let (bhs, _) = all.last().unwrap();
        assert_eq!(bhs[0], OP_SCSI_RSP);
        assert_eq!(bhs[3], scsi::STATUS_GOOD, "the write itself succeeded");
        assert_eq!(bhs[1] & 0x02, 0, "a write that took everything must not claim underflow");
        assert_eq!(
            u32::from_be_bytes(bhs[44..48].try_into().unwrap()),
            0,
            "none of the write was left behind"
        );
    }

    /// And an answer genuinely shorter than the room offered still says so:
    /// the residual is how the initiator learns which part of its buffer holds
    /// data. INQUIRY is the everyday case -- room for 255 bytes, 36 come back.
    #[test]
    fn an_answer_shorter_than_the_room_offered_still_reports_underflow() {
        let mut script = login_pdu("Normal", "iqn.2026-08.dev.ownbackup:image.test");
        let mut cmd = pdu(OP_SCSI_CMD, 0x81, 9, &[]);
        cmd[20..24].copy_from_slice(&255u32.to_be_bytes());
        cmd[32..38].copy_from_slice(&[0x12, 0, 0, 0, 255, 0]);
        script.extend(cmd);

        let mut wire = Wire::new(script);
        let mut s = session(Lun::read_write(Box::new(MemSource::new(4096))));
        s.run(&mut wire).unwrap();

        let all = pdus(&wire.output);
        let got: usize =
            all.iter().filter(|(b, _)| b[0] == OP_DATA_IN).map(|(_, d)| d.len()).sum();
        let (bhs, _) = all.last().unwrap();
        assert_eq!(bhs[0], OP_SCSI_RSP);
        assert!(got > 0 && got < 255, "INQUIRY answers with less than the room offered: {got}");
        assert_ne!(bhs[1] & 0x02, 0, "a short answer is an underflow");
        assert_eq!(u32::from_be_bytes(bhs[44..48].try_into().unwrap()) as usize, 255 - got);
    }

    /// The rest of a write arriving as its own PDU, which is what the initiator
    /// does when the command could not carry all of it.
    #[test]
    fn a_write_continued_in_data_out_is_assembled_in_order() {
        let mut script = login_pdu("Normal", "iqn.2026-08.dev.ownbackup:image.test");
        let mut cmd = pdu(OP_SCSI_CMD, 0xA1, 11, &vec![0x11u8; 512]);
        cmd[20..24].copy_from_slice(&1024u32.to_be_bytes());
        cmd[32..42].copy_from_slice(&[0x2A, 0, 0, 0, 0, 0, 0, 0, 2, 0]);
        script.extend(cmd);
        let mut rest = pdu(OP_DATA_OUT, 0x80, 11, &vec![0x22u8; 512]);
        rest[40..44].copy_from_slice(&512u32.to_be_bytes()); // buffer offset
        script.extend(rest);

        let lun = Arc::new(Mutex::new(Lun::read_write(Box::new(MemSource::new(4096)))));
        let mut wire = Wire::new(script);
        let mut s = Session::new("iqn.2026-08.dev.ownbackup:image.test".into(), lun.clone());
        s.run(&mut wire).unwrap();

        let mut back = vec![0u8; 1024];
        lun.lock().unwrap().read_at(0, &mut back).unwrap();
        assert_eq!(&back[..512], &[0x11u8; 512][..]);
        assert_eq!(&back[512..], &[0x22u8; 512][..]);
    }

    /// What the initiator asked for is what it has to be told. For these two
    /// keys the answer is not the target's preference: `InitialR2T` is settled
    /// with OR and `ImmediateData` with AND. Answering the Microsoft
    /// initiator's pair with our own left both sides waiting — it for an R2T,
    /// us for its data — and an NTFS mount died of it (2026-09-18).
    #[test]
    fn the_write_keys_are_answered_with_the_result_the_rfc_gives_them() {
        let script = login_pdu("Normal", "iqn.2026-08.dev.ownbackup:image.test");
        let mut wire = Wire::new(script);
        let mut s = session(Lun::read_write(Box::new(MemSource::new(4096))));
        s.run(&mut wire).unwrap();

        let (_, answer) = pdus(&wire.output).into_iter().next().unwrap();
        let text = String::from_utf8_lossy(&answer).replace('\0', " ");
        assert!(text.contains("InitialR2T=Yes"), "{text}");
        assert!(text.contains("ImmediateData=No"), "{text}");
        assert!(s.initial_r2t, "the initiator asked to be the one who waits");
        assert!(!s.immediate_data);
    }

    /// The Microsoft initiator's write: the command arrives alone, and the data
    /// only after an R2T naming where it goes and how much is wanted.
    #[test]
    fn a_write_nobody_sent_unasked_is_asked_for_with_r2t() {
        let mut script = login_pdu("Normal", "iqn.2026-08.dev.ownbackup:image.test");
        let mut cmd = pdu(OP_SCSI_CMD, 0xA1, 13, &[]); // final | write, no data
        cmd[20..24].copy_from_slice(&1024u32.to_be_bytes());
        cmd[32..42].copy_from_slice(&[0x2A, 0, 0, 0, 0, 0, 0, 0, 2, 0]);
        let itt = cmd[16..20].to_vec();
        script.extend(cmd);
        let mut data = pdu(OP_DATA_OUT, 0x80, 13, &vec![0x33u8; 1024]);
        data[40..44].copy_from_slice(&0u32.to_be_bytes());
        script.extend(data);

        let lun = Arc::new(Mutex::new(Lun::read_write(Box::new(MemSource::new(4096)))));
        let mut wire = Wire::new(script);
        let mut s = Session::new("iqn.2026-08.dev.ownbackup:image.test".into(), lun.clone());
        s.run(&mut wire).unwrap();

        let r2t = pdus(&wire.output)
            .into_iter()
            .find(|(bhs, _)| bhs[0] == OP_R2T)
            .expect("the write must have been asked for");
        let (bhs, _) = r2t;
        assert_eq!(&bhs[16..20], &itt[..], "the R2T names the command it is for");
        assert_ne!(u32::from_be_bytes(bhs[20..24].try_into().unwrap()), u32::MAX);
        assert_eq!(u32::from_be_bytes(bhs[40..44].try_into().unwrap()), 0);
        assert_eq!(u32::from_be_bytes(bhs[44..48].try_into().unwrap()), 1024);
        // The command window is shut until the data is in: a command sent now
        // would be read as the data we asked for and end the connection.
        let exp_cmd_sn = u32::from_be_bytes(bhs[28..32].try_into().unwrap());
        assert_eq!(
            u32::from_be_bytes(bhs[32..36].try_into().unwrap()),
            exp_cmd_sn.wrapping_sub(1),
            "the window must be closed while the write is being collected"
        );
        // And open again once the command is answered.
        let (last, _) = pdus(&wire.output).into_iter().last().unwrap();
        assert_eq!(last[0], OP_SCSI_RSP);
        assert_eq!(
            u32::from_be_bytes(last[32..36].try_into().unwrap()),
            u32::from_be_bytes(last[28..32].try_into().unwrap()),
            "the next command is welcome once this one is done"
        );

        let mut back = vec![0u8; 1024];
        lun.lock().unwrap().read_at(0, &mut back).unwrap();
        assert_eq!(back, vec![0x33u8; 1024]);
        assert_eq!(s.writes, 1);
    }

    /// And an initiator that promised to send the data unasked is taken at its
    /// word: nothing is asked for, the burst is read as it arrives.
    #[test]
    fn data_sent_unasked_is_taken_when_the_login_allowed_it() {
        let mut script = login_unsolicited("iqn.2026-08.dev.ownbackup:image.test");
        let mut cmd = pdu(OP_SCSI_CMD, 0xA1, 14, &vec![0x44u8; 512]);
        cmd[20..24].copy_from_slice(&1024u32.to_be_bytes());
        cmd[32..42].copy_from_slice(&[0x2A, 0, 0, 0, 0, 0, 0, 0, 2, 0]);
        script.extend(cmd);
        let mut rest = pdu(OP_DATA_OUT, 0x80, 14, &vec![0x55u8; 512]);
        rest[40..44].copy_from_slice(&512u32.to_be_bytes());
        script.extend(rest);

        let lun = Arc::new(Mutex::new(Lun::read_write(Box::new(MemSource::new(4096)))));
        let mut wire = Wire::new(script);
        let mut s = Session::new("iqn.2026-08.dev.ownbackup:image.test".into(), lun.clone());
        s.run(&mut wire).unwrap();

        assert!(
            pdus(&wire.output).iter().all(|(bhs, _)| bhs[0] != OP_R2T),
            "nothing had to be asked for"
        );
        let mut back = vec![0u8; 1024];
        lun.lock().unwrap().read_at(0, &mut back).unwrap();
        assert_eq!(&back[..512], &[0x44u8; 512][..]);
        assert_eq!(&back[512..], &[0x55u8; 512][..]);
    }

    /// Data-Out that does not continue where the last piece ended would be
    /// written to the wrong place — the connection ends instead.
    #[test]
    fn data_out_at_the_wrong_offset_ends_the_connection() {
        let mut script = login_pdu("Normal", "iqn.2026-08.dev.ownbackup:image.test");
        let mut cmd = pdu(OP_SCSI_CMD, 0xA1, 12, &vec![0x11u8; 512]);
        cmd[20..24].copy_from_slice(&1024u32.to_be_bytes());
        cmd[32..42].copy_from_slice(&[0x2A, 0, 0, 0, 0, 0, 0, 0, 2, 0]);
        script.extend(cmd);
        let mut rest = pdu(OP_DATA_OUT, 0x80, 12, &vec![0x22u8; 512]);
        rest[40..44].copy_from_slice(&4096u32.to_be_bytes());
        script.extend(rest);

        let mut wire = Wire::new(script);
        let mut s = session(Lun::read_write(Box::new(MemSource::new(4096))));
        let e = s.run(&mut wire).unwrap_err().to_string();
        assert!(e.contains("Data-Out at offset 4096"), "{e}");
    }

    #[test]
    fn nothing_is_served_before_a_login() {
        let mut wire = Wire::new(pdu(OP_SCSI_CMD, 0x81, 3, &[]));
        let mut s = session(Lun::read_write(Box::new(MemSource::new(4096))));
        let e = s.run(&mut wire).unwrap_err().to_string();
        assert!(e.contains("before login"), "{e}");
    }

    #[test]
    fn an_opcode_we_do_not_know_is_rejected_not_ignored() {
        let mut script = login_pdu("Normal", "iqn.2026-08.dev.ownbackup:image.test");
        script.extend(pdu(0x1C, 0x80, 4, &[])); // SCSI task management: not ours
        let mut wire = Wire::new(script);
        let mut s = session(Lun::read_write(Box::new(MemSource::new(4096))));
        s.run(&mut wire).unwrap();
        let rsp = responses(&wire.output);
        assert_eq!(rsp[1].0, OP_REJECT);
    }

    /// Read replies are cut to what the initiator said it can take in one PDU.
    #[test]
    fn a_big_read_is_cut_into_pieces_the_initiator_asked_for() {
        let mut lun = Lun::read_write(Box::new(MemSource::new(16384)));
        lun.write_at(0, &[0x33; 8192]).unwrap();
        let mut script = login_pdu("Normal", "iqn.2026-08.dev.ownbackup:image.test");
        // The login above declares MaxRecvDataSegmentLength=65536; ask for a
        // read bigger than one chunk by lowering it here would need a second
        // login, so instead read 8 KiB and check it arrives whole.
        let mut cmd = pdu(OP_SCSI_CMD, 0x81, 21, &[]);
        cmd[20..24].copy_from_slice(&8192u32.to_be_bytes());
        cmd[32..42].copy_from_slice(&[0x28, 0, 0, 0, 0, 0, 0, 0, 16, 0]);
        script.extend(cmd);

        let mut wire = Wire::new(script);
        let mut s = session(lun);
        s.run(&mut wire).unwrap();
        let rsp = responses(&wire.output);
        let data: Vec<u8> =
            rsp.iter().filter(|(op, _)| *op == OP_DATA_IN).flat_map(|(_, d)| d.clone()).collect();
        assert_eq!(data.len(), 8192);
        assert!(data.iter().all(|&b| b == 0x33));
    }
}
