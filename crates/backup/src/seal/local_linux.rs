//! Linux implementation: a root-only directory pinned by its descriptor,
//! files created exclusively with mode 0600 and synced together with their
//! directory, and a machine key wrapped by `systemd-creds` — sealed to the
//! TPM2 when the machine has one, to the host key otherwise.
//!
//! There is no DPAPI on Linux and no account a key could be bound to: whoever
//! is root on this machine can open what the service can. The machine key
//! keeps the key files useless anywhere else (a copied directory, a disk taken
//! out), and with a TPM2 the key cannot leave the machine at all. Without
//! `systemd-creds` there is no store: nothing falls back to a plaintext key.

use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use own_backup_common::{tr, OwnBackupError, Result};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{bad, Backend, KEY_DIRECTORY_NAME, MAX_LOCAL_BLOB};
use crate::seal::{aead_open, aead_seal, fill_random};

/// The machine key, wrapped by `systemd-creds`, inside the key directory.
pub const MACHINE_KEY_FILE: &str = "machine.cred";
/// File framing: magic, then one byte naming what sealed it.
const MACHINE_MAGIC: &[u8; 8] = b"OBKMK\0\x01\0";
/// Bound into the credential: `systemd-creds` refuses another name.
const CREDENTIAL_NAME: &str = "own-backup-machine-key";
const KEY_SUFFIX: &str = ".key";
const MAX_CREDENTIAL: usize = 16 * 1024;
/// The private volume-password database, as on Windows.
pub const VOLUME_SECRETS_DB: &str = "volume-secrets.db";

/// What keeps the machine key on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyProtection {
    /// TPM2 and the host key together (`systemd-creds --with-key=tpm2+host`).
    Tpm2,
    /// The host key alone, `/var/lib/systemd/credential.secret`.
    Host,
}

impl KeyProtection {
    /// The name the service and the window report.
    pub fn as_str(self) -> &'static str {
        match self {
            KeyProtection::Tpm2 => "tpm2",
            KeyProtection::Host => "host",
        }
    }

    fn byte(self) -> u8 {
        match self {
            KeyProtection::Tpm2 => 2,
            KeyProtection::Host => 1,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            2 => Some(KeyProtection::Tpm2),
            1 => Some(KeyProtection::Host),
            _ => None,
        }
    }
}

/// Wraps and unwraps the machine key. The real one is `systemd-creds`; tests
/// inject their own.
pub(super) trait CredentialSealer: Send + Sync {
    /// What a NEW machine key would be sealed with here, or why none can be.
    fn available(&self) -> Result<KeyProtection>;
    fn seal(&self, with: KeyProtection, plain: &[u8]) -> Result<Vec<u8>>;
    fn unseal(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>>;
}

/// `systemd-creds` from systemd 250+ (Ubuntu 24.04 has 255, Debian 13 257).
pub(super) struct SystemdCreds;

impl SystemdCreds {
    fn program() -> &'static str {
        ["/usr/bin/systemd-creds", "/bin/systemd-creds"]
            .into_iter()
            .find(|p| Path::new(p).exists())
            .unwrap_or("systemd-creds")
    }

