//! Building the Linux boot medium in the service (PLAN §6, stage 6): the
//! decisions, apart from the processes and files (`media_linux_build`), so
//! they can be tested anywhere.
//!
//! The build itself is `packaging/linux/media/build-media.sh` — the script
//! `scripts/build-linux-media.sh` runs from the checkout — installed as
//! `/usr/libexec/own-backup/build-linux-media`. It says what it is doing in
//! lines of its own (`OBK-STAGE`, `OBK-ERROR`, `OBK-ISO`, see the script's
//! header) between the tools' output; the later stages and the percentages
//! are read from what live-build and apt print anyway: live-build names
//! every step it starts (`[2026-10-20 19:04:10] lb chroot`), apt says how many
//! packages it installs and then names each one it sets up, xorriso counts
//! its percentage. Nothing here decides anything from a translated text: the
//! script's codes and the tools' untranslated output (`LANG=C.UTF-8` in the
//! builder) are what is read.

use std::collections::VecDeque;

use own_backup_common::tr;

/// The build's peak in the cache directory: the builder tree (~0.5 GiB),
/// live-build's chroot and binary trees (~4 GiB), the package cache
/// (~1.5 GiB), the ISO (~0.7 GiB) — generous on purpose.
pub const PEAK_BYTES: u64 = 10 << 30;

/// The least free space a build starts with, however much of the peak a
/// cache from an earlier build already holds: the chroot and binary trees are
/// rebuilt every time.
pub const MIN_FREE_BYTES: u64 = 6 << 30;

/// The least free space in the folder the ISO goes into (it is about 0.7 GiB).
pub const DEST_MIN_BYTES: u64 = 1 << 30;

/// How many of the tools' last lines a failure is judged by.
const TAIL_LINES: usize = 60;

/// The stages of a build, in order; the stick's three come last when the
/// medium is also written onto a stick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxStage {
    /// The installed program packed into a .deb, the Debian builder tree
    /// made or updated, the package checked against Debian 13.
    Builder,
    /// live-build's bootstrap: Debian's base system downloaded and unpacked.
    Download,
    /// live-build's chroot: the medium's packages installed and configured.
    System,
    /// live-build's binary: squashfs, boot loaders, the ISO — and the copy
    /// into the person's folder.
    Iso,
    Write,
    Verify,
    Logs,
}

impl LinuxStage {
    /// The stages a build goes through.
    pub fn plan(stick: bool) -> Vec<LinuxStage> {
        let mut out = vec![
            LinuxStage::Builder,
            LinuxStage::Download,
            LinuxStage::System,
            LinuxStage::Iso,
        ];
        if stick {
            out.extend([LinuxStage::Write, LinuxStage::Verify, LinuxStage::Logs]);
        }
        out
    }

    pub fn id(self) -> &'static str {
        match self {
            LinuxStage::Builder => "builder",
            LinuxStage::Download => "download",
            LinuxStage::System => "system",
            LinuxStage::Iso => "iso",
            LinuxStage::Write => "write",
            LinuxStage::Verify => "verify",
            LinuxStage::Logs => "logs",
        }
    }

    pub fn title(self) -> String {
        match self {
            LinuxStage::Builder => tr!("media_linux.stage_builder"),
            LinuxStage::Download => tr!("media_linux.stage_download"),
            LinuxStage::System => tr!("media_linux.stage_system"),
            LinuxStage::Iso => tr!("media_linux.stage_iso"),
            LinuxStage::Write => tr!("media_stick.stage_write"),
            LinuxStage::Verify => tr!("media_stick.stage_verify"),
            LinuxStage::Logs => tr!("media_stick.stage_logs"),
        }
        .into_owned()
    }
}

/// One line of the build script's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptLine {
    Stage(LinuxStage),
    Version(String),
    /// apt's reason, when the package does not install on Debian 13.
    Apt(String),
    Error {
        code: String,
        detail: String,
    },
    Iso(String),
    /// Anything else: a tool's own line.
    Tool(String),
}

pub fn parse_line(line: &str) -> ScriptLine {
    let Some(rest) = line.strip_prefix("OBK-") else {
        return ScriptLine::Tool(line.to_string());
    };
    let (word, arg) = rest.split_once(' ').unwrap_or((rest, ""));
    match word {
        "STAGE" => match arg.trim() {
            "builder" => ScriptLine::Stage(LinuxStage::Builder),
            "download" => ScriptLine::Stage(LinuxStage::Download),
            "system" => ScriptLine::Stage(LinuxStage::System),
            "iso" => ScriptLine::Stage(LinuxStage::Iso),
            _ => ScriptLine::Tool(line.to_string()),
        },
        "VERSION" => ScriptLine::Version(arg.trim().to_string()),
        "APT" => ScriptLine::Apt(arg.to_string()),
        "ERROR" => {
            let (code, detail) = arg.trim().split_once(' ').unwrap_or((arg.trim(), ""));
            ScriptLine::Error {
                code: code.to_string(),
                detail: detail.trim().to_string(),
            }
        }
        "ISO" => ScriptLine::Iso(arg.trim().to_string()),
        _ => ScriptLine::Tool(line.to_string()),
    }
}

/// The step live-build announces on `line` (`[2026-10-20 19:04:10] lb chroot`
/// → `chroot`, `… lb binary_rootfs ` → `binary_rootfs`).
pub fn lb_step(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let (stamp, rest) = rest.split_once(']')?;
    // "2026-10-20 19:04:10": digits, dashes, colons and one space.
    if stamp.len() != 19
        || !stamp
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b == b':' || b == b' ')
    {
        return None;
    }
    rest.trim_start()
        .strip_prefix("lb ")?
        .split_whitespace()
        .next()
}

