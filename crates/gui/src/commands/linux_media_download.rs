//! The ready Linux boot medium from GitHub instead of a build. The release
//! of the installed package (`OwnBackup-Linux-<version>_amd64.iso`, see
//! `media_linux::release_asset_name`) is looked up and downloaded here, in
//! the person's own process, straight into their folder: no root, no
//! password, nothing for the service to do. `curl` (a dependency of the
//! package) does the HTTPS, with the system's certificates.
//!
//! Windows, which cannot build that medium, downloads it the same way: the
//! medium of this program's version (the package revision is unknown here,
//! so the highest one the release has), with Windows' own `curl.exe`.
//!
//! The file is written under a hidden temporary name beside the result,
//! checked against the size GitHub lists and the release's SHA256SUMS, and
//! only then given its name — never replacing a file: an existing ISO keeps
//! its name and the new one gets ` (2)`, as with a build. A cancel or a
//! failure leaves nothing in the folder.

use serde::Serialize;

/// What the window offers before downloading.
#[derive(Debug, Clone, Serialize)]
pub struct LinuxMediaReleaseDto {
    /// The installed package's version, which the medium carries.
    pub version: String,
    pub tag: String,
    pub size: u64,
}

/// What was downloaded.
#[derive(Debug, Clone, Serialize)]
pub struct LinuxMediaDownloadDto {
    pub iso_path: String,
    pub iso_bytes: u64,
    pub version: String,
    pub tag: String,
}

/// The `linux-media-download` window event: `stage` is `download` (with
/// `received` of `total` bytes) or `verify`.
#[derive(Debug, Clone, Serialize)]
pub struct LinuxMediaDownloadProgress {
    pub stage: &'static str,
    pub received: u64,
    pub total: u64,
}

/// The release that has the installed version's medium; `None` when no
/// release has it (or the repository cannot be seen).
#[tauri::command]
pub async fn linux_media_release() -> Result<Option<LinuxMediaReleaseDto>, String> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        tokio::task::spawn_blocking(|| {
            imp::find().map(|found| {
                found.map(|(version, r)| LinuxMediaReleaseDto {
                    version,
                    tag: r.tag,
                    size: r.size,
                })
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(own_backup_common::tr!("linux_media_dl.linux_only").into_owned())
    }
}

/// Download the installed version's medium into `output_dir`; progress on
/// the `linux-media-download` event.
#[tauri::command]
pub async fn download_linux_media(
    app: tauri::AppHandle,
    output_dir: String,
) -> Result<LinuxMediaDownloadDto, String> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        tokio::task::spawn_blocking(move || imp::download(&app, &output_dir))
            .await
            .map_err(|e| e.to_string())?
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (app, output_dir);
        Err(own_backup_common::tr!("linux_media_dl.linux_only").into_owned())
    }
}