    /// Run with `input` on stdin; stdout back, held in wiped memory.
    fn run(args: &[&str], input: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        use std::process::{Command, Stdio};
        let mut child = Command::new(Self::program())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| no_store(format!("systemd-creds: {e}")))?;
        // A credential is a few hundred bytes: the pipe buffer takes the whole
        // input before the child reads it, so this cannot deadlock.
        let wrote = child.stdin.take().expect("stdin is piped").write_all(input);
        let mut out = Zeroizing::new(Vec::new());
        child
            .stdout
            .take()
            .expect("stdout is piped")
            .take(MAX_CREDENTIAL as u64 + 1)
            .read_to_end(&mut out)?;
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = (&mut e).take(4096).read_to_string(&mut err);
        }
        let status = child.wait()?;
        wrote?;
        if !status.success() {
            return Err(no_store(format!(
                "systemd-creds {}: {}",
                args.first().copied().unwrap_or_default(),
                err.lines()
                    .filter(|l| !l.contains("not located on encrypted media"))
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }
        if out.len() > MAX_CREDENTIAL {
            return Err(bad("systemd-creds output too large"));
        }
        Ok(out)
    }
}

impl CredentialSealer for SystemdCreds {
    fn available(&self) -> Result<KeyProtection> {
        let status = std::process::Command::new(Self::program())
            .arg("has-tpm2")
            .arg("--quiet")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| no_store(format!("systemd-creds: {e}")))?;
        // 0: firmware, driver, system and subsystem all support TPM2; any
        // other answer (a partial one included) leaves the host key.
        Ok(if status.success() {
            KeyProtection::Tpm2
        } else {
            KeyProtection::Host
        })
    }

    fn seal(&self, with: KeyProtection, plain: &[u8]) -> Result<Vec<u8>> {
        let with_key = match with {
            KeyProtection::Tpm2 => "--with-key=tpm2+host",
            KeyProtection::Host => "--with-key=host",
        };
        let name = format!("--name={CREDENTIAL_NAME}");
        // No PCR binding: a firmware or Secure Boot update must not cost
        // every authorization on the machine. The TPM still keeps the key from
        // leaving it, which is what this layer is for.
        let out = Self::run(
            &["encrypt", &name, with_key, "--tpm2-pcrs=", "-", "-"],
            plain,
        )?;
        Ok(out.to_vec()) // ciphertext: nothing secret in the returned Vec.
    }

    fn unseal(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let name = format!("--name={CREDENTIAL_NAME}");
        Self::run(&["decrypt", &name, "-", "-"], blob)
    }
}

fn no_store(detail: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Encryption(tr!("seal_local_linux.no_machine_key", error = detail).into_owned())
}

fn os(context: &str) -> OwnBackupError {
    let e = std::io::Error::last_os_error();
    OwnBackupError::Encryption(format!("Linux key store: {context}: {e}"))
}

fn c_name(name: &str) -> Result<CString> {
    CString::new(name).map_err(|_| bad("NUL in key-store file name"))
}

/// The directory, pinned by descriptor; every file is reached through it, so
/// a path swapped underneath after the checks reaches nothing.
pub(super) struct LinuxBackend {
    root: PathBuf,
    dir: File,
    owner: u32,
    sealer: Arc<dyn CredentialSealer>,
}