/// What the window shows of a build: the stage, its percentage (0 while it
/// cannot be measured — the window animates that) and a word about what is
/// happening in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub stage: LinuxStage,
    pub percent: u64,
    pub detail: String,
}

/// Follows the script's output and turns it into [`Progress`].
#[derive(Debug, Default)]
pub struct Tracker {
    stage: Option<LinuxStage>,
    percent: u64,
    detail: String,
    /// Packages apt said it installs in its current run (0 before it says).
    total: u64,
    fetched: u64,
    set_up: u64,
    pub version: Option<String>,
    pub iso: Option<String>,
    pub error: Option<(String, String)>,
    pub apt: Vec<String>,
    tail: VecDeque<String>,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    fn progress(&self) -> Option<Progress> {
        self.stage.map(|stage| Progress {
            stage,
            percent: self.percent,
            detail: self.detail.clone(),
        })
    }

    fn enter(&mut self, stage: LinuxStage) {
        if self.stage != Some(stage) {
            self.stage = Some(stage);
            self.percent = 0;
            self.detail.clear();
            self.total = 0;
            self.fetched = 0;
            self.set_up = 0;
        }
    }

    /// Never backwards within a stage: apt's second, smaller run must not
    /// pull the bar back to where its first one was.
    fn at_least(&mut self, percent: u64) {
        self.percent = self.percent.max(percent.min(99));
    }

    /// One line of output. `Some` when what the window shows has changed.
    pub fn feed(&mut self, line: &str) -> Option<Progress> {
        let before = self.progress();
        match parse_line(line) {
            ScriptLine::Stage(stage) => self.enter(stage),
            ScriptLine::Version(v) => self.version = Some(v),
            ScriptLine::Apt(l) => self.apt.push(l),
            ScriptLine::Error { code, detail } => self.error = Some((code, detail)),
            ScriptLine::Iso(path) => self.iso = Some(path),
            ScriptLine::Tool(l) => {
                self.tool_line(&l);
                if self.tail.len() == TAIL_LINES {
                    self.tail.pop_front();
                }
                self.tail.push_back(l);
            }
        }
        let now = self.progress();
        (now != before).then_some(now).flatten()
    }

    fn tool_line(&mut self, line: &str) {
        if let Some(step) = lb_step(line) {
            match step {
                "bootstrap" => self.enter(LinuxStage::Download),
                "chroot" | "installer" if self.stage != Some(LinuxStage::Iso) => {
                    self.enter(LinuxStage::System)
                }
                "binary" => self.enter(LinuxStage::Iso),
                _ => {}
            }
            if self.stage == Some(LinuxStage::Iso) {
                let (percent, detail) = match step {
                    "binary_chroot" => (2, String::new()),
                    "binary_rootfs" => (5, tr!("media_linux.detail_squashfs").into_owned()),
                    "binary_manifest" => (60, String::new()),
                    "binary_grub-efi" | "binary_syslinux" => {
                        (65, tr!("media_linux.detail_boot").into_owned())
                    }
                    "binary_checksums" => (75, String::new()),
                    "binary_iso" => (80, tr!("media_linux.detail_iso").into_owned()),
                    "binary_onie" | "binary_netboot" | "binary_tar" | "binary_hdd" => {
                        (96, String::new())
                    }
                    _ => return,
                };
                self.at_least(percent);
                self.detail = detail;
            }
            return;
        }
        // debootstrap: "I: Retrieving bash 5.2.37-2+b10", "I: Unpacking …".
        if let Some(rest) = line.strip_prefix("I: ") {
            for verb in ["Retrieving ", "Extracting ", "Unpacking ", "Configuring "] {
                if let Some(what) = rest.strip_prefix(verb) {
                    let name = what.split_whitespace().next().unwrap_or("");
                    // "Retrieving InRelease", "Retrieving Packages": indexes.
                    if !name.is_empty() && name.starts_with(|c: char| c.is_ascii_lowercase()) {
                        self.detail = name.to_string();
                    }
                    return;
                }
            }
            return;
        }
        // apt: "0 upgraded, 734 newly installed, 0 to remove and 0 not upgraded."
        if let Some((upgraded, rest)) = line.split_once(" upgraded, ") {
            if let (Ok(u), Some(n)) = (
                upgraded.trim().parse::<u64>(),
                rest.split_once(" newly installed")
                    .and_then(|(n, _)| n.trim().parse::<u64>().ok()),
            ) {
                self.total = u + n;
                self.fetched = 0;
                self.set_up = 0;
            }
            return;
        }
        if self.stage == Some(LinuxStage::System) && self.total > 0 {
            // "Get:12 http://deb.debian.org/debian trixie/main amd64 libfoo1 amd64 1.2-3 [55.1 kB]"
            if let Some(rest) = line.strip_prefix("Get:") {
                if rest.contains(" amd64 ") || rest.contains(" all ") {
                    self.fetched += 1;
                    if let Some(name) = apt_get_package(rest) {
                        self.detail = name.to_string();
                    }
                    self.measure();
                }
                return;
            }
            // "Setting up libfoo1:amd64 (1.2-3) ..."
            if let Some(rest) = line.strip_prefix("Setting up ") {
                self.set_up += 1;
                let name = rest.split_whitespace().next().unwrap_or("");
                self.detail = name.split(':').next().unwrap_or(name).to_string();
                self.measure();
            }
            return;
        }
        // xorriso: "xorriso : UPDATE :  21.41% done"
        if self.stage == Some(LinuxStage::Iso) {
            if let Some(rest) = line.strip_prefix("xorriso : UPDATE :") {
                if let Some(pct) = rest
                    .trim()
                    .strip_suffix("% done")
                    .and_then(|p| p.trim().parse::<f64>().ok())
                {
                    self.at_least(80 + (pct.clamp(0.0, 100.0) * 0.15) as u64);
                }
            }
        }
    }

