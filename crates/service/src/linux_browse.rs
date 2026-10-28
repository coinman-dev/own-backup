//! Opened Linux partitions, for the Total Commander plugin
//! (`crates/tc-plugin`): the files of an ext4 or btrfs partition — inside LVM
//! and LUKS when it is — read in place, without mounting it and without taking its
//! disk from Windows (`own_backup_backup::linux_browse`).
//!
//! An opened partition is a session. `ext4-view` keeps its filesystem behind
//! `Rc`, so each session lives in a thread of its own and is asked for
//! listings and file pieces over a channel. The id is random, so a session
//! is only of use to the caller that opened it. A session nobody has asked
//! for in half an hour is closed, and so is the oldest when there are too
//! many — the plugin opens it again, asking for the passphrase again.

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use own_backup_backup::linux_browse::{self, names, Entry, OpenError, RootInfo, Source};

/// A session nobody asked for this long is closed.
pub const IDLE: Duration = Duration::from_secs(30 * 60);
/// Open sessions at most; the least recently used goes first.
pub const MAX_SESSIONS: usize = 8;
/// The largest piece of a file one call returns.
pub const MAX_READ: usize = 1 << 20;

/// What the plugin asks for (`op`), besides file pieces ([`ReadRequest`]).
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    /// The partitions there are to open (answered by the caller, who knows
    /// the disks: [`Sessions::remember_sources`]).
    Sources,
    Open {
        source: String,
        #[serde(default)]
        passphrase: Option<Zeroizing<String>>,
    },
    List {
        session: u64,
        root: usize,
        path: String,
    },
    Stat {
        session: u64,
        root: usize,
        path: String,
    },
    Close {
        session: u64,
    },
}

/// A piece of a file: `len` bytes (at most [`MAX_READ`]) from `offset`.
#[derive(Debug, Deserialize)]
pub struct ReadRequest {
    pub session: u64,
    pub root: usize,
    pub path: String,
    pub offset: u64,
    pub len: usize,
}

/// The answer to [`Request::Open`].
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Opened {
    Open {
        session: u64,
        encrypted: bool,
        roots: Vec<RootInfo>,
    },
    NeedsPassword,
    WrongPassword,
}

/// What a partition looks like to the plugin.
#[derive(Debug, Serialize)]
pub struct SourceInfo {
    pub id: String,
    /// The folder name: unique, and nothing in it Windows refuses.
    pub name: String,
    pub encrypted: bool,
    pub size: u64,
}

type Reply<T> = SyncSender<Result<T, String>>;

enum Job {
    List {
        root: usize,
        path: Vec<u8>,
        reply: Reply<Vec<Entry>>,
    },
    Stat {
        root: usize,
        path: Vec<u8>,
        reply: Reply<Entry>,
    },
    Read {
        root: usize,
        path: Vec<u8>,
        offset: u64,
        len: usize,
        reply: Reply<Vec<u8>>,
    },
}

struct Session {
    jobs: Sender<Job>,
    last_used: Instant,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<u64, Session>,
    /// The partitions as last listed, by id: a partition is opened only
    /// where the service itself found it, never at a place a caller names.
    sources: HashMap<String, Source>,
}

#[derive(Default)]
pub struct Sessions {
    inner: Mutex<Inner>,
}

fn random_id() -> u64 {
    let mut b = [0u8; 8];
    let _ = getrandom::getrandom(&mut b);
    // Zero is what a plugin without a session sends.
    u64::from_le_bytes(b).max(1)
}

impl Sessions {
    pub fn remember_sources(&self, sources: &[Source]) {
        let mut inner = self.inner.lock().unwrap();
        inner.sources = sources.iter().map(|s| (s.id.clone(), s.clone())).collect();
    }

    pub fn source(&self, id: &str) -> Option<Source> {
        self.inner.lock().unwrap().sources.get(id).cloned()
    }

    /// Open `source` (blocking: the passphrase's key derivation takes a
    /// second or more).
    pub fn open(
        &self,
        source: &Source,
        passphrase: Option<Zeroizing<String>>,
    ) -> Result<Opened, String> {
        let vol = linux_browse::open_partition(&source.disk, source.offset, source.size)
            .map_err(|e| e.to_string())?;
        self.open_volume(vol, passphrase, &source.id)
    }