impl LinuxBackend {
    /// The installed service, as root.
    pub(super) fn new(data_dir: &Path) -> Result<Self> {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return Err(bad(tr!("seal_local_linux.root_only")));
        }
        Self::with_parts(data_dir, 0, Arc::new(SystemdCreds))
    }

    pub(super) fn with_parts(
        data_dir: &Path,
        owner: u32,
        sealer: Arc<dyn CredentialSealer>,
    ) -> Result<Self> {
        let parent_path = std::fs::canonicalize(data_dir)?;
        let parent = open_dir_at(libc::AT_FDCWD, parent_path.as_os_str().as_bytes())?;
        // Whoever may write the parent may rename the key directory away and
        // put their own in its place between two operations.
        check_dir(&parent, owner, 0o022, "data directory")?;
        let name = c_name(KEY_DIRECTORY_NAME)?;
        // SAFETY: mkdirat with a valid descriptor and C string.
        let made = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
        if made != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
            return Err(os("mkdir"));
        }
        let dir = open_dir_at(parent.as_raw_fd(), KEY_DIRECTORY_NAME.as_bytes())?;
        // Exactly 0700 and ours: an existing directory with any other access
        // is not adopted by changing it (as on Windows).
        check_dir_exact(&dir, owner)?;
        if made == 0 {
            parent.sync_all()?;
        }
        Ok(Self {
            root: parent_path.join(KEY_DIRECTORY_NAME),
            dir,
            owner,
            sealer,
        })
    }

    fn check_context(&self) -> Result<()> {
        check_dir_exact(&self.dir, self.owner)
    }

    fn file_name(id: Uuid) -> String {
        format!("{id}{KEY_SUFFIX}")
    }

    fn open_existing(&self, name: &str) -> Result<File> {
        let c = c_name(name)?;
        // SAFETY: openat on our pinned directory with a valid C string.
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NOCTTY,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: a descriptor we just opened and now own.
        let file = unsafe { File::from_raw_fd(fd) };
        check_file(&file, self.owner)?;
        Ok(file)
    }

    fn read_name(&self, name: &str) -> Result<Vec<u8>> {
        self.check_context()?;
        let mut file = self.open_existing(name)?;
        let mut data = Vec::new();
        (&mut file)
            .take(MAX_LOCAL_BLOB as u64 + 1)
            .read_to_end(&mut data)?;
        if data.len() > MAX_LOCAL_BLOB {
            return Err(bad("local key file too large"));
        }
        Ok(data)
    }

    /// Create `name` with `bytes`, never over anything, complete or not at
    /// all: a private temporary file is written and synced, then linked into
    /// place (a link refuses an existing name), then the directory is synced.
    /// `Ok(false)`: `name` already existed and is left as it was.
    fn publish(&self, name: &str, bytes: &[u8]) -> Result<bool> {
        self.check_context()?;
        let tmp = format!(".{name}.{}.tmp", Uuid::new_v4().simple());
        let tmp_c = c_name(&tmp)?;
        let name_c = c_name(name)?;
        // SAFETY: openat on our pinned directory with a valid C string.
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                tmp_c.as_ptr(),
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NOCTTY,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(os("create"));
        }
        // SAFETY: a descriptor we just created and now own.
        let mut file = unsafe { File::from_raw_fd(fd) };
        let unlink_tmp = || {
            // SAFETY: unlinkat on our pinned directory with a valid C string.
            unsafe { libc::unlinkat(self.dir.as_raw_fd(), tmp_c.as_ptr(), 0) };
        };
        let written = (|| -> Result<()> {
            // The umask can only take bits away; say exactly what it is.
            // SAFETY: fchmod on our own descriptor.
            if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
                return Err(os("chmod"));
            }
            check_file(&file, self.owner)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(e) = written {
            unlink_tmp();
            return Err(e);
        }
        // SAFETY: linkat within our pinned directory, valid C strings.
        let linked = unsafe {
            libc::linkat(
                self.dir.as_raw_fd(),
                tmp_c.as_ptr(),
                self.dir.as_raw_fd(),
                name_c.as_ptr(),
                0,
            )
        };
        let link_error = (linked != 0).then(std::io::Error::last_os_error);
        unlink_tmp();
        self.dir.sync_all()?;
        match link_error {
            None => Ok(true),
            Some(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(false),
            // No fallback to a rename: a rename replaces, and the one thing
            // this must never do is put a key over another.
            Some(e) => Err(OwnBackupError::Encryption(format!(
                "Linux key store: link: {e}"
            ))),
        }
    }

    fn remove_name(&self, name: &str) -> Result<()> {
        self.check_context()?;
        // Refuse what is not ours to remove (a hard link, someone else's
        // file), and say "already gone" only when it really is.
        match self.open_existing(name) {
            Ok(_) => {}
            Err(OwnBackupError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        let c = c_name(name)?;
        // SAFETY: unlinkat on our pinned directory with a valid C string.
        if unsafe { libc::unlinkat(self.dir.as_raw_fd(), c.as_ptr(), 0) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e.into());
            }
        }
        self.dir.sync_all()?;
        Ok(())
    }

    /// The machine key and what protects it. `create`: make one when there
    /// is none yet (the first authorization, the first remembered password);
    /// otherwise a missing key is an error — whatever was sealed with it is
    /// gone with it.
    pub(super) fn machine_key(&self, create: bool) -> Result<(Zeroizing<[u8; 32]>, KeyProtection)> {
        match self.read_name(MACHINE_KEY_FILE) {
            Ok(framed) => self.unseal_machine_key(&framed),
            Err(OwnBackupError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound && create => {
                let with = self.sealer.available()?;
                let mut key = Zeroizing::new([0u8; 32]);
                fill_random(&mut key[..])?;
                let blob = self.sealer.seal(with, &key[..])?;
                if blob.is_empty() || blob.len() > MAX_LOCAL_BLOB - MACHINE_MAGIC.len() - 1 {
                    return Err(bad("systemd-creds output has an unexpected size"));
                }
                let mut framed = Vec::with_capacity(MACHINE_MAGIC.len() + 1 + blob.len());
                framed.extend_from_slice(MACHINE_MAGIC);
                framed.push(with.byte());
                framed.extend_from_slice(&blob);
                // Whoever links first wins; the loser reads the winner's key,
                // so nothing sealed by either is lost.
                self.publish(MACHINE_KEY_FILE, &framed)?;
                // Probe the bytes on disk, not the key in memory.
                self.machine_key(false)
            }
            Err(OwnBackupError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(bad(tr!("seal_local_linux.machine_key_missing")))
            }
            Err(e) => Err(e),
        }
    }

    fn unseal_machine_key(&self, framed: &[u8]) -> Result<(Zeroizing<[u8; 32]>, KeyProtection)> {
        let with = framing(framed)?;
        let plain = self.sealer.unseal(&framed[MACHINE_MAGIC.len() + 1..])?;
        if plain.len() != 32 {
            return Err(bad(tr!("seal_local.bad_secret_length")));
        }
        let mut key = Zeroizing::new([0u8; 32]);
        key.copy_from_slice(&plain);
        Ok((key, with))
    }

    /// What protects the machine key, without opening it: the existing key's
    /// framing, or what a new one would get.
    pub(super) fn protection(&self) -> Result<KeyProtection> {
        match self.read_name(MACHINE_KEY_FILE) {
            Ok(framed) => framing(&framed),
            Err(OwnBackupError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                self.sealer.available()
            }
            Err(e) => Err(e),
        }
    }
}

fn framing(framed: &[u8]) -> Result<KeyProtection> {
    if framed.len() <= MACHINE_MAGIC.len() + 1 || &framed[..MACHINE_MAGIC.len()] != MACHINE_MAGIC {
        return Err(bad(tr!("seal_local.bad_format")));
    }
    KeyProtection::from_byte(framed[MACHINE_MAGIC.len()])
        .ok_or_else(|| bad(tr!("seal_local.bad_format")))
}

fn open_dir_at(at: libc::c_int, path: &[u8]) -> Result<File> {
    let c = CString::new(path).map_err(|_| bad("NUL in key-store path"))?;
    // SAFETY: openat with a valid descriptor (or AT_FDCWD) and C string.
    let fd = unsafe {
        libc::openat(
            at,
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(os("open directory"));
    }
    // SAFETY: a descriptor we just opened and now own.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn stat(file: &File) -> Result<libc::stat> {
    // SAFETY: fstat into a zeroed struct on our own descriptor.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(file.as_raw_fd(), &mut st) } != 0 {
        return Err(os("stat"));
    }
    Ok(st)
}

fn check_dir(file: &File, owner: u32, forbidden: u32, what: &str) -> Result<()> {
    let st = stat(file)?;
    if st.st_mode & libc::S_IFMT != libc::S_IFDIR
        || st.st_uid != owner
        || st.st_mode & forbidden != 0
    {
        return Err(bad(tr!("seal_local_linux.unsafe_directory", what = what)));
    }
    Ok(())
}

fn check_dir_exact(file: &File, owner: u32) -> Result<()> {
    let st = stat(file)?;
    if st.st_mode & libc::S_IFMT != libc::S_IFDIR
        || st.st_uid != owner
        || st.st_mode & 0o7777 != 0o700
    {
        return Err(bad(tr!(
            "seal_local_linux.unsafe_directory",
            what = KEY_DIRECTORY_NAME
        )));
    }
    Ok(())
}

/// A regular file of ours, with one name, readable by nobody else.
fn check_file(file: &File, owner: u32) -> Result<()> {
    let st = stat(file)?;
    if st.st_mode & libc::S_IFMT != libc::S_IFREG
        || st.st_nlink != 1
        || st.st_uid != owner
        || st.st_mode & 0o077 != 0
        || st.st_size as u64 > MAX_LOCAL_BLOB as u64
    {
        return Err(bad(
            "unexpected file type, owner, access, hard link or size in key store",
        ));
    }
    Ok(())
}

impl Backend for LinuxBackend {
    fn directory(&self) -> &Path {
        &self.root
    }

    fn protect(&self, entropy: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
        let (machine, _) = self.machine_key(true)?;
        let wrap = derive(&machine, "local-service-key");
        let mut nonce = [0u8; 12];
        fill_random(&mut nonce)?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&aead_seal(&wrap, &nonce, entropy, key)?);
        Ok(out)
    }

    fn unprotect(&self, entropy: &[u8], blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        if blob.len() < 12 + 16 || blob.len() > MAX_LOCAL_BLOB {
            return Err(bad("invalid local key size"));
        }
        let (machine, _) = self.machine_key(false)?;
        let wrap = derive(&machine, "local-service-key");
        let nonce: [u8; 12] = blob[..12].try_into().expect("checked length");
        aead_open(&wrap, &nonce, entropy, &blob[12..])
    }

    fn write_new(&self, id: Uuid, blob: &[u8]) -> Result<()> {
        if blob.len() > MAX_LOCAL_BLOB {
            return Err(bad("local key file too large"));
        }
        if self.publish(&Self::file_name(id), blob)? {
            Ok(())
        } else {
            Err(bad("a local key with this id already exists"))
        }
    }

    fn read(&self, id: Uuid) -> Result<Vec<u8>> {
        self.read_name(&Self::file_name(id))
    }

    fn remove(&self, id: Uuid) -> Result<()> {
        self.remove_name(&Self::file_name(id))
    }

    /// The keys on disk, by name. Anything that is not `<uuid>.key` is not
    /// one of ours and is left alone: this list feeds a delete.
    fn list_ids(&self) -> Result<Vec<Uuid>> {
        self.check_context()?;
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&self.root)
            .map_err(|e| bad(tr!("seal_local_windows.dir_unreadable", error = e)))?
        {
            let entry =
                entry.map_err(|e| bad(tr!("seal_local_windows.dir_entry_error", error = e)))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(stem) = name.strip_suffix(KEY_SUFFIX) else {
                continue;
            };
            if let Ok(id) = Uuid::parse_str(stem) {
                ids.push(id);
            }
        }
        Ok(ids)
    }
}