    /// apt downloads first and sets up after: 40 % of the stage for the one,
    /// 60 % for the other.
    fn measure(&mut self) {
        if self.total > 0 {
            let done = self.fetched.min(self.total) * 40 + self.set_up.min(self.total) * 60;
            self.at_least(done / self.total);
        }
    }

    /// The tools' last lines, oldest first.
    pub fn tail(&self) -> impl Iterator<Item = &str> {
        self.tail.iter().map(String::as_str)
    }
}

/// The package name of an apt `Get:` line's rest (after `Get:`): the word
/// before the architecture.
fn apt_get_package(rest: &str) -> Option<&str> {
    let words: Vec<&str> = rest.split_whitespace().collect();
    // N url suite/component arch NAME arch VERSION [size]
    words
        .windows(2)
        .rev()
        .find(|w| w[1] == "amd64" || w[1] == "all")
        .map(|w| w[0])
        .filter(|w| !w.contains('/'))
}

/// Why the tools failed, judged by their last lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolFailure {
    /// The mirror (this host) did not answer.
    Network(String),
    NoSpace,
    Other,
}

/// The host of a URL (`http://deb.debian.org/debian/` → `deb.debian.org`).
pub fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split('/').next().unwrap_or(rest);
    host.split(':').next().unwrap_or(host)
}

/// Network failures as debootstrap, wget and apt word them.
pub fn classify_failure<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    mirror_host: &str,
) -> ToolFailure {
    let mut found = ToolFailure::Other;
    for line in lines {
        if line.contains("No space left on device") {
            return ToolFailure::NoSpace;
        }
        let lower = line.to_ascii_lowercase();
        let network = [
            "temporary failure resolving",
            "could not resolve",
            "unable to resolve host",
            "could not connect to",
            "unable to connect to",
            "connection timed out",
            "network is unreachable",
            "no route to host",
            "connection refused",
            "failed to fetch",
            "failed getting release file",
            "couldn't download",
        ]
        .iter()
        .any(|p| lower.contains(p));
        if network {
            let host = line
                .split(|c: char| c.is_whitespace() || c == '\'' || c == '"')
                .find(|w| w.starts_with("http://") || w.starts_with("https://"))
                .map(url_host)
                .or_else(|| {
                    // "Temporary failure resolving 'deb.debian.org'"
                    line.split('\'').nth(1).filter(|h| h.contains('.'))
                })
                .unwrap_or(mirror_host);
            found = ToolFailure::Network(host.to_string());
        }
    }
    found
}

/// The person's words for the script's own refusal `code`.
pub fn refusal(code: &str, detail: &str, apt: &[String]) -> String {
    match code {
        "missing-tool" | "missing-keyring" => {
            tr!("media_linux.missing_tools", what = detail).into_owned()
        }
        "network" => tr!("media_linux.network", host = detail).into_owned(),
        "not-root" => tr!("media_linux.not_root").into_owned(),
        "arch" => tr!("media_linux.arch", arch = detail).into_owned(),
        "bad-deb" => tr!("media_linux.bad_deb").into_owned(),
        "apt-check" => tr!("media_linux.apt_check", reason = apt_reason(apt)).into_owned(),
        "no-iso" => tr!("media_linux.no_iso").into_owned(),
        other => tr!("media_linux.script_error", code = other).into_owned(),
    }
}

/// apt's reason in a few lines: the unmet dependencies when it lists them,
/// otherwise its errors, otherwise its last lines.
pub fn apt_reason(apt: &[String]) -> String {
    let pick = |f: &dyn Fn(&str) -> bool| -> Vec<String> {
        apt.iter()
            .map(|l| l.trim())
            .filter(|l| f(l))
            .map(str::to_string)
            .collect()
    };
    let mut lines = pick(&|l| l.contains("Depends:") || l.contains("Breaks:"));
    if lines.is_empty() {
        lines = pick(&|l| l.starts_with("E: "));
    }
    if lines.is_empty() {
        lines = pick(&|l| !l.is_empty());
        let skip = lines.len().saturating_sub(4);
        lines.drain(..skip);
    }
    lines.truncate(6);
    lines.join("; ")
}

/// The space a build needs free before it starts, when `cached` bytes of the
/// peak are already there (the builder and the package cache).
pub fn need_bytes(cached: u64) -> u64 {
    PEAK_BYTES.saturating_sub(cached).max(MIN_FREE_BYTES)
}

/// Bytes in GiB with one decimal, the way the refusal shows them.
pub fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1u64 << 30) as f64)
}

/// The upstream part of a Debian version: no epoch, no revision
/// (`1:0.1.5~beta-3` → `0.1.5~beta`).
pub fn upstream_version(version: &str) -> &str {
    let v = version.split_once(':').map_or(version, |(_, v)| v);
    v.rsplit_once('-').map_or(v, |(u, _)| u)
}