/// Stop the download that is running; it ends with the cancel error and
/// leaves nothing behind.
#[tauri::command]
pub fn cancel_linux_media_download() {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    imp::CANCEL.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod imp {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use own_backup_backup::media_linux::{
        github_releases_url, iso_file_name, sha256_listed, ReleaseIso,
    };
    use own_backup_common::tr;
    use sha2::{Digest, Sha256};
    use tauri::Emitter;

    use super::{LinuxMediaDownloadDto, LinuxMediaDownloadProgress};

    pub static CANCEL: AtomicBool = AtomicBool::new(false);

    // Messages whose Linux text ends by suggesting a build of the medium,
    // which Windows cannot do: there their `_win` text, without that advice.

    fn rate_limited() -> String {
        if cfg!(target_os = "windows") {
            tr!("linux_media_dl.rate_limited_win").into_owned()
        } else {
            tr!("linux_media_dl.rate_limited").into_owned()
        }
    }

    fn bad_answer() -> String {
        if cfg!(target_os = "windows") {
            tr!("linux_media_dl.bad_answer_win").into_owned()
        } else {
            tr!("linux_media_dl.bad_answer").into_owned()
        }
    }

    fn none_released() -> String {
        if cfg!(target_os = "windows") {
            tr!("linux_media_dl.none_win").into_owned()
        } else {
            tr!("linux_media_dl.none").into_owned()
        }
    }

    fn no_sums(tag: &str) -> String {
        if cfg!(target_os = "windows") {
            tr!("linux_media_dl.no_sums_win", tag = tag).into_owned()
        } else {
            tr!("linux_media_dl.no_sums", tag = tag).into_owned()
        }
    }

    /// GitHub's list of releases is a few hundred KiB; the checksum list a
    /// few lines.
    const MAX_TEXT: &str = "16M";

    /// curl with what every call here needs: HTTPS only (redirects too —
    /// assets are served from another host), failing on an HTTP error, the
    /// C locale for the messages read below.
    fn curl() -> Command {
        #[cfg(target_os = "linux")]
        let mut c = Command::new("curl");
        // Windows' own, by its full path: a `curl.exe` in the current folder
        // or on PATH is not run in its place. No console window; Schannel
        // checks revocation only as far as it can, the way the browser does —
        // a revocation server it cannot reach is not a failed download.
        #[cfg(target_os = "windows")]
        let mut c = {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
            let mut c = Command::new(Path::new(&root).join("System32").join("curl.exe"));
            c.creation_flags(CREATE_NO_WINDOW)
                .arg("--ssl-revoke-best-effort");
            c
        };
        c.env("LC_ALL", "C")
            .args([
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--connect-timeout",
                "20",
                "--user-agent",
            ])
            .arg(format!("own-backup/{}", env!("CARGO_PKG_VERSION")))
            .stdin(Stdio::null());
        c
    }

    enum Fetch {
        NotFound,
        Failed(String),
    }

    fn not_started(e: std::io::Error) -> String {
        if e.kind() == std::io::ErrorKind::NotFound {
            if cfg!(target_os = "windows") {
                tr!("linux_media_dl.no_curl_win").into_owned()
            } else {
                tr!("linux_media_dl.no_curl").into_owned()
            }
        } else {
            tr!("linux_media_dl.no_curl_start", error = e).into_owned()
        }
    }

    /// Why curl failed, by its exit code (and, for an HTTP error, the status
    /// in its untranslated message).
    fn failure(code: Option<i32>, stderr: &str) -> Fetch {
        match code {
            Some(22) if stderr.contains(" 404") => Fetch::NotFound,
            Some(22) if stderr.contains(" 403") || stderr.contains(" 429") => {
                Fetch::Failed(rate_limited())
            }
            Some(5 | 6 | 7 | 28 | 35 | 52 | 55 | 56) => {
                Fetch::Failed(tr!("linux_media_dl.no_network").into_owned())
            }
            _ => Fetch::Failed(tr!("linux_media_dl.failed", error = stderr.trim()).into_owned()),
        }
    }

    fn fetch_text(url: &str, accept: &str) -> Result<String, Fetch> {
        let out = curl()
            .args(["--max-time", "60", "--max-filesize", MAX_TEXT, "-H"])
            .arg(format!("Accept: {accept}"))
            .arg(url)
            .output()
            .map_err(|e| Fetch::Failed(not_started(e)))?;
        if !out.status.success() {
            return Err(failure(
                out.status.code(),
                &String::from_utf8_lossy(&out.stderr),
            ));
        }
        String::from_utf8(out.stdout).map_err(|_| Fetch::Failed(bad_answer()))
    }

    /// The installed version and the release with its medium.
    /// On Windows the version is this program's, in Debian's spelling
    /// (`0.1.6-beta` → `0.1.6~beta`), and the revision comes from the release.
    pub fn find() -> Result<Option<(String, ReleaseIso)>, String> {
        #[cfg(target_os = "linux")]
        let version = {
            use own_backup_backup::media_linux_build::{installed, PACKAGE};
            installed(Path::new("/"), PACKAGE)
                .map_err(|_| tr!("linux_media_dl.not_installed").into_owned())?
                .version
        };
        let text = match fetch_text(&github_releases_url(), "application/vnd.github+json") {
            Ok(t) => t,
            // A private repository answers 404 too: nothing to download.
            Err(Fetch::NotFound) => return Ok(None),
            Err(Fetch::Failed(m)) => return Err(m),
        };
        let releases: serde_json::Value = serde_json::from_str(&text).map_err(|_| bad_answer())?;
        #[cfg(target_os = "linux")]
        let found = own_backup_backup::media_linux::find_release_iso(&releases, &version)
            .map(|r| (version, r));
        #[cfg(target_os = "windows")]
        let found = own_backup_backup::media_linux::find_release_iso_of_upstream(
            &releases,
            &own_backup_backup::media_linux::deb_upstream(env!("CARGO_PKG_VERSION")),
        );
        Ok(found)
    }

    /// Removes the temporary file unless it was given its name.
    struct Part(Option<PathBuf>);

    impl Drop for Part {
        fn drop(&mut self) {
            if let Some(p) = self.0.take() {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    /// The expected SHA-256 of the ISO from whichever of the release's
    /// checksum lists names it.
    fn expected_sha256(release: &ReleaseIso) -> Result<String, String> {
        let mut failed = None;
        for url in &release.sums_urls {
            match fetch_text(url, "application/octet-stream") {
                Ok(sums) => {
                    if let Some(hash) = sha256_listed(&sums, &release.name) {
                        return Ok(hash);
                    }
                }
                Err(Fetch::NotFound) => {}
                Err(Fetch::Failed(m)) => failed = Some(m),
            }
        }
        Err(failed.unwrap_or_else(|| no_sums(&release.tag)))
    }

    /// Hidden temporary files an earlier download left in `dir` (the window
    /// was closed by force or crashed mid-download): only ours, by name.
    fn sweep_parts(dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.as_encoded_bytes();
            if name.starts_with(b".OwnBackup-Linux-")
                && name.ends_with(b".part")
                && e.file_type().is_ok_and(|t| t.is_file())
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    fn sha256_file(path: &Path) -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => hasher.update(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
            if CANCEL.load(Ordering::SeqCst) {
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
        }
        Ok(hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }

    pub fn download(
        app: &tauri::AppHandle,
        output_dir: &str,
    ) -> Result<LinuxMediaDownloadDto, String> {
        CANCEL.store(false, Ordering::SeqCst);
        let cancelled = || tr!("linux_media_dl.cancelled").into_owned();
        let dir = PathBuf::from(output_dir.trim());
        if !dir.is_absolute() || !dir.is_dir() {
            return Err(tr!("linux_media_dl.no_folder", path = output_dir).into_owned());
        }
        let (version, release) = find()?.ok_or_else(none_released)?;
        // The checksum first: a release that cannot be checked is refused
        // before a gigabyte is downloaded.
        let expected = expected_sha256(&release)?;
        if CANCEL.load(Ordering::SeqCst) {
            return Err(cancelled());
        }
        let emit = |stage: &'static str, received: u64| {
            let _ = app.emit(
                "linux-media-download",
                LinuxMediaDownloadProgress {
                    stage,
                    received,
                    total: release.size,
                },
            );
        };

        sweep_parts(&dir);
        let part_path = dir.join(format!(
            ".{}.{}.part",
            iso_file_name(&version, 1),
            uuid::Uuid::new_v4().simple()
        ));
        let write_failed = |e: std::io::Error| {
            tr!(
                "linux_media_dl.write_failed",
                path = dir.display(),
                error = e
            )
            .into_owned()
        };
        // Made here, exclusively; curl then writes into it (and starts it
        // over on a retry, which a pipe could not).
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part_path)
            .map_err(write_failed)?;
        let mut part = Part(Some(part_path.clone()));

        emit("download", 0);
        let mut child = curl()
            .args([
                "--retry",
                "3",
                "--speed-limit",
                "1024",
                "--speed-time",
                "60",
                "--max-filesize",
            ])
            .arg(release.size.to_string())
            .arg("--output")
            .arg(&part_path)
            .arg(&release.url)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(not_started)?;
        let mut stderr = child.stderr.take();
        let stderr_reader = std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(e) = stderr.as_mut() {
                let _ = e.read_to_string(&mut s);
            }
            s
        });
        // Polled, not read: a cancel stops a stalled connection at once.
        let status = loop {
            if CANCEL.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(cancelled());
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => return Err(e.to_string()),
            }
            let now = std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
            emit("download", now.min(release.size));
            std::thread::sleep(Duration::from_millis(250));
        };
        let err_text = stderr_reader.join().unwrap_or_default();
        if !status.success() {
            return Err(match failure(status.code(), &err_text) {
                Fetch::NotFound => none_released(),
                Fetch::Failed(m) => m,
            });
        }
        let received = std::fs::metadata(&part_path).map_err(write_failed)?.len();
        if received != release.size {
            return Err(tr!("linux_media_dl.wrong_size").into_owned());
        }

        emit("verify", received);
        let got = match sha256_file(&part_path) {
            Ok(h) => h,
            Err(_) if CANCEL.load(Ordering::SeqCst) => return Err(cancelled()),
            Err(e) => return Err(write_failed(e)),
        };
        if got != expected {
            return Err(tr!("linux_media_dl.bad_checksum").into_owned());
        }
        if CANCEL.load(Ordering::SeqCst) {
            return Err(cancelled());
        }
        // Opened for writing: Windows flushes only a file it may write.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&part_path)
            .and_then(|f| f.sync_all())
            .map_err(write_failed)?;

        let saved = give_name(&part_path, &dir, &version).map_err(write_failed)?;
        part.0 = None;
        tracing::info!(iso = %saved.display(), tag = %release.tag, "ready Linux boot medium downloaded");
        Ok(LinuxMediaDownloadDto {
            iso_path: saved.display().to_string(),
            iso_bytes: received,
            version,
            tag: release.tag,
        })
    }

    /// Rename the finished file to `OwnBackup-Linux-<version>.iso`, or
    /// ` (2)` and on when that exists — never replacing anything.
    #[cfg(target_os = "linux")]
    fn give_name(part: &Path, dir: &Path, version: &str) -> std::io::Result<PathBuf> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c = |p: &Path| {
            CString::new(p.as_os_str().as_bytes())
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
        };
        let from = c(part)?;
        for n in 1..1000 {
            let target = dir.join(iso_file_name(version, n));
            let to = c(&target)?;
            // SAFETY: two valid C strings.
            let r = unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    from.as_ptr(),
                    libc::AT_FDCWD,
                    to.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if r == 0 {
                return Ok(target);
            }
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EEXIST) => continue,
                // A file system without RENAME_NOREPLACE (FAT, some network
                // ones): a hard link fails on an existing name just the same.
                Some(libc::EINVAL) | Some(libc::ENOSYS) => {
                    match std::fs::hard_link(part, &target) {
                        Ok(()) => {
                            let _ = std::fs::remove_file(part);
                            return Ok(target);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(_) if !target.exists() => {
                            std::fs::rename(part, &target)?;
                            return Ok(target);
                        }
                        Err(_) => continue,
                    }
                }
                _ => return Err(e),
            }
        }
        Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
    }

    /// Windows: `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` refuses an
    /// existing name on every file system (`std::fs::rename` would replace).
    #[cfg(target_os = "windows")]
    fn give_name(part: &Path, dir: &Path, version: &str) -> std::io::Result<PathBuf> {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
        use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
        let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain([0]).collect() };
        let from = wide(part);
        for n in 1..1000 {
            let target = dir.join(iso_file_name(version, n));
            let to = wide(&target);
            // SAFETY: two NUL-terminated wide strings that outlive the call.
            match unsafe {
                MoveFileExW(
                    PCWSTR(from.as_ptr()),
                    PCWSTR(to.as_ptr()),
                    MOVEFILE_WRITE_THROUGH,
                )
            } {
                Ok(()) => return Ok(target),
                Err(e)
                    if e.code() == ERROR_ALREADY_EXISTS.to_hresult()
                        || e.code() == ERROR_FILE_EXISTS.to_hresult() =>
                {
                    continue
                }
                Err(e) => return Err(std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF)),
            }
        }
        Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
    }
}