/// A purpose-specific key from the machine key: the job keys and the volume
/// passwords never share one.
fn derive(machine: &[u8; 32], purpose: &str) -> Zeroizing<[u8; 32]> {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, &machine[..]);
    let mut out = Zeroizing::new([0u8; 32]);
    let info = format!("{}/linux-machine-key/{purpose}", super::SCHEME);
    hk.expand(info.as_bytes(), &mut out[..])
        .expect("32 bytes is within HKDF-SHA256");
    out
}

/// The root-only directory checked before SQLite may create the private
/// password database and its WAL/SHM files there, and the sealing of those
/// passwords under the machine key.
pub struct PrivateSecretDatabase {
    backend: Arc<LinuxBackend>,
    path: PathBuf,
}

impl PrivateSecretDatabase {
    pub fn new(data_dir: &Path) -> Result<Self> {
        Self::from_backend(LinuxBackend::new(data_dir)?)
    }

    fn from_backend(backend: LinuxBackend) -> Result<Self> {
        let path = backend.root.join(VOLUME_SECRETS_DB);
        let store = Self {
            backend: Arc::new(backend),
            path,
        };
        store.validate_files()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The database and its WAL/SHM, where they exist: regular files of ours
    /// with one name. SQLite creates them under the service's umask; they are
    /// narrowed to 0600 here (the directory is 0700 in any case).
    pub fn validate_files(&self) -> Result<()> {
        self.backend.check_context()?;
        for suffix in ["", "-wal", "-shm"] {
            let name = format!("{VOLUME_SECRETS_DB}{suffix}");
            let c = c_name(&name)?;
            // SAFETY: openat on the pinned directory with a valid C string.
            let fd = unsafe {
                libc::openat(
                    self.backend.dir.as_raw_fd(),
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NOCTTY,
                )
            };
            if fd < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::NotFound {
                    continue;
                }
                return Err(e.into());
            }
            // SAFETY: a descriptor we just opened and now own.
            let file = unsafe { File::from_raw_fd(fd) };
            let st = stat(&file)?;
            if st.st_mode & libc::S_IFMT != libc::S_IFREG
                || st.st_nlink != 1
                || st.st_uid != self.backend.owner
            {
                return Err(bad(
                    "unexpected file type, owner or hard link in the password database",
                ));
            }
            // SAFETY: fchmod on our own descriptor.
            if st.st_mode & 0o077 != 0 && unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
                return Err(os("chmod"));
            }
        }
        Ok(())
    }

    /// Seals volume passwords under the machine key, as DPAPI does on Windows.
    pub fn secret_store(&self) -> Arc<dyn crate::secrets::SecretStore + Send + Sync> {
        Arc::new(MachineKeySecrets {
            backend: self.backend.clone(),
        })
    }
}