/// The ISO's file name: `OwnBackup-Linux-0.1.5~beta.iso` for the first, then
/// `OwnBackup-Linux-0.1.5~beta (2).iso` and so on — an existing file is never
/// replaced.
pub fn iso_file_name(version: &str, n: u32) -> String {
    let stem = format!("OwnBackup-Linux-{}", upstream_version(version));
    let stem: String = stem
        .chars()
        .map(|c| if c == '/' || c.is_control() { '_' } else { c })
        .collect();
    if n <= 1 {
        format!("{stem}.iso")
    } else {
        format!("{stem} ({n}).iso")
    }
}

// --- the ready ISO on GitHub ---------------------------------------------------------------

/// Where the window looks for a ready medium: this repository's releases.
pub const GITHUB_REPO: &str = "coinman-dev/own-backup";

/// The releases, newest first, as GitHub's API lists them.
pub fn github_releases_url() -> String {
    format!("https://api.github.com/repos/{GITHUB_REPO}/releases?per_page=100")
}

/// The name a ready medium of `version` has among a release's assets:
/// `OwnBackup-Linux-0.1.5.beta-4_amd64.iso` for the package `0.1.5~beta-4`.
/// The whole Debian version, revision included: the medium carries exactly
/// one build of the program. Characters GitHub does not keep in an asset's
/// name (`~`, `+`, `:`) are dots, as GitHub itself would make them; the epoch
/// is dropped.
pub fn release_asset_name(version: &str) -> String {
    let v = version.split_once(':').map_or(version, |(_, v)| v);
    let v: String = v
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '.'
            }
        })
        .collect();
    format!("OwnBackup-Linux-{v}_amd64.iso")
}

/// A ready medium found in a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseIso {
    pub tag: String,
    pub name: String,
    pub size: u64,
    pub url: String,
    /// The release's checksum lists (`SHA256SUMS`, `SHA256SUMS.txt` — the
    /// Windows installer's list is the latter); one of them must name the ISO.
    pub sums_urls: Vec<String>,
}

/// The newest published (not draft) release in GitHub's answer that has
/// the medium of `version`. `None` when no release has it.
pub fn find_release_iso(releases: &serde_json::Value, version: &str) -> Option<ReleaseIso> {
    let want = release_asset_name(version);
    releases.as_array()?.iter().find_map(|r| {
        if r.get("draft").and_then(|d| d.as_bool()) == Some(true) {
            return None;
        }
        let assets = r.get("assets")?.as_array()?;
        fn name_of(a: &serde_json::Value) -> Option<&str> {
            a.get("name").and_then(|n| n.as_str())
        }
        let url_of = |a: &serde_json::Value| {
            a.get("browser_download_url")
                .and_then(|u| u.as_str())
                .filter(|u| u.starts_with("https://"))
                .map(str::to_string)
        };
        let iso = assets.iter().find(|a| name_of(a) == Some(want.as_str()))?;
        let sums_urls = assets
            .iter()
            .filter(|a| matches!(name_of(a), Some("SHA256SUMS" | "SHA256SUMS.txt")))
            .filter_map(url_of)
            .collect();
        Some(ReleaseIso {
            tag: r.get("tag_name")?.as_str()?.to_string(),
            name: want.clone(),
            size: iso.get("size")?.as_u64()?,
            url: url_of(iso)?,
            sums_urls,
        })
    })
}

/// The Debian upstream version of a program version: `0.1.6-beta` →
/// `0.1.6~beta`, as `scripts/build-deb.sh` makes it (the first `-` only).
pub fn deb_upstream(version: &str) -> String {
    version.replacen('-', "~", 1)
}

/// Windows: the medium of the program version `upstream` (`0.1.6~beta`),
/// whose package revision the Windows program does not know. The newest
/// release with a medium of that version is taken. A release's own package
/// has no revision (`0.1.8~beta`, from 0.1.8-beta on), and that medium is
/// preferred; otherwise the highest revision there (`0.1.6~beta-1`, as
/// releases up to 0.1.7-beta and test builds are named). The answer is the
/// whole Debian version and the release, as [`find_release_iso`] finds it.
pub fn find_release_iso_of_upstream(
    releases: &serde_json::Value,
    upstream: &str,
) -> Option<(String, ReleaseIso)> {
    let bare = release_asset_name(upstream);
    let prefix = format!("{}-", bare.strip_suffix("_amd64.iso")?);
    let version = releases.as_array()?.iter().find_map(|r| {
        if r.get("draft").and_then(|d| d.as_bool()) == Some(true) {
            return None;
        }
        let names = r
            .get("assets")?
            .as_array()?
            .iter()
            .filter_map(|a| a.get("name")?.as_str());
        if names.clone().any(|n| n == bare) {
            return Some(upstream.to_string());
        }
        names
            .filter_map(|n| n.strip_prefix(prefix.as_str())?.strip_suffix("_amd64.iso"))
            .filter(|rev| {
                rev.starts_with(|c: char| c.is_ascii_digit())
                    && rev.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
            })
            .max_by_key(|rev| revision_key(rev))
            .map(|rev| format!("{upstream}-{rev}"))
    })?;
    find_release_iso(releases, &version).map(|r| (version, r))
}

