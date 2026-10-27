//! What a Total Commander path means here, independent of Windows and of
//! the transport: the top level lists the partitions, a partition opens
//! (asking for its passphrase when it is encrypted), and inside it are its
//! filesystems — the partition's own, or its logical volumes as folders.
//!
//! ```text
//! \                                   the partitions (Диск 3 · раздел 3 · …)
//! \Диск 3 · раздел 3 · …              its files, or its logical volumes
//! \Диск 3 · раздел 3 · …\vgubuntu-root\home\user
//! ```
//!
//! A session the service has closed (half an hour idle, a restart) is
//! opened again without a word: the passphrase is kept for that, in memory,
//! for as long as Total Commander runs.

use std::collections::HashMap;

use serde::Deserialize;
use zeroize::Zeroizing;

/// A partition the service offers.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SourceInfo {
    pub id: String,
    pub name: String,
    pub encrypted: bool,
    pub size: u64,
}

/// A filesystem of an opened partition.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RootInfo {
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub error: Option<String>,
}

/// A file or folder.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    #[serde(default)]
    pub modified: i64,
    #[serde(default)]
    pub nanos: u32,
}

impl Entry {
    fn folder(name: &str, size: u64) -> Self {
        Entry {
            name: name.to_string(),
            dir: true,
            size,
            modified: 0,
            nanos: 0,
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Opened {
    Open {
        session: u64,
        #[serde(default)]
        encrypted: bool,
        roots: Vec<RootInfo>,
    },
    NeedsPassword,
    WrongPassword,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteError {
    /// The session is gone: open the partition again.
    SessionGone,
    Failed(String),
}

/// The service, as this module needs it.
pub trait Remote {
    fn sources(&mut self) -> Result<Vec<SourceInfo>, RemoteError>;
    fn open(&mut self, source: &str, passphrase: Option<&str>) -> Result<Opened, RemoteError>;
    fn list(&mut self, session: u64, root: usize, path: &str) -> Result<Vec<Entry>, RemoteError>;
    fn stat(&mut self, session: u64, root: usize, path: &str) -> Result<Entry, RemoteError>;
    fn read(
        &mut self,
        session: u64,
        root: usize,
        path: &str,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, RemoteError>;
}

/// The person, as this module needs them.
pub trait Ui {
    /// The passphrase of the partition `name`; `retry` after a wrong one.
    /// `None` when they cancel.
    fn passphrase(&mut self, name: &str, retry: bool) -> Option<Zeroizing<String>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseError {
    /// The person cancelled the passphrase: nothing to say about it.
    Cancelled,
    NotFound,
    Failed(String),
}

impl From<RemoteError> for BrowseError {
    fn from(e: RemoteError) -> Self {
        match e {
            RemoteError::SessionGone => BrowseError::Failed("the session has gone".into()),
            RemoteError::Failed(m) => BrowseError::Failed(m),
        }
    }
}

struct OpenSource {
    session: u64,
    roots: Vec<RootInfo>,
}

impl OpenSource {
    /// The partition is one filesystem, shown as its own contents.
    fn single(&self) -> bool {
        self.roots.len() == 1 && self.roots[0].name.is_empty()
    }
}

pub struct Browser<R: Remote> {
    remote: R,
    sources: Vec<SourceInfo>,
    open: HashMap<String, OpenSource>,
    /// Passphrases that opened a partition, by its id.
    passphrases: HashMap<String, Zeroizing<String>>,
}

/// Where in a filesystem a path leads.
struct Target {
    source: String,
    session: u64,
    root: usize,
    path: String,
}

fn parts(path: &str) -> Vec<&str> {
    path.split(['\\', '/']).filter(|p| !p.is_empty()).collect()
}

impl<R: Remote> Browser<R> {
    pub fn new(remote: R) -> Self {
        Self {
            remote,
            sources: Vec::new(),
            open: HashMap::new(),
            passphrases: HashMap::new(),
        }
    }

    pub fn remote(&mut self) -> &mut R {
        &mut self.remote
    }

    fn refresh(&mut self) -> Result<(), BrowseError> {
        self.sources = self.remote.sources()?;
        Ok(())
    }

    fn source_by_name(&mut self, name: &str) -> Result<SourceInfo, BrowseError> {
        if !self.sources.iter().any(|s| s.name == name) {
            self.refresh()?;
        }
        self.sources
            .iter()
            .find(|s| s.name == name)
            .cloned()
            .ok_or(BrowseError::NotFound)
    }

    /// Open `source`, asking for its passphrase as long as the person gives
    /// one that does not fit.
    fn open_source(&mut self, ui: &mut dyn Ui, source: &SourceInfo) -> Result<(), BrowseError> {
        if self.open.contains_key(&source.id) {
            return Ok(());
        }
        let mut pass = self.passphrases.get(&source.id).cloned();
        let mut refreshed = false;
        let mut wrong = false;
        loop {
            match self.remote.open(&source.id, pass.as_deref().map(|p| p.as_str())) {
                Ok(Opened::Open { session, roots, .. }) => {
                    if let Some(p) = pass {
                        self.passphrases.insert(source.id.clone(), p);
                    }
                    self.open
                        .insert(source.id.clone(), OpenSource { session, roots });
                    return Ok(());
                }
                Ok(Opened::NeedsPassword) => {}
                Ok(Opened::WrongPassword) => {
                    self.passphrases.remove(&source.id);
                    wrong = true;
                }
                // The service restarted and no longer knows the partition by
                // its id: it learns it again from a fresh list.
                Err(RemoteError::Failed(_)) if !refreshed => {
                    refreshed = true;
                    self.refresh()?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            pass = Some(
                ui.passphrase(&source.name, wrong)
                    .ok_or(BrowseError::Cancelled)?,
            );
        }
    }

    /// Resolve a path inside a partition; `None` for the partition itself
    /// when it is a set of logical volumes (they are its contents).
    fn target(&mut self, ui: &mut dyn Ui, path: &str) -> Result<Option<Target>, BrowseError> {
        let p = parts(path);
        let Some(first) = p.first() else {
            return Ok(None);
        };
        let source = self.source_by_name(first)?;
        self.open_source(ui, &source)?;
        let open = &self.open[&source.id];
        let (root, rest) = if open.single() {
            (0, &p[1..])
        } else {
            match p.get(1) {
                None => return Ok(None),
                Some(name) => {
                    let i = open
                        .roots
                        .iter()
                        .position(|r| r.name == *name)
                        .ok_or(BrowseError::NotFound)?;
                    if let Some(why) = &open.roots[i].error {
                        return Err(BrowseError::Failed(why.clone()));
                    }
                    (i, &p[2..])
                }
            }
        };
        Ok(Some(Target {
            source: source.id,
            session: open.session,
            root,
            path: format!("\\{}", rest.join("\\")),
        }))
    }

    /// Do `f` in the filesystem `path` leads to, opening the partition
    /// again once if its session has gone.
    fn with_target<T>(
        &mut self,
        ui: &mut dyn Ui,
        path: &str,
        mut f: impl FnMut(&mut R, &Target) -> Result<T, RemoteError>,
    ) -> Result<T, BrowseError> {
        for attempt in 0..2 {
            let t = self.target(ui, path)?.ok_or(BrowseError::NotFound)?;
            match f(&mut self.remote, &t) {
                Err(RemoteError::SessionGone) if attempt == 0 => {
                    self.open.remove(&t.source);
                }
                r => return r.map_err(Into::into),
            }
        }
        Err(BrowseError::Failed("the session has gone".into()))
    }

    /// The contents of the folder at `path`.
    pub fn list(&mut self, ui: &mut dyn Ui, path: &str) -> Result<Vec<Entry>, BrowseError> {
        let p = parts(path);
        if p.is_empty() {
            self.refresh()?;
            return Ok(self
                .sources
                .iter()
                .map(|s| Entry::folder(&s.name, s.size))
                .collect());
        }
        if p.len() == 1 {
            let source = self.source_by_name(p[0])?;
            self.open_source(ui, &source)?;
            let open = &self.open[&source.id];
            if !open.single() {
                return Ok(open
                    .roots
                    .iter()
                    .map(|r| Entry::folder(&r.name, r.size))
                    .collect());
            }
        }
        self.with_target(ui, path, |r, t| r.list(t.session, t.root, &t.path))
    }

    /// The file or folder at `path`.
    pub fn stat(&mut self, ui: &mut dyn Ui, path: &str) -> Result<Entry, BrowseError> {
        self.with_target(ui, path, |r, t| r.stat(t.session, t.root, &t.path))
    }

    /// Up to `len` bytes of the file at `path` from `offset`.
    pub fn read(
        &mut self,
        ui: &mut dyn Ui,
        path: &str,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, BrowseError> {
        self.with_target(ui, path, |r, t| r.read(t.session, t.root, &t.path, offset, len))
    }
}

/// Seconds since 1970 to a Windows FILETIME (100 ns since 1601).
pub fn filetime(seconds: i64, nanos: u32) -> u64 {
    const EPOCH_DIFF: i64 = 11_644_473_600;
    if seconds <= -EPOCH_DIFF {
        return 0;
    }
    ((seconds + EPOCH_DIFF) as u64) * 10_000_000 + u64::from(nanos / 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A service with one plain partition and one encrypted LVM partition.
    #[derive(Default)]
    struct Fake {
        opens: Vec<(String, Option<String>)>,
        next: u64,
        live: Vec<u64>,
        forget_sources: bool,
    }

    const PW: &str = "secret";

    impl Remote for Fake {
        fn sources(&mut self) -> Result<Vec<SourceInfo>, RemoteError> {
            self.forget_sources = false;
            Ok(vec![
                SourceInfo {
                    id: "d#1".into(),
                    name: "Диск 1 · раздел 1".into(),
                    encrypted: false,
                    size: 100,
                },
                SourceInfo {
                    id: "d#3".into(),
                    name: "Диск 1 · раздел 3".into(),
                    encrypted: true,
                    size: 300,
                },
            ])
        }
        fn open(&mut self, source: &str, pass: Option<&str>) -> Result<Opened, RemoteError> {
            self.opens.push((source.into(), pass.map(String::from)));
            if self.forget_sources {
                return Err(RemoteError::Failed("not in the list".into()));
            }
            self.next += 1;
            let roots = match source {
                "d#1" => vec![RootInfo {
                    name: String::new(),
                    size: 100,
                    label: String::new(),
                    error: None,
                }],
                "d#3" => match pass {
                    None => return Ok(Opened::NeedsPassword),
                    Some(p) if p != PW => return Ok(Opened::WrongPassword),
                    Some(_) => vec![
                        RootInfo {
                            name: "vg-root".into(),
                            size: 200,
                            label: String::new(),
                            error: None,
                        },
                        RootInfo {
                            name: "vg-thin".into(),
                            size: 50,
                            label: String::new(),
                            error: Some("a thin segment".into()),
                        },
                    ],
                },
                _ => return Err(RemoteError::Failed("no such".into())),
            };
            self.live.push(self.next);
            Ok(Opened::Open {
                session: self.next,
                encrypted: source == "d#3",
                roots,
            })
        }
        fn list(&mut self, session: u64, root: usize, path: &str) -> Result<Vec<Entry>, RemoteError> {
            if !self.live.contains(&session) {
                return Err(RemoteError::SessionGone);
            }
            Ok(vec![Entry {
                name: format!("{root}:{path}"),
                dir: false,
                size: 1,
                modified: 0,
                nanos: 0,
            }])
        }
        fn stat(&mut self, session: u64, root: usize, path: &str) -> Result<Entry, RemoteError> {
            self.list(session, root, path).map(|mut v| v.remove(0))
        }
        fn read(
            &mut self,
            session: u64,
            _root: usize,
            path: &str,
            offset: u64,
            _len: usize,
        ) -> Result<Vec<u8>, RemoteError> {
            if !self.live.contains(&session) {
                return Err(RemoteError::SessionGone);
            }
            Ok(format!("{path}@{offset}").into_bytes())
        }
    }

    /// Answers with these passphrases, then cancels.
    struct Person(Vec<&'static str>, Vec<bool>);
    impl Ui for Person {
        fn passphrase(&mut self, _name: &str, retry: bool) -> Option<Zeroizing<String>> {
            self.1.push(retry);
            if self.0.is_empty() {
                None
            } else {
                Some(Zeroizing::new(self.0.remove(0).to_string()))
            }
        }
    }

    #[test]
    fn the_top_lists_the_partitions_and_a_plain_one_shows_its_files() {
        let mut b = Browser::new(Fake::default());
        let mut ui = Person(vec![], vec![]);
        let top = b.list(&mut ui, "\\").unwrap();
        assert_eq!(top.len(), 2);
        assert!(top.iter().all(|e| e.dir));
        let files = b.list(&mut ui, "\\Диск 1 · раздел 1").unwrap();
        assert_eq!(files[0].name, "0:\\", "its own root, without a volume level");
        let deep = b.list(&mut ui, "\\Диск 1 · раздел 1\\home\\user").unwrap();
        assert_eq!(deep[0].name, "0:\\home\\user");
        assert!(ui.1.is_empty(), "no passphrase asked for a plain partition");
    }

    #[test]
    fn an_encrypted_partition_asks_until_the_passphrase_fits_and_remembers_it() {
        let mut b = Browser::new(Fake::default());
        let mut ui = Person(vec!["wrong", PW], vec![]);
        let vols = b.list(&mut ui, "\\Диск 1 · раздел 3").unwrap();
        let names: Vec<&str> = vols.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["vg-root", "vg-thin"]);
        assert_eq!(ui.1, [false, true], "asked, then told it was wrong");
        let inside = b.list(&mut ui, "\\Диск 1 · раздел 3\\vg-root\\etc").unwrap();
        assert_eq!(inside[0].name, "0:\\etc");
        assert!(matches!(
            b.list(&mut ui, "\\Диск 1 · раздел 3\\vg-thin"),
            Err(BrowseError::Failed(m)) if m.contains("thin")
        ));
        assert_eq!(b.list(&mut ui, "\\Диск 1 · раздел 3\\nothing"), Err(BrowseError::NotFound));

        // The service forgot the session: opened again with the kept
        // passphrase, nobody asked.
        b.remote().live.clear();
        let again = b.read(&mut ui, "\\Диск 1 · раздел 3\\vg-root\\f", 5, 10).unwrap();
        assert_eq!(again, b"\\f@5");
        assert_eq!(ui.1.len(), 2, "not asked again");
        let last = b.remote().opens.last().unwrap().clone();
        assert_eq!(last, ("d#3".to_string(), Some(PW.to_string())));
    }

    #[test]
    fn a_cancelled_passphrase_is_not_an_error_to_show() {
        let mut b = Browser::new(Fake::default());
        let mut ui = Person(vec![], vec![]);
        assert_eq!(b.list(&mut ui, "\\Диск 1 · раздел 3"), Err(BrowseError::Cancelled));
        assert_eq!(b.list(&mut ui, "\\Диск 9"), Err(BrowseError::NotFound));
    }

    /// After a restart the service knows no partition by its id until it
    /// lists them again.
    #[test]
    fn a_restarted_service_learns_the_partitions_again() {
        let mut b = Browser::new(Fake::default());
        let mut ui = Person(vec![], vec![]);
        b.list(&mut ui, "\\").unwrap();
        b.remote().forget_sources = true;
        let files = b.list(&mut ui, "\\Диск 1 · раздел 1").unwrap();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn times_become_filetimes() {
        assert_eq!(filetime(0, 0), 116_444_736_000_000_000);
        assert_eq!(filetime(1, 500), 116_444_736_010_000_005);
        assert_eq!(filetime(-20_000_000_000, 0), 0, "before 1601");
    }
}