struct MachineKeySecrets {
    backend: Arc<LinuxBackend>,
}

const SECRET_AAD: &[u8] = b"own-backup/v2/volume-secret/1";

impl crate::secrets::SecretStore for MachineKeySecrets {
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let (machine, _) = self.backend.machine_key(true)?;
        let key = derive(&machine, "volume-secrets");
        let mut nonce = [0u8; 12];
        fill_random(&mut nonce)?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&aead_seal(&key, &nonce, SECRET_AAD, plaintext)?);
        Ok(out)
    }

    fn unprotect(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        if blob.len() < 12 + 16 {
            return Err(bad("sealed password too short"));
        }
        let (machine, _) = self.backend.machine_key(false)?;
        let key = derive(&machine, "volume-secrets");
        let nonce: [u8; 12] = blob[..12].try_into().expect("checked length");
        aead_open(&key, &nonce, SECRET_AAD, &blob[12..])
    }
}

/// Data directories whose key directory the engine must keep out of every
/// copy, besides the configured one: the service registers its own at start
/// (it may run with another `--data-dir`).
static KEY_STORE_DATA_DIRS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Tell the engine where a key store lives (the service, at start).
pub fn register_data_dir(data_dir: &Path) {
    if let Ok(mut dirs) = KEY_STORE_DATA_DIRS.lock() {
        if !dirs.iter().any(|d| d == data_dir) {
            dirs.push(data_dir.to_path_buf());
        }
    }
}