    /// [`Sessions::open`] of a volume already in hand (the tests' images).
    pub fn open_volume(
        &self,
        vol: Box<dyn own_backup_backup::fat_map::RawVolume + Send>,
        passphrase: Option<Zeroizing<String>>,
        what: &str,
    ) -> Result<Opened, String> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (jobs, jobs_rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("linux-browse".into())
            .spawn(move || {
                let opened = linux_browse::open(vol, passphrase.as_ref().map(|p| p.as_bytes()));
                drop(passphrase);
                let mut part = match opened {
                    Ok(p) => {
                        let _ = ready_tx.send(Ok((p.encrypted, p.info())));
                        p
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                // Until the session is dropped (its sender with it).
                while let Ok(job) = jobs_rx.recv() {
                    match job {
                        Job::List { root, path, reply } => {
                            let _ = reply.send(fs_of(&mut part, root).and_then(|f| f.list(&path)));
                        }
                        Job::Stat { root, path, reply } => {
                            let _ = reply.send(fs_of(&mut part, root).and_then(|f| f.stat(&path)));
                        }
                        Job::Read {
                            root,
                            path,
                            offset,
                            len,
                            reply,
                        } => {
                            let _ = reply.send(
                                fs_of(&mut part, root).and_then(|f| f.read(&path, offset, len)),
                            );
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let (encrypted, roots) = match ready_rx.recv() {
            Ok(Ok(opened)) => opened,
            Ok(Err(OpenError::NeedsPassword)) => return Ok(Opened::NeedsPassword),
            Ok(Err(OpenError::WrongPassword)) => return Ok(Opened::WrongPassword),
            Ok(Err(OpenError::Failed(why))) => return Err(why),
            Err(_) => return Err("the partition's reader stopped".into()),
        };
        let id = random_id();
        let mut inner = self.inner.lock().unwrap();
        Self::expire(&mut inner);
        while inner.sessions.len() >= MAX_SESSIONS {
            let oldest = inner
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.last_used)
                .map(|(id, _)| *id);
            match oldest {
                Some(old) => inner.sessions.remove(&old),
                None => break,
            };
        }
        inner.sessions.insert(
            id,
            Session {
                jobs,
                last_used: Instant::now(),
            },
        );
        tracing::info!(
            what,
            encrypted,
            filesystems = roots.len(),
            open = inner.sessions.len(),
            "linux browse: partition opened"
        );
        Ok(Opened::Open {
            session: id,
            encrypted,
            roots,
        })
    }

    fn expire(inner: &mut Inner) {
        inner.sessions.retain(|_, s| s.last_used.elapsed() < IDLE);
    }

    fn jobs(&self, session: u64) -> Result<Sender<Job>, String> {
        let mut inner = self.inner.lock().unwrap();
        Self::expire(&mut inner);
        let s = inner
            .sessions
            .get_mut(&session)
            .ok_or_else(|| SESSION_GONE.to_string())?;
        s.last_used = Instant::now();
        Ok(s.jobs.clone())
    }

    fn ask<T>(&self, session: u64, job: impl FnOnce(Reply<T>) -> Job) -> Result<T, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.jobs(session)?
            .send(job(tx))
            .map_err(|_| SESSION_GONE.to_string())?;
        rx.recv().map_err(|_| SESSION_GONE.to_string())?
    }

    pub fn list(&self, session: u64, root: usize, path: &str) -> Result<Vec<Entry>, String> {
        let path = names::path_from_display(path);
        self.ask(session, |reply| Job::List { root, path, reply })
    }

    pub fn stat(&self, session: u64, root: usize, path: &str) -> Result<Entry, String> {
        let path = names::path_from_display(path);
        self.ask(session, |reply| Job::Stat { root, path, reply })
    }

    pub fn read(&self, r: &ReadRequest) -> Result<Vec<u8>, String> {
        let path = names::path_from_display(&r.path);
        let (offset, len) = (r.offset, r.len.min(MAX_READ));
        self.ask(r.session, |reply| Job::Read {
            root: r.root,
            path,
            offset,
            len,
            reply,
        })
    }

    pub fn close(&self, session: u64) {
        self.inner.lock().unwrap().sessions.remove(&session);
    }
}

fn fs_of(part: &mut linux_browse::Partition, root: usize) -> Result<&mut linux_browse::Fs, String> {
    part.roots
        .get_mut(root)
        .ok_or_else(|| "no such filesystem in the partition".to_string())
        .and_then(|r| r.fs.as_mut().map_err(|e| e.clone()))
}

/// What the plugin sees when its session has gone (idle, or the service
/// restarted): it opens the partition again. Matched on by the plugin, so
/// never translated.
pub const SESSION_GONE: &str = "linux-browse: no such session";

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(Vec<u8>);
    impl own_backup_backup::fat_map::RawVolume for Mem {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> own_backup_common::Result<()> {
            let s = offset as usize;
            buf.copy_from_slice(&self.0[s..s + buf.len()]);
            Ok(())
        }
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
    }

    fn ext4_image() -> Option<Vec<u8>> {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("home/user")).unwrap();
        std::fs::write(src.join("home/user/note.txt"), "hello").unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(8 << 20)
            .unwrap();
        let ok = std::process::Command::new("mke2fs")
            .args(["-q", "-F", "-t", "ext4", "-d"])
            .arg(&src)
            .arg(&img)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then(|| std::fs::read(&img).unwrap())
    }

    #[test]
    fn a_session_lists_and_reads_until_it_is_closed() {
        let Some(img) = ext4_image() else {
            eprintln!("no mke2fs; skipped");
            return;
        };
        let s = Sessions::default();
        let Opened::Open { session, roots, .. } =
            s.open_volume(Box::new(Mem(img)), None, "test").unwrap()
        else {
            panic!("not opened");
        };
        assert_eq!(roots.len(), 1);
        let user = s.list(session, 0, "\\home\\user").unwrap();
        assert_eq!(user[0].name, "note.txt");
        assert_eq!(
            s.stat(session, 0, "\\home\\user\\note.txt").unwrap().size,
            5
        );
        let r = ReadRequest {
            session,
            root: 0,
            path: "\\home\\user\\note.txt".into(),
            offset: 1,
            len: 100,
        };
        assert_eq!(s.read(&r).unwrap(), b"ello");
        assert!(s.list(session, 3, "\\").is_err(), "no such filesystem");
        assert!(s
            .list(session + 1, 0, "\\")
            .unwrap_err()
            .contains("no such session"));
        s.close(session);
        assert_eq!(s.read(&r).unwrap_err(), SESSION_GONE);
    }

    #[test]
    fn too_many_sessions_close_the_oldest() {
        let Some(img) = ext4_image() else {
            return;
        };
        let s = Sessions::default();
        let mut ids = Vec::new();
        for _ in 0..MAX_SESSIONS + 2 {
            if let Opened::Open { session, .. } = s
                .open_volume(Box::new(Mem(img.clone())), None, "test")
                .unwrap()
            {
                ids.push(session);
            }
        }
        assert!(s.list(ids[0], 0, "\\").is_err(), "the first went");
        assert!(s.list(ids[1], 0, "\\").is_err());
        assert!(s.list(*ids.last().unwrap(), 0, "\\").is_ok());
    }

    #[test]
    fn requests_are_read_as_the_plugin_writes_them() {
        let r: Request =
            serde_json::from_str(r#"{"op":"open","source":"x#3","passphrase":"pw"}"#).unwrap();
        assert!(
            matches!(r, Request::Open { ref source, passphrase: Some(ref p) } if source == "x#3" && p.as_str() == "pw")
        );
        let r: Request =
            serde_json::from_str(r#"{"op":"list","session":5,"root":0,"path":"\\a"}"#).unwrap();
        assert!(matches!(r, Request::List { session: 5, root: 0, ref path } if path == "\\a"));
        assert_eq!(
            serde_json::to_value(Opened::NeedsPassword).unwrap(),
            serde_json::json!({"result": "needs_password"})
        );
    }
}
