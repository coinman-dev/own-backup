//! A04 acceptance in an isolated Setup PE: CHAP, discovery and real NTFS reads.
//! The VM has two CDs and a disposable FAT USB drive for evidence only.
#[cfg(target_os = "windows")]
mod probe {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use own_backup_backup::qcow2::Qcow2RandomReader;
    use own_backup_backup::seal::context::ArchiveKeys;
    use own_backup_backup::{engine_win, iscsi, iscsi_win};
    use sha2::{Digest, Sha256};
    use zeroize::Zeroizing;

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn say(message: impl AsRef<str>) {
        let message = message.as_ref();
        println!("{message}");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(r"X:\audit-a04.log")
        {
            let _ = writeln!(file, "{message}");
        }
        if let Ok(mut port) = std::fs::OpenOptions::new().write(true).open(r"\\.\COM1") {
            let _ = writeln!(port, "{message}");
        }
    }

    fn command(program: &str, args: &[&str]) -> Result<()> {
        let output = std::process::Command::new(program).args(args).output()?;
        say(format!(
            "WINPE-A04-COMMAND program={program} args={args:?} exit={}",
            output.status
        ));
        // Setup PE's console output is OEM text; preserve its bytes in the RAM disk.
        let log = PathBuf::from(r"X:\").join(format!("a04-{program}.out"));
        std::fs::write(log, [&output.stdout[..], &output.stderr[..]].concat())?;
        Ok(())
    }

    fn register_test_initiator() -> Result<()> {
        use windows::core::PCWSTR;
        use windows::Win32::System::Services::{
            CloseServiceHandle, CreateServiceW, OpenSCManagerW, SC_MANAGER_CREATE_SERVICE,
            SERVICE_DEMAND_START, SERVICE_ERROR_NORMAL, SERVICE_QUERY_STATUS,
            SERVICE_WIN32_SHARE_PROCESS,
        };

        // These keys belong to this disposable PE RAM disk. A dedicated svchost
        // group avoids replacing PE's existing netsvcs group.
        command(
            "reg.exe",
            &[
                "add",
                r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Svchost",
                "/v",
                "A04Iscsi",
                "/t",
                "REG_MULTI_SZ",
                "/d",
                "MSiSCSI",
                "/f",
            ],
        )?;
        let name: Vec<u16> = "MSiSCSI".encode_utf16().chain(Some(0)).collect();
        let binary: Vec<u16> = r"%SystemRoot%\System32\svchost.exe -k A04Iscsi"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: local SCM; all strings are terminated and live through the call.
        let scm =
            unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CREATE_SERVICE) }?;
        let service = unsafe {
            CreateServiceW(
                scm,
                PCWSTR(name.as_ptr()),
                PCWSTR(name.as_ptr()),
                SERVICE_QUERY_STATUS,
                SERVICE_WIN32_SHARE_PROCESS,
                SERVICE_DEMAND_START,
                SERVICE_ERROR_NORMAL,
                PCWSTR(binary.as_ptr()),
                PCWSTR::null(),
                None,
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
            )
        };
        let _ = unsafe { CloseServiceHandle(scm) };
        let service = service?;
        let _ = unsafe { CloseServiceHandle(service) };
        command(
            "reg.exe",
            &[
                "add",
                r"HKLM\SYSTEM\CurrentControlSet\Services\MSiSCSI\Parameters",
                "/v",
                "ServiceDll",
                "/t",
                "REG_EXPAND_SZ",
                "/d",
                r"%SystemRoot%\System32\iscsiexe.dll",
                "/f",
            ],
        )?;
        say("WINPE-A04-TEST-REGISTRATION MSiSCSI=true svchost_group=A04Iscsi");
        Ok(())
    }

    fn session_count() -> Result<u32> {
        use windows::Win32::Storage::IscsiDisc::GetIScsiSessionListW;
        let mut size = 0;
        let mut count = 0;
        // SAFETY: a null buffer and zero byte count request the required size.
        let code = unsafe { GetIScsiSessionListW(&mut size, &mut count, std::ptr::null_mut()) };
        if code != 0 && code != 122 {
            return Err(format!("GetIScsiSessionListW failed: {code:#x}").into());
        }
        Ok(count)
    }

    fn empty_sessions(stage: &str) -> Result<()> {
        let count = session_count()?;
        say(format!("WINPE-A04-SESSIONS stage={stage} count={count}"));
        if count != 0 {
            return Err(format!("orphaned session at {stage}").into());
        }
        Ok(())
    }

    fn media(marker: &str) -> Result<PathBuf> {
        (b'C'..=b'Z')
            .map(|l| PathBuf::from(format!("{}:\\", l as char)))
            .find(|p| p.join(marker).exists())
            .ok_or_else(|| format!("missing media {marker}").into())
    }

    fn pdu(stream: &mut TcpStream, opcode: u8, flags: u8, tag: u32, data: &[u8]) -> Result<()> {
        let mut header = [0u8; 48];
        header[0] = opcode;
        header[1] = flags;
        header[5..8].copy_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
        header[8..14].copy_from_slice(&[0x80, 0, 0, 1, 2, 3]);
        header[16..20].copy_from_slice(&tag.to_be_bytes());
        header[24..28].copy_from_slice(&1u32.to_be_bytes());
        stream.write_all(&header)?;
        stream.write_all(data)?;
        stream.write_all(&[0u8; 3][..(4 - data.len() % 4) % 4])?;
        Ok(())
    }

    fn reply(stream: &mut TcpStream) -> Result<([u8; 48], Vec<u8>)> {
        let mut header = [0; 48];
        stream.read_exact(&mut header)?;
        let size = u32::from_be_bytes([0, header[5], header[6], header[7]]) as usize;
        if size > 256 * 1024 {
            return Err("oversized target reply".into());
        }
        let mut data = vec![0; size + (4 - size % 4) % 4];
        stream.read_exact(&mut data)?;
        data.truncate(size);
        Ok((header, data))
    }

    fn socket(port: u16) -> Result<TcpStream> {
        let socket = TcpStream::connect(("127.0.0.1", port))?;
        socket.set_read_timeout(Some(Duration::from_secs(10)))?;
        socket.set_write_timeout(Some(Duration::from_secs(10)))?;
        Ok(socket)
    }

    fn wire_checks(port: u16, iqn: &str) -> Result<()> {
        let mut no_chap = socket(port)?;
        let keys = format!("InitiatorName=iqn.2026-10.dev.ownbackup:a04\0SessionType=Normal\0TargetName={iqn}\0AuthMethod=None\0");
        pdu(&mut no_chap, 0x43, 0x83, 1, keys.as_bytes())?;
        let (header, _) = reply(&mut no_chap)?;
        if header[0] != 0x23 || header[36..38] != [2, 1] {
            return Err("target did not reject missing CHAP with authentication failure".into());
        }
        drop(no_chap);
        say("WINPE-A04-NO-CHAP refused=true status=2/1");

        let mut discovery = socket(port)?;
        pdu(&mut discovery, 0x43, 0x83, 1,
            b"InitiatorName=iqn.2026-10.dev.ownbackup:a04\0SessionType=Discovery\0AuthMethod=None\0")?;
        let (header, _) = reply(&mut discovery)?;
        if header[0] != 0x23 || header[36..38] != [0, 0] || header[1] & 0x83 != 0x83 {
            return Err("discovery login failed".into());
        }
        pdu(&mut discovery, 0x44, 0x80, 2, b"SendTargets=All\0")?;
        let (header, data) = reply(&mut discovery)?;
        if header[0] != 0x24 || !data.windows(iqn.len()).any(|w| w == iqn.as_bytes()) {
            return Err("discovery did not list the test target".into());
        }
        let mut read = [0u8; 48];
        read[0] = 0x01;
        read[1] = 0xc1; // final, read, simple task
        read[16..20].copy_from_slice(&3u32.to_be_bytes());
        read[20..24].copy_from_slice(&512u32.to_be_bytes());
        read[24..28].copy_from_slice(&2u32.to_be_bytes());
        read[32..42].copy_from_slice(&[0x28, 0, 0, 0, 0, 0, 0, 0, 1, 0]);
        discovery.write_all(&read)?;
        let mut unexpected = [0u8; 1];
        // A timeout is a FAIL: the target must actually close the connection.
        if discovery.read(&mut unexpected)? != 0 {
            return Err("discovery READ received target data".into());
        }
        say("WINPE-A04-DISCOVERY listed=true read_refused=true bytes_returned=0");
        Ok(())
    }

    fn chap_checks(path: &Path) -> Result<()> {
        use own_backup_backup::win_device::WinRawDevice;
        let source = Qcow2RandomReader::open(path)?;
        let size = source.virtual_size();
        let serial = iscsi::serial_for_image(path);
        let iqn = iscsi::iqn_for(&serial);
        let listener = std::net::TcpListener::bind(("127.0.0.1", 3260))?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_iqn = iqn.clone();
        let worker = std::thread::spawn(move || {
            iscsi::serve_listener_chap(
                listener,
                worker_iqn,
                Arc::new(Mutex::new(
                    iscsi::Lun::read_only(Box::new(source)).with_serial(&serial),
                )),
                worker_stop,
                Arc::new(Zeroizing::new(*b"0123456789abcdef")),
            )
        });
        let result = (|| -> Result<()> {
            wire_checks(3260, &iqn)?;
            match iscsi_win::login("127.0.0.1", 3260, &iqn, b"wrong-wrong-1234", size) {
                Ok(attached) => {
                    attached.logout();
                    return Err("native initiator accepted wrong CHAP".into());
                }
                Err(error) => say(format!("WINPE-A04-WRONG-CHAP refused=true error={error}")),
            }
            empty_sessions("after-wrong-chap")?;
            let attached = iscsi_win::login("127.0.0.1", 3260, &iqn, b"0123456789abcdef", size)?;
            let disk = attached.disk_number;
            let read = (|| -> Result<bool> {
                if session_count()? != 1 {
                    return Err("native login did not create exactly one session".into());
                }
                let mut bytes = [0; 512];
                WinRawDevice::open_read(&format!(r"\\.\PhysicalDrive{disk}"))?
                    .read_at(0, &mut bytes)?;
                Ok(bytes.starts_with(b"OBK-MANUAL-CHAP-CONTROL"))
            })();
            let logout = attached.logout();
            let correct = read?;
            say(format!(
                "WINPE-A04-CORRECT-CHAP disk={disk} same_bytes={correct} logout={logout}"
            ));
            if !correct || !logout {
                return Err("native read/logout failed".into());
            }
            empty_sessions("after-correct-chap")
        })();
        stop.store(true, Ordering::SeqCst);
        worker.join().map_err(|_| "target worker panicked")??;
        result
    }

    fn file_checks(image: &Path) -> Result<()> {
        let keys = ArchiveKeys::from_image_password(image, "лабораторный пароль архива")?;
        let physical = engine_win::mount_image_with_keys(image, false, Some(&keys))?;
        let result = (|| -> Result<()> {
            if session_count()? != 1 {
                return Err("OBK mount did not create exactly one session".into());
            }
            let disk: u32 = physical
                .rsplit("PhysicalDrive")
                .next()
                .ok_or("missing disk number")?
                .parse()?;
            let letters = own_backup_backup::volume_win::drive_letters_on_disk(disk);
            say(format!(
                "WINPE-A04-OBK-MOUNT physical={physical} letters={letters:?}"
            ));
            let root = letters
                .iter()
                .map(PathBuf::from)
                .find(|p| p.join("keep-1.bin").exists())
                .ok_or("mounted archive has no control NTFS volume")?;
            // Independent source hashes preserved by H03's host-side NTFS extraction.
            for (name, expected) in [
                (
                    "keep-1.bin",
                    "6acfe96bb9801018a55e414171ce872610c9331727313723648c07734e69396a",
                ),
                (
                    r"docs\keep-2.bin",
                    "d8a5f91ac637fc1560efbfe98dc3787b660312b68060eac7c986816599eefd10",
                ),
                (
                    "gen-1.bin",
                    "9f5e78e53b07e50280c6036000671199304af70555b1388cf183bc683d3bbbb1",
                ),
                (
                    "gen-2.bin",
                    "cb4ddc09dd1cec36b671e302794e6c7ffb12060319a2b0ae92d279843857a662",
                ),
            ] {
                let bytes = std::fs::read(root.join(name))?;
                let hash = format!("{:x}", Sha256::digest(&bytes));
                say(format!(
                    "WINPE-A04-FILE name={name} bytes={} sha256={hash} match={}",
                    bytes.len(),
                    hash == expected
                ));
                if hash != expected {
                    return Err(format!("control file differs: {name}").into());
                }
            }
            if root.join("excluded-secret.bin").exists() {
                return Err("excluded file returned".into());
            }
            Ok(())
        })();
        let unmount = engine_win::unmount_image(image);
        result?;
        unmount?;
        empty_sessions("after-obk-unmount")?;
        say("WINPE-A04-OBK-FILES matched=4 excluded_absent=true unmounted=true");
        Ok(())
    }

    pub fn run() -> Result<()> {
        say("WINPE-A04-BEGIN");
        if !own_backup_common::media_mode::is_windows_pe() {
            return Err("not WinPE".into());
        }
        command("wpeinit.exe", &[])?;
        command("wpeutil.exe", &["disablefirewall"])?;
        let before = iscsi_win::status();
        say(format!(
            "WINPE-A04-INITIATOR before driver={} present={} running={} ready={} message={}",
            before.driver_file,
            before.service_present,
            before.running,
            before.ready,
            before.message
        ));
        if !before.service_present && std::env::args().any(|arg| arg == "--register-initiator") {
            register_test_initiator()?;
        }
        command("net.exe", &["stop", "eventlog"])?;
        command(
            "reg.exe",
            &[
                "delete",
                r"HKLM\SYSTEM\CurrentControlSet\Control\MiniNT",
                "/f",
            ],
        )?;
        let eventlog = command("net.exe", &["start", "eventlog"]);
        let initiator = command("net.exe", &["start", "msiscsi"]);
        let restored = command(
            "reg.exe",
            &["add", r"HKLM\SYSTEM\CurrentControlSet\Control\MiniNT", "/f"],
        );
        restored?;
        eventlog?;
        initiator?;
        let after = iscsi_win::status();
        say(format!(
            "WINPE-A04-INITIATOR after driver={} present={} running={} ready={} message={}",
            after.driver_file, after.service_present, after.running, after.ready, after.message
        ));
        iscsi_win::ensure_initiator_service()?;
        empty_sessions("before")?;
        chap_checks(&media("a04-media.marker")?.join("manual-probe.qcow2"))?;
        file_checks(&media("h03-media.marker")?.join("lab_disk0_full_1.obk"))?;
        say("WINPE-A04-PASS");
        Ok(())
    }

    pub fn main() {
        let result = run();
        if let Err(ref error) = result {
            say(format!("WINPE-A04-FAIL {error}"));
        }
        // Preserve a visible console for the host's screenshot; shutdown is host driven.
        say("WINPE-A04-DONE");
        if let Ok(root) = media("a04-output.marker") {
            if let Ok(entries) = std::fs::read_dir(r"X:\") {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with("a04-") || name == "audit-a04.log" {
                        let destination = root.join(&name);
                        if std::fs::copy(entry.path(), &destination).is_ok() {
                            if let Ok(file) =
                                std::fs::OpenOptions::new().write(true).open(destination)
                            {
                                let _ = file.sync_all();
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(10));
            let _ = command("wpeutil.exe", &["shutdown"]);
        }
        if result.is_err() {
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "windows")]
fn main() {
    probe::main();
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("audit_winpe_a04 is Windows-only");
}