/// Does the volume `source` hold a key directory of this machine that has
/// anything in it? Decision 6/E4: such a volume is copied only with that
/// directory left out, whatever the job's rules say.
///
/// Answered by where the directory's bytes lie (`source_identity`: the same
/// extent comparison as "destination on the source", through LUKS, LVM and
/// partitions alike). When that cannot be told, the answer is yes: the cost
/// of a wrong yes is one scan for a name, of a wrong no a copy carrying the
/// keys.
pub fn key_store_on_source(source: &str) -> bool {
    let mut dirs: Vec<PathBuf> = KEY_STORE_DATA_DIRS
        .lock()
        .map(|d| d.clone())
        .unwrap_or_default();
    let configured = own_backup_common::config::AppConfig::load_or_default().data_dir;
    if !dirs.contains(&configured) {
        dirs.push(configured);
    }
    key_store_in(source, &dirs)
}

/// [`key_store_on_source`] over the data directories given.
fn key_store_in(source: &str, dirs: &[PathBuf]) -> bool {
    use std::os::unix::fs::FileTypeExt;
    // A volume is a block device. A regular file read as a source (an image
    // the CLI is pointed at, the engine's own tests) is not where this
    // machine's key directory lives, and must not be refused for it.
    if std::fs::metadata(source).is_ok_and(|m| !m.file_type().is_block_device()) {
        return false;
    }
    dirs.iter()
        .map(|d| d.join(KEY_DIRECTORY_NAME))
        .filter(|d| holds_anything(d))
        .any(|d| {
            let Some(path) = d.to_str() else { return true };
            !matches!(
                crate::source_identity::where_the_destination_sits(&[source.to_string()], path),
                crate::source_identity::DestPlacement::Clear
            )
        })
}