/// The ready medium of the newest release that has one, whatever version is
/// installed here (owner, 29.10): a test build's own revision is never
/// released, and a newer medium reads what an older program wrote. Drafts
/// are skipped; the newest is the latest `published_at`, not GitHub's order.
/// The version is the release tag's in Debian's spelling (`v0.1.8-beta` →
/// `0.1.8~beta`), with the medium's revision when it has one, as
/// [`find_release_iso_of_upstream`] picks it.
pub fn find_latest_release_iso(releases: &serde_json::Value) -> Option<(String, ReleaseIso)> {
    let mut published: Vec<&serde_json::Value> = releases
        .as_array()?
        .iter()
        .filter(|r| r.get("draft").and_then(|d| d.as_bool()) != Some(true))
        .collect();
    let date = |r: &serde_json::Value| {
        r.get("published_at")
            .and_then(|d| d.as_str())
            .unwrap_or_default()
            .to_string()
    };
    // Stable: releases published at the same moment keep GitHub's order.
    published.sort_by_key(|r| std::cmp::Reverse(date(r)));
    published.into_iter().find_map(|r| {
        let tag = r.get("tag_name")?.as_str()?;
        let upstream = deb_upstream(tag.strip_prefix('v').unwrap_or(tag));
        find_release_iso_of_upstream(&serde_json::Value::Array(vec![r.clone()]), &upstream)
    })
}

/// Revisions compared by their leading number: `10` after `9`.
fn revision_key(rev: &str) -> (u64, String) {
    let digits: String = rev.chars().take_while(|c| c.is_ascii_digit()).collect();
    (digits.parse().unwrap_or(0), rev.to_string())
}

