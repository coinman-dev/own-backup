//! Disposable WinPE acceptance using the production CLI and portable recovery key.
#[cfg(windows)]
mod probe {
    use own_backup_backup::win_device::WinRawDevice;
    use sha2::{Digest, Sha256};
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn say(message: &str) {
        println!("{message}");
        for path in [r"X:\pe-recovery.log", r"\\.\COM1"] {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{message}");
            }
        }
    }

    fn check(ok: bool, message: &str) -> Result<()> {
        if !ok {
            return Err(message.into());
        }
        say(&format!("PE-OK {message}"));
        Ok(())
    }

    fn unique_root(marker: &str) -> Result<PathBuf> {
        let roots: Vec<_> = (b'C'..=b'Z')
            .map(|letter| PathBuf::from(format!("{}:\\", letter as char)))
            .filter(|root| root.join(marker).exists())
            .collect();
        check(roots.len() == 1, "one fixture volume matches the marker")?;
        Ok(roots[0].clone())
    }

    fn cli(name: &str, args: &[&str], key: &str) -> Result<(bool, String)> {
        let output = format!(r"X:\pe-{name}.out");
        let errors = format!(r"X:\pe-{name}.err");
        let mut child = Command::new(r"X:\Program Files\OwnBackup\own-backup-cli.exe")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(File::create(&output)?)
            .stderr(File::create(&errors)?)
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or("no CLI stdin")?
            .write_all(format!("\n{key}\n").as_bytes())?;
        let deadline = Instant::now() + Duration::from_secs(180);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                return Err(format!("CLI {name} timed out").into());
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let stdout = fs::read_to_string(output)?;
        let stderr = fs::read_to_string(errors)?;
        check(
            !stdout.contains(key) && !stderr.contains(key),
            "CLI output does not echo the supplied secret",
        )?;
        say(&format!("PE-CLI name={name} exit={:?}", status.code()));
        Ok((status.success(), stdout))
    }

    fn run() -> Result<()> {
        say("PE-START");
        check(
            own_backup_common::media_mode::is_windows_pe(),
            "running in real Windows PE",
        )?;
        let source = unique_root(r"Lab\acceptance\stand-mixed-recovery.txt")?;
        let archive = unique_root(r"stand\mixed")?.join(r"stand\mixed");
        let mut images: Vec<_> = fs::read_dir(archive)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.file_name().to_string_lossy().contains("_inc_")
                    && entry.path().extension().is_some_and(|ext| ext == "obk")
            })
            .collect();
        images.sort_by_key(|entry| entry.metadata().and_then(|meta| meta.modified()).ok());
        let image = images
            .last()
            .ok_or("no encrypted incremental point")?
            .path();
        let image = image.to_str().ok_or("archive path is not UTF-8")?;
        let key = fs::read_to_string(source.join(r"Lab\acceptance\stand-mixed-recovery.txt"))?;
        let key = key.trim();
        check(
            key.len() > 32 && key.len() < 200,
            "portable fixture key has a bounded size",
        )?;
        let (success, _) = cli("wrong-key", &["verify", image], "obkr1invalid-lab-key")?;
        check(!success, "wrong recovery key is refused")?;
        let (success, stdout) = cli("verify", &["verify", image], key)?;
        check(
            success && stdout.contains("Result: VALID"),
            "complete encrypted incremental version verifies using only recovery key",
        )?;
        let mut targets = Vec::new();
        for number in 0..8 {
            if let Ok(mut disk) = WinRawDevice::open_read(&format!(r"\\.\PhysicalDrive{number}")) {
                say(&format!("PE-DISK number={number} bytes={}", disk.length()));
                if disk.length() == 2 << 30 {
                    let mut first = vec![0; 1024 * 1024];
                    disk.read_at(0, &mut first)?;
                    // WinPE assigns an MBR disk signature even to a disk with
                    // no partitions. Only those four bytes may be nonzero.
                    first[440..444].fill(0);
                    if first.iter().all(|byte| *byte == 0) {
                        targets.push(number);
                    }
                }
            }
        }
        check(
            targets.len() == 1,
            "exactly one blank disposable two-GiB disk exists",
        )?;
        check(
            !Path::new(r"W:\").exists(),
            "W is unused before target preparation",
        )?;
        fs::write(r"X:\pe-diskpart.txt", format!("select disk {}\r\nonline disk noerr\r\nattributes disk clear readonly\r\nconvert gpt\r\ncreate partition primary\r\nformat fs=ntfs quick label=PETARGET\r\nassign letter=W\r\n", targets[0]))?;
        let prepared = Command::new("diskpart.exe")
            .args(["/s", r"X:\pe-diskpart.txt"])
            .output()?;
        fs::write(
            r"X:\pe-diskpart.out",
            [&prepared.stdout[..], &prepared.stderr[..]].concat(),
        )?;
        check(
            prepared.status.success() && Path::new(r"W:\").exists(),
            "blank restore target has been prepared",
        )?;
        let (success, _) = cli(
            "restore",
            &["restore", image, "--target", "W:", "--yes"],
            key,
        )?;
        check(
            success,
            "encrypted incremental volume restored by portable recovery key",
        )?;
        let deadline = Instant::now() + Duration::from_secs(50);
        while !Path::new(r"W:\control.bin").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_secs(1));
        }
        let actual = format!("{:x}", Sha256::digest(fs::read(r"W:\control.bin")?));
        let expected: serde_json::Value = serde_json::from_slice(&fs::read(
            source.join(r"Lab\acceptance\mixed-restored-content.json"),
        )?)?;
        check(
            expected["sha256"]
                .as_str()
                .is_some_and(|hash| hash.eq_ignore_ascii_case(&actual)),
            "independent SHA-256 matches the source control file",
        )?;
        check(
            !Path::new(r"W:\omit.bin").exists() && !Path::new(r"W:\omit-tiny.txt").exists(),
            "excluded files remain absent",
        )?;
        say(&format!(
            "PE-RESULT PASS control_sha256={actual} target_disk={}",
            targets[0]
        ));
        Ok(())
    }

    pub fn main() {
        if let Err(error) = run() {
            say(&format!("PE-RESULT FAIL {error}"));
        }
        if let Ok(root) = unique_root("pe-output.marker") {
            if let Ok(entries) = fs::read_dir(r"X:\") {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with("pe-") && entry.path().is_file() {
                        let _ = fs::copy(entry.path(), root.join(name));
                    }
                }
            }
        }
        let _ = Command::new("wpeutil.exe").arg("shutdown").status();
    }
}

fn main() {
    #[cfg(windows)]
    probe::main();
}