/// An entry of any kind in `dir`; an unreadable directory counts as full.
fn holds_anything(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_some(),
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// What protects (or would protect) this machine's key store: `tpm2` or
/// `host`; an error says why there is no store at all.
pub fn protection(data_dir: &Path) -> Result<KeyProtection> {
    LinuxBackend::new(data_dir)?.protection()
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A stand-in for `systemd-creds`: AES-GCM under a fixed "host" key.
    pub(crate) struct FakeCreds {
        pub(crate) host: [u8; 32],
        pub(crate) with: Option<KeyProtection>,
        pub(crate) seals: AtomicUsize,
    }

    impl FakeCreds {
        pub(crate) fn new(host: u8) -> Arc<Self> {
            Arc::new(Self {
                host: [host; 32],
                with: Some(KeyProtection::Host),
                seals: AtomicUsize::new(0),
            })
        }
    }

    impl CredentialSealer for FakeCreds {
        fn available(&self) -> Result<KeyProtection> {
            self.with.ok_or_else(|| no_store("no systemd-creds here"))
        }
        fn seal(&self, _with: KeyProtection, plain: &[u8]) -> Result<Vec<u8>> {
            self.seals.fetch_add(1, Ordering::SeqCst);
            let mut nonce = [0u8; 12];
            fill_random(&mut nonce)?;
            let mut out = nonce.to_vec();
            out.extend_from_slice(&aead_seal(&self.host, &nonce, b"cred", plain)?);
            Ok(out)
        }
        fn unseal(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
            aead_open(
                &self.host,
                blob[..12].try_into().unwrap(),
                b"cred",
                &blob[12..],
            )
        }
    }

    pub(crate) fn me() -> u32 {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() }
    }

    /// A data directory as the package makes it: owned by us, not writable
    /// by group or others (a tempdir is 0700 already).
    pub(crate) fn data_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    pub(crate) fn backend(dir: &Path, creds: &Arc<FakeCreds>) -> LinuxBackend {
        LinuxBackend::with_parts(dir, me(), creds.clone()).unwrap()
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    #[test]
    fn the_directory_is_0700_and_key_files_0600_created_once() {
        let data = data_dir();
        let creds = FakeCreds::new(1);
        let b = backend(data.path(), &creds);
        let root = data.path().join(KEY_DIRECTORY_NAME);
        assert_eq!(mode(&root), 0o700);

        let id = Uuid::new_v4();
        b.write_new(id, b"sealed bytes").unwrap();
        assert_eq!(mode(&root.join(format!("{id}.key"))), 0o600);
        assert_eq!(b.read(id).unwrap(), b"sealed bytes");
        assert!(b.write_new(id, b"over it").is_err(), "never over a key");
        assert_eq!(b.read(id).unwrap(), b"sealed bytes");
        assert_eq!(b.list_ids().unwrap(), vec![id]);
        b.remove(id).unwrap();
        b.remove(id).unwrap(); // already gone is fine
        assert!(b.read(id).is_err());
        // No temporary file is left behind by any of it.
        let left: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn a_permissive_directory_is_not_adopted() {
        use std::os::unix::fs::PermissionsExt;
        let data = data_dir();
        let root = data.path().join(KEY_DIRECTORY_NAME);
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(LinuxBackend::with_parts(data.path(), me(), FakeCreds::new(1)).is_err());
        assert_eq!(mode(&root), 0o755, "left as it was");

        // A symlink in its place is not followed either.
        std::fs::remove_dir(&root).unwrap();
        let elsewhere = data.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &root).unwrap();
        assert!(LinuxBackend::with_parts(data.path(), me(), FakeCreds::new(1)).is_err());

        // Nor is a data directory others may write into.
        let open = data.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(LinuxBackend::with_parts(&open, me(), FakeCreds::new(1)).is_err());
        assert!(!open.join(KEY_DIRECTORY_NAME).exists());
    }

    #[test]
    fn hard_links_symlinks_and_open_access_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let data = data_dir();
        let b = backend(data.path(), &FakeCreds::new(1));
        let root = data.path().join(KEY_DIRECTORY_NAME);
        let id = Uuid::new_v4();
        b.write_new(id, b"protected").unwrap();
        let file = root.join(format!("{id}.key"));

        let alias = data.path().join("alias");
        std::fs::hard_link(&file, &alias).unwrap();
        assert!(b.read(id).is_err());
        assert!(b.remove(id).is_err(), "not removed through a second name");
        assert!(file.exists());
        std::fs::remove_file(&alias).unwrap();
        assert_eq!(b.read(id).unwrap(), b"protected");

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(b.read(id).is_err());
        std::fs::remove_file(&file).unwrap();

        let target = data.path().join("target");
        std::fs::write(&target, b"bait").unwrap();
        std::os::unix::fs::symlink(&target, &file).unwrap();
        assert!(b.read(id).is_err());
        assert!(target.exists());
    }

    #[test]
    fn the_machine_key_is_made_once_reopened_and_never_replaced() {
        let data = data_dir();
        let creds = FakeCreds::new(3);
        let b = backend(data.path(), &creds);
        assert!(b.machine_key(false).is_err(), "not made by a read");
        let (first, with) = b.machine_key(true).unwrap();
        assert_eq!(with, KeyProtection::Host);
        assert_eq!(b.protection().unwrap(), KeyProtection::Host);
        let again = backend(data.path(), &creds);
        assert_eq!(&*again.machine_key(false).unwrap().0, &*first);
        assert_eq!(&*again.machine_key(true).unwrap().0, &*first);
        assert_eq!(creds.seals.load(Ordering::SeqCst), 1);
        // Sealed elsewhere (another host key): it does not open here.
        let other = backend(data.path(), &FakeCreds::new(4));
        assert!(other.machine_key(false).is_err());
    }

    /// Several first users at once (the service's first authorization and its
    /// first remembered password) must all end up with the one key on disk.
    #[test]
    fn concurrent_first_uses_agree_on_one_machine_key() {
        let data = data_dir();
        let creds = FakeCreds::new(5);
        let start = Arc::new(std::sync::Barrier::new(8));
        let keys: Vec<[u8; 32]> = (0..8)
            .map(|_| {
                let dir = data.path().to_path_buf();
                let creds = creds.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    let b = backend(&dir, &creds);
                    start.wait();
                    *b.machine_key(true).unwrap().0
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        assert!(keys.windows(2).all(|w| w[0] == w[1]));
        let root = data.path().join(KEY_DIRECTORY_NAME);
        let names: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![MACHINE_KEY_FILE.to_string()]);
    }

    #[test]
    fn without_systemd_creds_there_is_no_store_and_no_plaintext_key() {
        let data = data_dir();
        let creds = Arc::new(FakeCreds {
            host: [6; 32],
            with: None,
            seals: AtomicUsize::new(0),
        });
        let b = backend(data.path(), &creds);
        let e = b.protect(b"binding", &[7; 32]).unwrap_err().to_string();
        assert!(e.contains("no systemd-creds here"), "{e}");
        assert!(b.protection().is_err());
        assert!(!data
            .path()
            .join(KEY_DIRECTORY_NAME)
            .join(MACHINE_KEY_FILE)
            .exists());
    }

    #[test]
    fn job_keys_and_volume_passwords_are_sealed_under_the_machine_key() {
        let data = data_dir();
        let creds = FakeCreds::new(8);
        let b = backend(data.path(), &creds);
        let blob = b.protect(b"binding", &[9; 32]).unwrap();
        assert_eq!(&*b.unprotect(b"binding", &blob).unwrap(), &[9; 32]);
        assert!(b.unprotect(b"other job", &blob).is_err());

        let db = PrivateSecretDatabase::from_backend(backend(data.path(), &creds)).unwrap();
        assert_eq!(
            db.path(),
            data.path().join(KEY_DIRECTORY_NAME).join(VOLUME_SECRETS_DB)
        );
        let secrets = db.secret_store();
        let sealed = secrets.protect(b"luks passphrase").unwrap();
        assert_eq!(&*secrets.unprotect(&sealed).unwrap(), b"luks passphrase");
        // The two purposes do not share a key.
        assert!(b.unprotect(SECRET_AAD, &sealed).is_err());
    }

    /// A key directory with something in it, on a source that cannot be
    /// placed: the engine must treat it as on that source. An empty one, or
    /// none, asks for nothing.
    #[test]
    fn a_key_store_that_cannot_be_placed_counts_as_on_the_source() {
        // Not registered: other tests run the engine in this process.
        let data = data_dir();
        let dirs = [data.path().to_path_buf()];
        let nowhere = "/dev/own-backup-test-no-such-device";
        assert!(!key_store_in(nowhere, &dirs), "no key directory yet");
        let root = data.path().join(KEY_DIRECTORY_NAME);
        std::fs::create_dir(&root).unwrap();
        assert!(!key_store_in(nowhere, &dirs), "an empty one holds nothing");
        std::fs::write(root.join(MACHINE_KEY_FILE), b"x").unwrap();
        assert!(key_store_in(nowhere, &dirs), "fail closed");
    }

    /// The real `systemd-creds`, host key: only as root, where it is present.
    #[test]
    fn real_systemd_creds_round_trip_when_available() {
        if me() != 0 || !Path::new(SystemdCreds::program()).exists() {
            eprintln!("skipped: needs root and systemd-creds");
            return;
        }
        let creds = SystemdCreds;
        let blob = match creds.seal(KeyProtection::Host, &[0x5a; 32]) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipped: systemd-creds cannot seal here: {e}");
                return;
            }
        };
        assert!(!blob.windows(32).any(|w| w == [0x5a; 32]));
        assert_eq!(&*creds.unseal(&blob).unwrap(), &[0x5a; 32]);
        let mut broken = blob.clone();
        let last = broken.len() - 1;
        broken[last] ^= 1;
        assert!(creds.unseal(&broken).is_err());
    }
}