/// The SHA-256 a checksum list (`sha256sum`'s lines, text or binary mode)
/// gives for `name`, in lower case.
pub fn sha256_listed(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.trim_end().split_once(char::is_whitespace)?;
        let file = file.trim_start().trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The newest published release with a medium wins, whatever is
    /// installed: drafts and releases without one are passed over, and the
    /// date decides, not the order of GitHub's answer.
    #[test]
    fn the_latest_release_with_a_medium_is_found() {
        let asset = |name: &str| {
            serde_json::json!({
                "name": name,
                "size": 7,
                "browser_download_url": format!("https://github.com/x/releases/download/t/{name}"),
            })
        };
        let releases = serde_json::json!([
            { "tag_name": "v0.1.9-beta", "draft": true, "published_at": null,
              "assets": [asset("OwnBackup-Linux-0.1.9.beta_amd64.iso")] },
            { "tag_name": "v0.1.6-beta", "draft": false, "published_at": "2026-10-10T10:00:00Z",
              "assets": [asset("OwnBackup-Linux-0.1.6.beta-1_amd64.iso"), asset("SHA256SUMS")] },
            { "tag_name": "v0.1.8-beta", "draft": false, "published_at": "2026-10-28T10:00:00Z",
              "assets": [asset("OwnBackup-Linux-0.1.8.beta_amd64.iso"), asset("SHA256SUMS")] },
            { "tag_name": "v0.1.8-hotfix", "draft": false, "published_at": "2026-10-29T10:00:00Z",
              "assets": [asset("OwnBackup-Setup.exe")] },
        ]);
        let (version, r) = find_latest_release_iso(&releases).unwrap();
        assert_eq!(version, "0.1.8~beta");
        assert_eq!(r.tag, "v0.1.8-beta");
        assert_eq!(r.name, "OwnBackup-Linux-0.1.8.beta_amd64.iso");
        assert_eq!(r.sums_urls.len(), 1);

        // Only an older release with a revised medium: that one, revision and all.
        let older = serde_json::json!([releases[1].clone()]);
        let (version, r) = find_latest_release_iso(&older).unwrap();
        assert_eq!(version, "0.1.6~beta-1");
        assert_eq!(r.name, "OwnBackup-Linux-0.1.6.beta-1_amd64.iso");

        assert!(find_latest_release_iso(&serde_json::json!([releases[3].clone()])).is_none());
    }

    #[test]
    fn windows_takes_the_highest_revision_of_its_version_in_the_newest_release() {
        assert_eq!(deb_upstream("0.1.6-beta"), "0.1.6~beta");
        assert_eq!(deb_upstream("0.2.0"), "0.2.0");
        let asset = |name: &str, size: u64| {
            serde_json::json!({
                "name": name,
                "size": size,
                "browser_download_url": format!("https://github.com/x/releases/download/t/{name}"),
            })
        };
        let releases = serde_json::json!([
            { "tag_name": "draft", "draft": true,
              "assets": [asset("OwnBackup-Linux-0.1.6.beta-12_amd64.iso", 1)] },
            { "tag_name": "v0.1.7-beta", "draft": false,
              "assets": [asset("OwnBackup-Linux-0.1.7.beta-1_amd64.iso", 2)] },
            { "tag_name": "v0.1.6-beta", "draft": false,
              "assets": [asset("OwnBackup-Linux-0.1.6.beta-2_amd64.iso", 3),
                         asset("OwnBackup-Linux-0.1.6.beta-10_amd64.iso", 4),
                         asset("OwnBackup-Linux-0.1.6.beta-9_amd64.iso", 5),
                         asset("OwnBackup-Linux-0.1.6.beta-x_amd64.iso", 6),
                         asset("own-backup_0.1.6.beta-11_amd64.deb", 7),
                         asset("SHA256SUMS.txt", 90)] },
            { "tag_name": "older", "draft": false,
              "assets": [asset("OwnBackup-Linux-0.1.6.beta-11_amd64.iso", 8)] },
        ]);
        let (version, found) = find_release_iso_of_upstream(&releases, "0.1.6~beta").unwrap();
        assert_eq!(version, "0.1.6~beta-10");
        assert_eq!(found.tag, "v0.1.6-beta");
        assert_eq!(found.size, 4);
        assert_eq!(found.sums_urls.len(), 1);
        // Not a prefix match: 0.1.6~beta is not 0.1.6~beta2.
        let other = serde_json::json!([{ "tag_name": "t", "draft": false,
            "assets": [asset("OwnBackup-Linux-0.1.6.beta2-1_amd64.iso", 1)] }]);
        assert!(find_release_iso_of_upstream(&other, "0.1.6~beta").is_none());
        assert!(find_release_iso_of_upstream(&releases, "0.1.8~beta").is_none());
    }

    /// From 0.1.8-beta a release's package has no revision, and neither has
    /// its medium's name; a test build beside it (`-2`) is not taken instead.
    #[test]
    fn a_release_medium_without_a_revision_is_found_and_preferred() {
        let asset = |name: &str, size: u64| {
            serde_json::json!({
                "name": name,
                "size": size,
                "browser_download_url": format!("https://github.com/x/releases/download/t/{name}"),
            })
        };
        let releases = serde_json::json!([
            { "tag_name": "v0.1.8-beta", "draft": false,
              "assets": [asset("own-backup-install_0.1.8-beta.exe", 1),
                         asset("own-backup_0.1.8.beta_amd64.deb", 2),
                         asset("OwnBackup-Linux-0.1.8.beta-2_amd64.iso", 3),
                         asset("OwnBackup-Linux-0.1.8.beta_amd64.iso", 4),
                         asset("SHA256SUMS.txt", 90)] },
            { "tag_name": "v0.1.7-beta", "draft": false,
              "assets": [asset("OwnBackup-Linux-0.1.7.beta-1_amd64.iso", 5)] },
        ]);
        let (version, found) = find_release_iso_of_upstream(&releases, "0.1.8~beta").unwrap();
        assert_eq!(version, "0.1.8~beta");
        assert_eq!(found.name, "OwnBackup-Linux-0.1.8.beta_amd64.iso");
        assert_eq!(found.size, 4);
        // The installed package of that release finds the same file.
        assert_eq!(find_release_iso(&releases, "0.1.8~beta").unwrap().size, 4);
        // Older releases keep their revision.
        let (version, _) = find_release_iso_of_upstream(&releases, "0.1.7~beta").unwrap();
        assert_eq!(version, "0.1.7~beta-1");
    }

    #[test]
    fn a_ready_medium_is_the_exact_build_in_the_newest_release_that_has_it() {
        assert_eq!(
            release_asset_name("0.1.5~beta-4"),
            "OwnBackup-Linux-0.1.5.beta-4_amd64.iso"
        );
        assert_eq!(
            release_asset_name("1:2.0+git1-1"),
            "OwnBackup-Linux-2.0.git1-1_amd64.iso"
        );
        let asset = |name: &str, size: u64| {
            serde_json::json!({
                "name": name,
                "size": size,
                "browser_download_url": format!("https://github.com/x/releases/download/t/{name}"),
            })
        };
        let releases = serde_json::json!([
            { "tag_name": "draft", "draft": true,
              "assets": [asset("OwnBackup-Linux-0.1.5.beta-4_amd64.iso", 1)] },
            { "tag_name": "other", "draft": false,
              "assets": [asset("OwnBackup-Linux-0.1.5.beta-3_amd64.iso", 2)] },
            { "tag_name": "linux-test-0.1.5-beta.4", "draft": false,
              "assets": [asset("own-backup_0.1.5.beta-4_amd64.deb", 3),
                         asset("OwnBackup-Linux-0.1.5.beta-4_amd64.iso", 600),
                         asset("SHA256SUMS.txt", 90),
                         asset("SHA256SUMS", 100)] },
            { "tag_name": "older", "draft": false,
              "assets": [asset("OwnBackup-Linux-0.1.5.beta-4_amd64.iso", 5)] },
        ]);
        let found = find_release_iso(&releases, "0.1.5~beta-4").unwrap();
        assert_eq!(found.tag, "linux-test-0.1.5-beta.4");
        assert_eq!(found.size, 600);
        assert!(found
            .url
            .ends_with("/OwnBackup-Linux-0.1.5.beta-4_amd64.iso"));
        assert_eq!(found.sums_urls.len(), 2);
        assert!(found.sums_urls.iter().any(|u| u.ends_with("/SHA256SUMS")));
        assert!(found
            .sums_urls
            .iter()
            .any(|u| u.ends_with("/SHA256SUMS.txt")));
        assert!(find_release_iso(&releases, "0.1.5~beta-5").is_none());
        // GitHub's answer for a private repository, or anything else.
        assert!(
            find_release_iso(&serde_json::json!({"message": "Not Found"}), "0.1.5~beta-4")
                .is_none()
        );
    }

    #[test]
    fn the_checksum_of_the_named_file_is_read_from_the_list() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!("{a}  own-backup.deb\n{b} *OwnBackup-Linux-1_amd64.iso\nshort  x.iso\n");
        assert_eq!(sha256_listed(&sums, "own-backup.deb"), Some(a));
        assert_eq!(
            sha256_listed(&sums, "OwnBackup-Linux-1_amd64.iso"),
            Some("b".repeat(64))
        );
        assert_eq!(sha256_listed(&sums, "x.iso"), None);
        assert_eq!(sha256_listed(&sums, "missing.iso"), None);
    }

    fn feed_all(t: &mut Tracker, text: &str) -> Vec<Progress> {
        text.lines().filter_map(|l| t.feed(l)).collect()
    }

    #[test]
    fn the_script_s_own_lines_are_read_and_the_rest_is_the_tools() {
        assert_eq!(
            parse_line("OBK-STAGE builder"),
            ScriptLine::Stage(LinuxStage::Builder)
        );
        assert_eq!(
            parse_line("OBK-ERROR missing-tool debootstrap"),
            ScriptLine::Error {
                code: "missing-tool".into(),
                detail: "debootstrap".into()
            }
        );
        assert_eq!(
            parse_line("OBK-ERROR apt-check"),
            ScriptLine::Error {
                code: "apt-check".into(),
                detail: String::new()
            }
        );
        assert_eq!(
            parse_line("OBK-ISO /var/cache/own-backup/media/builder/build/x.hybrid.iso"),
            ScriptLine::Iso("/var/cache/own-backup/media/builder/build/x.hybrid.iso".into())
        );
        assert_eq!(
            parse_line("OBK-VERSION 0.1.5~beta-1"),
            ScriptLine::Version("0.1.5~beta-1".into())
        );
        assert_eq!(
            parse_line("OBK-APT  own-backup : Depends: libc6 (>= 2.42)"),
            ScriptLine::Apt(" own-backup : Depends: libc6 (>= 2.42)".into())
        );
        assert_eq!(
            parse_line("OBK-STAGE nonsense"),
            ScriptLine::Tool("OBK-STAGE nonsense".into())
        );
        assert_eq!(
            parse_line("I: Retrieving apt"),
            ScriptLine::Tool("I: Retrieving apt".into())
        );
    }

    #[test]
    fn live_build_steps_are_read_from_their_time_stamped_lines() {
        assert_eq!(lb_step("[2026-10-20 19:04:10] lb chroot "), Some("chroot"));
        assert_eq!(
            lb_step("[2026-10-20 19:08:48] lb binary_rootfs "),
            Some("binary_rootfs")
        );
        assert_eq!(
            lb_step("[2026-10-20 19:04:10] lb chroot_prep install all mode-archives-chroot"),
            Some("chroot_prep")
        );
        assert_eq!(lb_step("P: Begin unmounting /sys..."), None);
        assert_eq!(lb_step("[linux-media] lb config"), None);
        assert_eq!(lb_step("[2026-10-20] lb chroot"), None);
    }

    /// Excerpts of a real build's output (tmp/linux-audit/media-build.log,
    /// 20.10), in order: what the window would show at each change.
    #[test]
    fn a_build_walks_through_its_stages_with_details_and_percentages() {
        let mut t = Tracker::new();
        let shown = feed_all(
            &mut t,
            "OBK-VERSION 0.1.5~beta-3\n\
             OBK-STAGE builder\n\
             I: Retrieving InRelease \n\
             I: Retrieving bash 5.2.37-2+b10\n\
             OBK-STAGE download\n\
             [2026-10-20 19:01:51] lb build noauto\n\
             [2026-10-20 19:01:51] lb bootstrap \n\
             I: Retrieving adduser 3.152\n\
             [2026-10-20 19:04:10] lb chroot \n\
             Get:6 http://security.debian.org/debian-security trixie-security InRelease [43.4 kB]\n\
             0 upgraded, 4 newly installed, 0 to remove and 0 not upgraded.\n\
             Get:1 http://deb.debian.org/debian trixie/main amd64 liblzo2-2 amd64 2.10-3+b1 [55.1 kB]\n\
             Get:2 http://deb.debian.org/debian trixie/main amd64 squashfs-tools amd64 1:4.6.1-1 [194 kB]\n\
             Setting up liblzo2-2:amd64 (2.10-3+b1) ...\n\
             0 upgraded, 1 newly installed, 0 to remove and 0 not upgraded.\n\
             [2026-10-20 19:08:10] lb installer \n\
             [2026-10-20 19:08:32] lb binary \n\
             [2026-10-20 19:08:32] lb chroot_prep install devpts proc selinuxfs sysfs\n\
             [2026-10-20 19:08:48] lb binary_rootfs \n\
             xorriso : UPDATE :     305 files added in 1 seconds\n\
             [2026-10-20 19:10:10] lb binary_iso \n\
             xorriso : UPDATE :  21.41% done\n\
             OBK-ISO /w/builder/build/own-backup-media-amd64.hybrid.iso",
        );
        let simple: Vec<(&str, u64, &str)> = shown
            .iter()
            .map(|p| (p.stage.id(), p.percent, p.detail.as_str()))
            .collect();
        let squashfs = tr!("media_linux.detail_squashfs");
        let iso = tr!("media_linux.detail_iso");
        assert_eq!(
            simple,
            vec![
                ("builder", 0, ""),
                ("builder", 0, "bash"),
                ("download", 0, ""),
                ("download", 0, "adduser"),
                ("system", 0, ""),
                ("system", 10, "liblzo2-2"),
                ("system", 20, "squashfs-tools"),
                ("system", 35, "liblzo2-2"),
                // apt's second run: the bar holds, the word moves on.
                ("iso", 0, ""),
                ("iso", 5, squashfs.as_ref()),
                ("iso", 80, iso.as_ref()),
                ("iso", 83, iso.as_ref()),
            ]
        );
        assert_eq!(t.version.as_deref(), Some("0.1.5~beta-3"));
        assert_eq!(
            t.iso.as_deref(),
            Some("/w/builder/build/own-backup-media-amd64.hybrid.iso")
        );
        assert!(t.error.is_none());
    }

    #[test]
    fn the_installer_and_binary_steps_never_send_the_bar_back_to_the_system() {
        let mut t = Tracker::new();
        feed_all(
            &mut t,
            "[2026-10-20 19:08:32] lb binary \n\
             [2026-10-20 19:08:32] lb chroot_devpts install\n\
             [2026-10-20 19:08:48] lb binary_rootfs ",
        );
        assert_eq!(t.progress().unwrap().stage, LinuxStage::Iso);
    }

    #[test]
    fn apt_counts_cap_below_the_end_of_the_stage() {
        let mut t = Tracker::new();
        let mut text =
            String::from("OBK-STAGE system\n0 upgraded, 2 newly installed, 0 to remove.\n");
        for _ in 0..5 {
            text.push_str("Setting up foo (1) ...\n");
        }
        feed_all(&mut t, &text);
        assert_eq!(t.progress().unwrap().percent, 60);
        let mut t = Tracker::new();
        feed_all(
            &mut t,
            "[2026-10-20 19:10:10] lb binary \nxorriso : UPDATE :  100.00% done",
        );
        assert_eq!(t.progress().unwrap().percent, 95);
    }

    #[test]
    fn the_script_s_error_and_apt_reason_are_kept() {
        let mut t = Tracker::new();
        feed_all(
            &mut t,
            "OBK-APT Reading package lists...\n\
             OBK-APT The following packages have unmet dependencies:\n\
             OBK-APT  own-backup : Depends: libc6 (>= 2.42) but 2.41-12 is to be installed\n\
             OBK-APT E: Unable to correct problems, you have held broken packages.\n\
             OBK-ERROR apt-check",
        );
        assert_eq!(t.error, Some(("apt-check".into(), String::new())));
        assert_eq!(
            apt_reason(&t.apt),
            "own-backup : Depends: libc6 (>= 2.42) but 2.41-12 is to be installed"
        );
        let text = refusal("apt-check", "", &t.apt);
        assert!(text.contains("libc6 (>= 2.42)"), "{text}");
    }

    #[test]
    fn apt_reason_falls_back_to_errors_then_to_the_last_lines() {
        let lines = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            apt_reason(&lines(&["Reading…", "E: Invalid archive signature"])),
            "E: Invalid archive signature"
        );
        assert_eq!(
            apt_reason(&lines(&["a", "b", "c", "d", "e", ""])),
            "b; c; d; e"
        );
    }

    #[test]
    fn refusals_name_the_remedy() {
        for code in ["missing-tool", "missing-keyring"] {
            let text = refusal(code, "debootstrap", &[]);
            assert!(
                text.contains("sudo apt install debootstrap debian-archive-keyring"),
                "{text}"
            );
        }
        let text = refusal("network", "deb.debian.org", &[]);
        assert!(text.contains("deb.debian.org"), "{text}");
        let text = refusal("something-new", "", &[]);
        assert!(text.contains("something-new"), "{text}");
    }

    #[test]
    fn network_failures_are_told_from_the_tools_words() {
        let host = "deb.debian.org";
        assert_eq!(
            classify_failure(
                ["W: Failed to fetch http://security.debian.org/debian-security/dists/trixie-security/InRelease  Temporary failure resolving 'security.debian.org'"],
                host
            ),
            ToolFailure::Network("security.debian.org".into())
        );
        assert_eq!(
            classify_failure(
                ["E: Failed getting release file http://deb.debian.org/debian/dists/trixie/Release"],
                host
            ),
            ToolFailure::Network("deb.debian.org".into())
        );
        assert_eq!(
            classify_failure(["wget: unable to resolve host address"], host),
            ToolFailure::Network("deb.debian.org".into())
        );
        assert_eq!(
            classify_failure(
                [
                    "cp: error writing 'x': No space left on device",
                    "Failed to fetch"
                ],
                host
            ),
            ToolFailure::NoSpace
        );
        assert_eq!(
            classify_failure(
                ["E: Sub-process /usr/bin/dpkg returned an error code (1)"],
                host
            ),
            ToolFailure::Other
        );
    }

    #[test]
    fn hosts_come_out_of_urls() {
        assert_eq!(url_host("http://deb.debian.org/debian/"), "deb.debian.org");
        assert_eq!(
            url_host("https://mirror.example:8080/debian"),
            "mirror.example"
        );
        assert_eq!(url_host("deb.debian.org"), "deb.debian.org");
    }

    #[test]
    fn the_space_needed_shrinks_with_the_cache_but_not_below_the_trees() {
        assert_eq!(need_bytes(0), PEAK_BYTES);
        assert_eq!(need_bytes(2 << 30), 8 << 30);
        assert_eq!(need_bytes(9 << 30), MIN_FREE_BYTES);
        assert_eq!(need_bytes(u64::MAX), MIN_FREE_BYTES);
        assert_eq!(gib(10 << 30), "10.0");
        assert_eq!(gib(3 << 29), "1.5");
    }

    #[test]
    fn the_iso_is_named_after_the_upstream_version_and_never_reuses_a_name() {
        assert_eq!(upstream_version("0.1.5~beta-1"), "0.1.5~beta");
        assert_eq!(upstream_version("1:2.0-3ubuntu1"), "2.0");
        assert_eq!(upstream_version("2.0"), "2.0");
        assert_eq!(
            iso_file_name("0.1.5~beta-1", 1),
            "OwnBackup-Linux-0.1.5~beta.iso"
        );
        assert_eq!(
            iso_file_name("0.1.5~beta-1", 2),
            "OwnBackup-Linux-0.1.5~beta (2).iso"
        );
        assert_eq!(iso_file_name("1/2-1", 1), "OwnBackup-Linux-1_2.iso");
    }

    #[test]
    fn the_plan_adds_the_stick_s_stages_only_with_a_stick() {
        let ids = |s: bool| {
            LinuxStage::plan(s)
                .iter()
                .map(|s| s.id())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(false), ["builder", "download", "system", "iso"]);
        assert_eq!(
            ids(true),
            ["builder", "download", "system", "iso", "write", "verify", "logs"]
        );
    }
}
