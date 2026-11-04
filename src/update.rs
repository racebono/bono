//! New versions of the app: finding one on this build's release channel, downloading it, and
//! installing it over this installation.
//!
//! Stable builds follow stable releases and nightly builds follow nightlies. Settings, Whisper
//! models and Piper voices live in the per-user preference directory, not in the installation,
//! so replacing the installation's files carries them over untouched.
//!
//! Downloads are kept in the installation's `update/` folder, named after their release's tag:
//! `<tag>.zip` once downloaded and verified, `<tag>.partial/` while it's unpacked, and `<tag>/`
//! when it's ready to install.

use crate::{Download, DownloadDigest};
use anyhow::Context as _;
use bono_i18n::t;
use reqwest::{StatusCode, header};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
    time::SystemTime,
};

const RELEASES: &str = "https://api.github.com/repos/racebono/bono/releases";
/// The release file with this platform's build.
const ASSET: Option<&str> = if cfg!(all(windows, target_arch = "x86_64")) {
    Some("bono-windows-x86_64.zip")
} else {
    None
};
/// The app, at the root of a release's archive.
const EXE: &str = "bono.exe";
/// Nightlies are looked for in at most this many pages of 100 releases.
const MAX_PAGES: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Nightly,
}

impl Channel {
    /// Nightly builds carry the commit they were built from; stable ones don't.
    pub fn current() -> Self {
        match commit() {
            Some(_) => Self::Nightly,
            None => Self::Stable,
        }
    }
}

fn commit() -> Option<&'static str> {
    option_env!("BONO_COMMIT").filter(|commit| !commit.is_empty())
}

/// Whether this build can update itself: only release builds of a platform there are builds for.
/// A development build runs from the source tree, which mustn't be replaced.
pub fn supported() -> bool {
    !cfg!(debug_assertions) && ASSET.is_some()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Also names its files in the `update/` folder
    pub tag: String,
    pub name: String,
    /// The archive's size, in bytes
    pub size: u64,
    url: String,
    sha256: String,
}

#[derive(Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    name: Option<String>,
    draft: bool,
    prerelease: bool,
    target_commitish: String,
    /// Unset while a draft
    published_at: Option<String>,
    assets: Vec<GitHubAsset>,
}

#[derive(Clone, Deserialize)]
struct GitHubAsset {
    name: String,
    /// `uploaded` once the whole file is there
    state: String,
    size: u64,
    browser_download_url: String,
    /// `sha256:<hex>`
    digest: Option<String>,
}

impl GitHubRelease {
    /// The release, if it has this platform's build in full, with its digest to check it by.
    fn release(&self) -> Option<Release> {
        let asset = (self.assets.iter())
            .find(|asset| Some(asset.name.as_str()) == ASSET && asset.state == "uploaded")?;
        if !valid_tag(&self.tag_name) {
            log::warn!("Ignoring release {:?}: unexpected tag", self.tag_name);
            return None;
        }
        Some(Release {
            tag: self.tag_name.clone(),
            name: (self.name.as_deref().map(str::trim))
                .filter(|name| !name.is_empty())
                .unwrap_or(&self.tag_name)
                .to_owned(),
            size: asset.size,
            url: asset.browser_download_url.clone(),
            sha256: (asset.digest.as_deref())
                .and_then(|digest| digest.strip_prefix("sha256:"))?
                .to_owned(),
        })
    }
}

/// Tags name files and folders, so only plain names are taken.
fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && !tag.starts_with('.')
        && (tag.bytes()).all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// The latest stable release, if it's newer than `current`. One that's still having its build
/// uploaded doesn't count yet.
fn pick_stable(latest: &GitHubRelease, current: &str) -> anyhow::Result<Option<Release>> {
    if latest.draft || latest.prerelease {
        return Ok(None);
    }
    let Some(release) = latest.release() else {
        return Ok(None);
    };
    let tag = &latest.tag_name;
    let version = semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag))
        .with_context(|| format!("Unexpected release version {tag:?}"))?;
    Ok((version > semver::Version::parse(current)?).then_some(release))
}

/// The newest nightly, unless it's the one built from `commit`.
fn pick_nightly(releases: &[GitHubRelease], commit: &str) -> anyhow::Result<Option<Release>> {
    let mut newest = None::<(SystemTime, Release)>;
    // When this build's nightly came out, if it's there: whether or not it's complete, nothing
    // older is newer
    let mut own = None::<SystemTime>;
    for github in releases {
        if github.draft || !github.prerelease || !github.tag_name.starts_with("nightly-") {
            continue;
        }
        let Some(published) = github.published_at.as_deref() else {
            continue;
        };
        let published = humantime::parse_rfc3339(published)
            .with_context(|| format!("Unexpected release date {published:?}"))?;
        if github.target_commitish.eq_ignore_ascii_case(commit) {
            own = own.max(Some(published));
            continue;
        }
        let Some(release) = github.release() else {
            continue;
        };
        if newest.as_ref().is_none_or(|(time, _)| published > *time) {
            newest = Some((published, release));
        }
    }
    Ok(newest
        .filter(|(time, _)| own.is_none_or(|own| *time > own))
        .map(|(_, release)| release))
}

/// The URL of the page after this one, from a `Link` header.
fn next_page(link: &str) -> Option<&str> {
    link.split(',').find_map(|link| {
        let (url, params) = link.split_once(';')?;
        (params
            .split(';')
            .any(|param| param.trim() == r#"rel="next""#))
        .then(|| url.trim().strip_prefix('<')?.strip_suffix('>'))
        .flatten()
    })
}

fn github(client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    client
        .get(url)
        // GitHub turns down requests without one
        .header(header::USER_AGENT, concat!("bono/", env!("BONO_VERSION")))
        .header(header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

/// The newest release on this build's channel, if it's newer than this build.
pub async fn check(client: &reqwest::Client) -> anyhow::Result<Option<Release>> {
    let Some(commit) = commit() else {
        let response = github(client, &format!("{RELEASES}/latest")).send().await?;
        // There's no stable release yet
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let latest = response.error_for_status()?.json().await?;
        return pick_stable(&latest, env!("CARGO_PKG_VERSION"));
    };

    // GitHub doesn't say in what order it lists releases, so all of them are looked at
    let mut releases = Vec::<GitHubRelease>::new();
    let mut url = Some(format!("{RELEASES}?per_page=100"));
    for _ in 0..MAX_PAGES {
        let Some(page) = url.take() else { break };
        let response = github(client, &page).send().await?.error_for_status()?;
        url = (response.headers().get(header::LINK))
            .and_then(|link| link.to_str().ok())
            .and_then(next_page)
            .map(str::to_owned);
        releases.extend(response.json::<Vec<GitHubRelease>>().await?);
    }
    if url.is_some() {
        log::warn!("Only the first {} releases were looked at", releases.len());
    }
    pick_nightly(&releases, commit)
}

/// Held by the app updating an installation, so another one running from it leaves its
/// downloads alone.
const LOCK: &str = "lock";
/// What went wrong with the last installation, written by the installer
const FAILED: &str = "install-failed.txt";
/// How many times the installer tries to move a file or folder still in use, a second apart
const RETRIES: u32 = 30;

/// Where downloads are kept, and the installation they replace.
pub struct Staging {
    install: PathBuf,
    dir: PathBuf,
    lock: OnceLock<std::fs::File>,
}

impl Staging {
    /// For the installation `bono.exe` runs from.
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self::at(sdl3::filesystem::get_base_path()?))
    }

    fn at(install: &Path) -> Self {
        Self {
            install: install.to_owned(),
            dir: install.join("update"),
            lock: OnceLock::new(),
        }
    }

    fn zip(&self, release: &Release) -> PathBuf {
        self.dir.join(format!("{}.zip", release.tag))
    }

    fn partial(&self, release: &Release) -> PathBuf {
        self.dir.join(format!("{}.partial", release.tag))
    }

    fn ready(&self, release: &Release) -> PathBuf {
        self.dir.join(&release.tag)
    }

    /// Takes the installation's updates for this app, for as long as it runs. Another app
    /// running from the same folder would remove what this one downloads, and keep the installer
    /// from moving files it has open. Blocks.
    pub fn claim(&self) -> anyhow::Result<()> {
        if self.lock.get().is_some() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        let mut options = std::fs::File::options();
        options.create(true).truncate(false).write(true);
        // Open without sharing: no one else can open it while it's open
        #[cfg(windows)]
        std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, 0);
        let file = match options.open(self.dir.join(LOCK)) {
            Ok(file) => file,
            // ERROR_SHARING_VIOLATION: another Bono, or the installer, has it open
            Err(e) if cfg!(windows) && e.raw_os_error() == Some(32) => {
                anyhow::bail!(t!("update.locked"))
            }
            Err(e) => return Err(e.into()),
        };
        // Only one check runs at a time
        let _ = self.lock.set(file);
        Ok(())
    }

    /// What went wrong with the last installation, if it failed, said only once. Blocks.
    pub fn take_failure(&self) -> Option<String> {
        let path = self.dir.join(FAILED);
        let failure = match std::fs::read_to_string(&path) {
            Ok(failure) => failure,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => e.to_string(),
        };
        if let Err(e) = std::fs::remove_file(&path) {
            log::warn!("Unable to remove {}: {e}", path.display());
        }
        Some(failure.trim().to_owned())
    }

    /// Starts downloading `release`'s archive.
    pub fn download(&self, client: &reqwest::Client, release: &Release) -> Download {
        let request = client
            .get(&release.url)
            .header(header::USER_AGENT, concat!("bono/", env!("BONO_VERSION")));
        Download::new(
            request,
            self.zip(release),
            Some(release.size),
            Some(DownloadDigest::Sha256(release.sha256.clone())),
        )
    }

    /// Unpacks `release`'s downloaded archive, ready to install. Blocks.
    pub fn unpack(&self, release: &Release) -> anyhow::Result<()> {
        let (zip, partial, ready) = (
            self.zip(release),
            self.partial(release),
            self.ready(release),
        );
        remove(&partial)?;
        let file = std::io::BufReader::new(std::fs::File::open(&zip)?);
        zip::ZipArchive::new(file)?.extract(&partial)?;
        anyhow::ensure!(partial.join(EXE).is_file(), "The download has no {EXE}");
        remove(&ready)?;
        std::fs::rename(&partial, &ready)?;
        if let Err(e) = std::fs::remove_file(&zip) {
            log::warn!("Unable to remove {}: {e}", zip.display());
        }
        Ok(())
    }

    /// Removes every download but `latest`'s unpacked one, returning whether that one's there.
    /// Blocks.
    pub fn tidy(&self, latest: Option<&Release>) -> bool {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            Err(e) => {
                log::warn!("Unable to read {}: {e}", self.dir.display());
                return false;
            }
        };
        let ready = latest.map(|release| self.ready(release));
        for entry in entries.flatten() {
            let path = entry.path();
            if Some(&path) != ready.as_ref() && entry.file_name() != LOCK {
                if let Err(e) = remove(&path) {
                    log::warn!("Unable to remove {}: {e}", path.display());
                }
            }
        }
        ready.is_some_and(|ready| ready.join(EXE).is_file())
    }

    /// Starts installing `release`, once unpacked, as soon as the process `pid` (this app)
    /// exits, then opens the new version. Returns once the installer is waiting.
    pub async fn install(&self, release: &Release, pid: u32) -> anyhow::Result<()> {
        self.installer(release, RETRIES, true).start(pid).await
    }

    fn installer(&self, release: &Release, retries: u32, breakaway: bool) -> Installer {
        Installer {
            source: self.ready(release),
            target: self.install.clone(),
            backup: self.dir.join("backup"),
            failed: self.dir.join(FAILED),
            lock: self.dir.join(LOCK),
            retries,
            breakaway,
        }
    }
}

/// Removes a file or folder, if it's there.
fn remove(path: &Path) -> std::io::Result<()> {
    let result = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Puts an unpacked version (`source`) in place of the installation's (`target`).
struct Installer {
    source: PathBuf,
    target: PathBuf,
    /// Where what's replaced goes, on the installation's drive
    backup: PathBuf,
    /// Where it says what went wrong, if it failed
    failed: PathBuf,
    /// The app's claim on the installation's updates, which it takes over
    lock: PathBuf,
    retries: u32,
    /// Leaves the app's job, if it's in one, which may end its processes with it: the installer
    /// could be stopped halfway. A job that doesn't allow that fails the start
    breakaway: bool,
}

/// The installer: waits for the app to exit, puts the new version in place and opens it.
///
/// Each file and folder at the top of the new version replaces the installation's by renaming,
/// all on the same drive: nothing is copied, so nothing is left half written. What it replaces
/// is moved aside first. If anything fails, everything moved goes back where it was, so the old
/// version opens again, still offering the new one, and the installer says what went wrong.
/// Anything else in the installation's folder stays. Moves are retried while something still has
/// the file or folder open. Paths come in through the environment, so they need no quoting.
#[cfg(windows)]
const INSTALLER: &str = include_str!("update/installer.ps1");

#[cfg(windows)]
impl Installer {
    async fn start(self, pid: u32) -> anyhow::Result<()> {
        use base64::Engine as _;
        use tokio::io::AsyncBufReadExt as _;

        /// How long PowerShell may take to start waiting for the app
        const START: std::time::Duration = std::time::Duration::from_secs(30);
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

        // An encoded command needs no quoting either
        let script = (INSTALLER.encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let system = std::env::var_os("SystemRoot").context("SystemRoot isn't set")?;
        let powershell = Path::new(&system).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let mut command = tokio::process::Command::new(powershell);
        command
            .args(["-NoProfile", "-NonInteractive", "-EncodedCommand"])
            .arg(base64::engine::general_purpose::STANDARD.encode(script))
            .env("BONO_UPDATE_PID", pid.to_string())
            .env("BONO_UPDATE_SOURCE", &self.source)
            .env("BONO_UPDATE_TARGET", &self.target)
            .env("BONO_UPDATE_BACKUP", &self.backup)
            .env("BONO_UPDATE_FAILED", &self.failed)
            .env("BONO_UPDATE_LOCK", &self.lock)
            .env("BONO_UPDATE_RETRIES", self.retries.to_string())
            .current_dir(&self.target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let breakaway = match self.breakaway {
            true => CREATE_BREAKAWAY_FROM_JOB,
            false => 0,
        };
        let mut child = (command.creation_flags(CREATE_NO_WINDOW | breakaway))
            .spawn()
            .context("Unable to start the installer. If Bono was started by another program, start it by itself")?;

        let stdout = child.stdout.take().context("No installer output")?;
        let mut stdout = tokio::io::BufReader::new(stdout);
        let mut line = String::new();
        match tokio::time::timeout(START, stdout.read_line(&mut line)).await {
            Ok(Ok(_)) if line.trim() == "waiting" => Ok(()),
            result => {
                // It mustn't install later, unasked
                let _ = child.start_kill();
                match result {
                    Ok(Err(e)) => Err(e).context("Unable to start the installer"),
                    Err(_) => anyhow::bail!("The installer didn't start in {START:?}"),
                    Ok(Ok(_)) => anyhow::bail!("The installer didn't start"),
                }
            }
        }
    }
}

#[cfg(not(windows))]
impl Installer {
    async fn start(self, _: u32) -> anyhow::Result<()> {
        anyhow::bail!("Updates can't be installed on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> GitHubAsset {
        GitHubAsset {
            name: name.into(),
            state: "uploaded".into(),
            size: 3,
            browser_download_url: format!("https://example.com/{name}"),
            digest: Some("sha256:abc".into()),
        }
    }

    fn release(tag: &str, prerelease: bool, commit: &str, created: &str) -> GitHubRelease {
        GitHubRelease {
            tag_name: tag.into(),
            name: Some(format!("Release {tag}")),
            draft: false,
            prerelease,
            target_commitish: commit.into(),
            published_at: Some(created.into()),
            assets: ASSET.into_iter().map(asset).collect(),
        }
    }

    #[test]
    fn parses_github_releases() {
        let json = |digest: &str| {
            format!(
                r#"{{"tag_name":"v1.2.3","name":"","draft":false,"prerelease":false,
                "target_commitish":"master","published_at":"2026-11-03T04:45:29Z","body":"notes",
                "assets":[{{"name":"bono-windows-x86_64.zip","state":"uploaded","size":7,
                "browser_download_url":"https://example.com/a.zip","digest":{digest},"id":1}}]}}"#
            )
        };
        let github = serde_json::from_str::<GitHubRelease>(&json(r#""sha256:abc""#)).unwrap();
        if ASSET.is_none() {
            return assert_eq!(github.release(), None);
        }
        let release = github.release().unwrap();
        assert_eq!(
            (release.tag.as_str(), release.name.as_str()),
            ("v1.2.3", "v1.2.3")
        );
        assert_eq!((release.size, release.sha256.as_str()), (7, "abc"));

        // Without a digest to check it by, it isn't offered
        for digest in ["null", r#""md5:abc""#] {
            let github = serde_json::from_str::<GitHubRelease>(&json(digest)).unwrap();
            assert_eq!(github.release(), None);
        }
    }

    #[test]
    fn stable_must_be_newer_and_complete() {
        if ASSET.is_none() {
            return;
        }
        let latest = release("v0.2.0", false, "master", "2026-11-03T00:00:00Z");
        assert_eq!(
            pick_stable(&latest, "0.1.0").unwrap().unwrap().tag,
            "v0.2.0"
        );
        assert_eq!(pick_stable(&latest, "0.2.0").unwrap(), None);
        assert_eq!(pick_stable(&latest, "0.3.0").unwrap(), None);
        let unprefixed = release("0.2.0", false, "master", "2026-11-03T00:00:00Z");
        assert!(pick_stable(&unprefixed, "0.1.0").unwrap().is_some());

        let mut uploading = release("v0.2.0", false, "master", "2026-11-03T00:00:00Z");
        uploading.assets[0].state = "open".into();
        assert_eq!(pick_stable(&uploading, "0.1.0").unwrap(), None);
        let mut building = release("v0.2.0", false, "master", "2026-11-03T00:00:00Z");
        building.assets.clear();
        assert_eq!(pick_stable(&building, "0.1.0").unwrap(), None);

        let odd = release("latest", false, "master", "2026-11-03T00:00:00Z");
        assert!(pick_stable(&odd, "0.1.0").is_err());
    }

    #[test]
    fn nightly_is_the_newest_unless_installed() {
        if ASSET.is_none() {
            return;
        }
        let releases = [
            release(
                "nightly-2026-11-01-1-1",
                true,
                "aaa",
                "2026-11-01T03:00:00Z",
            ),
            release(
                "nightly-2026-11-03-3-1",
                true,
                "ccc",
                "2026-11-03T03:00:00Z",
            ),
            release(
                "nightly-2026-11-02-2-1",
                true,
                "bbb",
                "2026-11-02T03:00:00Z",
            ),
            // Newer, but not a nightly
            release("v0.2.0", false, "ddd", "2026-11-04T03:00:00Z"),
            release("beta", true, "eee", "2026-11-04T03:00:00Z"),
        ];
        let newest = pick_nightly(&releases, "aaa").unwrap().unwrap();
        assert_eq!(newest.tag, "nightly-2026-11-03-3-1");
        assert_eq!(newest.name, "Release nightly-2026-11-03-3-1");
        assert_eq!(newest.sha256, "abc");
        assert_eq!(pick_nightly(&releases, "CCC").unwrap(), None);

        // This build's nightly, newest but still uploading: nothing older is newer
        let mut own = releases.to_vec();
        own[1].assets[0].state = "open".into();
        assert_eq!(pick_nightly(&own, "ccc").unwrap(), None);
        // A draft isn't out yet
        own[2].published_at = None;
        let newest = pick_nightly(&own, "zzz").unwrap().unwrap();
        assert_eq!(newest.tag, "nightly-2026-11-01-1-1");

        // One still uploading doesn't count
        let mut uploading = releases.to_vec();
        uploading[1].assets[0].state = "open".into();
        assert_eq!(
            pick_nightly(&uploading, "aaa").unwrap().unwrap().tag,
            "nightly-2026-11-02-2-1"
        );
        assert_eq!(pick_nightly(&[], "aaa").unwrap(), None);
    }

    /// Needs the network: GitHub's real answers are understood.
    #[tokio::test]
    #[ignore]
    async fn reads_published_releases() {
        let client = reqwest::Client::new();
        let response = github(&client, &format!("{RELEASES}?per_page=100"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let releases = response.json::<Vec<GitHubRelease>>().await.unwrap();
        let newest = pick_nightly(&releases, "").unwrap();
        if ASSET.is_some() {
            println!("{:?}", newest.expect("no nightly"));
        }
        let response = github(&client, &format!("{RELEASES}/latest"))
            .send()
            .await
            .unwrap();
        if response.status() != StatusCode::NOT_FOUND {
            let latest = response.error_for_status().unwrap().json().await.unwrap();
            println!("{:?}", pick_stable(&latest, "0.0.0").unwrap());
        }
    }

    #[test]
    fn tags_are_plain_names() {
        assert!(valid_tag("nightly-2026-11-03-41577732307-1"));
        assert!(valid_tag("v0.2.0"));
        for tag in ["", ".", "..", "a/b", r"a\b", "a b", "a:b"] {
            assert!(!valid_tag(tag), "{tag:?}");
        }
    }

    #[test]
    fn follows_next_link() {
        let link = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=7>; rel="last""#;
        assert_eq!(next_page(link), Some("https://api.github.com/x?page=2"));
        let last = r#"<https://api.github.com/x?page=1>; rel="prev", <https://api.github.com/x?page=1>; rel="first""#;
        assert_eq!(next_page(last), None);
    }

    fn staged(name: &str) -> Release {
        Release {
            tag: name.into(),
            name: name.into(),
            size: 0,
            url: String::new(),
            sha256: String::new(),
        }
    }

    #[test]
    fn unpacks_and_tidies() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let staging = Staging::at(dir.path());
        let release = staged("v0.2.0");
        staging.claim().unwrap();

        let mut zip = zip::ZipWriter::new(std::fs::File::create(staging.zip(&release)).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in [(EXE, "exe"), ("assets/a.txt", "a")] {
            zip.start_file(name, options).unwrap();
            zip.write_all(data.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        // Left behind by an earlier attempt
        std::fs::create_dir_all(staging.partial(&release).join("stale")).unwrap();
        std::fs::write(staging.dir.join("old.zip"), "old").unwrap();

        staging.unpack(&release).unwrap();
        let ready = staging.ready(&release);
        assert_eq!(
            std::fs::read_to_string(ready.join("assets/a.txt")).unwrap(),
            "a"
        );
        assert!(!staging.zip(&release).exists() && !staging.partial(&release).exists());

        // The lock stays
        assert!(staging.tidy(Some(&release)));
        assert_eq!(std::fs::read_dir(&staging.dir).unwrap().count(), 2);
        assert!(!staging.tidy(Some(&staged("v0.3.0"))));
        assert_eq!(std::fs::read_dir(&staging.dir).unwrap().count(), 1);
        assert!(!staging.tidy(None));
    }

    #[test]
    fn archive_without_app_isnt_ready() {
        let dir = tempfile::tempdir().unwrap();
        let staging = Staging::at(dir.path());
        let release = staged("v0.2.0");
        std::fs::create_dir_all(&staging.dir).unwrap();
        let mut zip = zip::ZipWriter::new(std::fs::File::create(staging.zip(&release)).unwrap());
        zip.start_file("other.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.finish().unwrap();
        assert!(staging.unpack(&release).is_err());
        assert!(!staging.ready(&release).exists());
    }

    #[test]
    fn one_app_updates_an_installation() {
        let dir = tempfile::tempdir().unwrap();
        let first = Staging::at(dir.path());
        first.claim().unwrap();
        first.claim().unwrap();
        assert!(Staging::at(dir.path()).claim().is_err());
        drop(first);
        Staging::at(dir.path()).claim().unwrap();
    }

    #[test]
    fn failure_is_said_once() {
        let dir = tempfile::tempdir().unwrap();
        let staging = Staging::at(dir.path());
        assert_eq!(staging.take_failure(), None);
        std::fs::create_dir_all(&staging.dir).unwrap();
        std::fs::write(staging.dir.join(FAILED), "Access denied\r\n").unwrap();
        assert_eq!(staging.take_failure().as_deref(), Some("Access denied"));
        assert_eq!(staging.take_failure(), None);
    }

    #[cfg(windows)]
    fn system(exe: &str) -> PathBuf {
        Path::new(&std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join(exe)
    }

    #[cfg(windows)]
    fn hidden_command(exe: &str) -> std::process::Command {
        use std::os::windows::process::CommandExt as _;

        let mut command = std::process::Command::new(system(exe));
        // These console programs stand in for the GUI app during installer tests.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }

    /// A self-exiting stand-in that ShellExecute can launch without opening a console.
    #[cfg(windows)]
    fn gui_stand_in(exe: &str, destination: &Path) {
        let mut image = std::fs::read(system(exe)).unwrap();
        // Microsoft's PE format: e_lfanew points to the four-byte PE signature,
        // followed by a 20-byte COFF header. Subsystem is at optional-header offset 68
        // in both PE32 and PE32+. Only the temporary copy is changed.
        // https://learn.microsoft.com/en-us/windows/win32/debug/pe-format
        assert_eq!(&image[..2], b"MZ");
        let pe = u32::from_le_bytes(image[0x3c..0x40].try_into().unwrap()) as usize;
        assert_eq!(&image[pe..pe + 4], b"PE\0\0");
        let optional = pe + 4 + 20;
        let magic = u16::from_le_bytes(image[optional..optional + 2].try_into().unwrap());
        assert!(matches!(magic, 0x10b | 0x20b));
        let subsystem = optional + 68;
        assert_eq!(&image[subsystem..subsystem + 2], &3u16.to_le_bytes());
        image[subsystem..subsystem + 2].copy_from_slice(&2u16.to_le_bytes());
        std::fs::write(destination, image).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn gui_stand_ins_exit_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(EXE);
        for source in ["whoami.exe", "hostname.exe"] {
            gui_stand_in(source, &exe);
            // No console-suppression flags: the GUI subsystem must suffice on its own.
            assert!(
                std::process::Command::new(&exe)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }

    /// An unpacked version, with something that opens and exits by itself for the app
    #[cfg(windows)]
    fn unpacked(staging: &Staging, release: &Release) {
        let ready = staging.ready(release);
        std::fs::create_dir_all(ready.join("assets")).unwrap();
        std::fs::write(ready.join("assets/new.txt"), "new").unwrap();
        std::fs::write(ready.join("changed.txt"), "new").unwrap();
        gui_stand_in("whoami.exe", &ready.join(EXE));
    }

    /// An installed version, with a file the new one doesn't have
    #[cfg(windows)]
    fn installed(install: &Path) {
        std::fs::create_dir_all(install.join("assets")).unwrap();
        std::fs::write(install.join("assets/old.txt"), "old").unwrap();
        std::fs::write(install.join("assets/a.txt"), "a").unwrap();
        std::fs::write(install.join("changed.txt"), "old").unwrap();
        std::fs::write(install.join("kept.txt"), "kept").unwrap();
        gui_stand_in("hostname.exe", &install.join(EXE));
    }

    #[cfg(windows)]
    async fn wait_for(what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{what}");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// The real installer waits for the app to exit, puts the new version in place, keeps what
    /// it doesn't replace, and opens the new version.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn installer_waits_for_the_app_then_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        // PowerShell takes brackets for wildcards in places
        let install = dir.path().join("bono [app]");
        let staging = Staging::at(&install);
        let release = staged("v0.2.0");
        unpacked(&staging, &release);
        installed(&install);

        // Standing in for the running app
        let mut app = hidden_command("ping.exe")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // Tests may run in a job that doesn't let it leave
        let installing = (staging.installer(&release, RETRIES, false))
            .start(app.id())
            .await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let early = std::fs::read_to_string(install.join("changed.txt")).unwrap() != "old";
        app.kill().unwrap();
        app.wait().unwrap();
        installing.unwrap();
        assert!(!early, "installed before the app exited");

        // bono.exe goes last
        let new_exe = std::fs::metadata(system("whoami.exe")).unwrap().len();
        wait_for("the install didn't finish", || {
            std::fs::metadata(install.join(EXE)).is_ok_and(|exe| exe.len() == new_exe)
        })
        .await;
        let read = |path: &str| std::fs::read_to_string(install.join(path)).unwrap();
        assert_eq!(read("changed.txt"), "new");
        assert_eq!(read("assets/new.txt"), "new");
        assert!(!install.join("assets/old.txt").exists());
        assert_eq!(read("kept.txt"), "kept");
        assert_eq!(staging.take_failure(), None);
    }

    /// When something can't be replaced, the installer puts everything back and says so.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn installer_rolls_back_a_failed_install() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("bono app");
        let staging = Staging::at(&install);
        let release = staged("v0.2.0");
        unpacked(&staging, &release);
        installed(&install);
        // Open without sharing, so its folder can't be moved
        let held = std::fs::File::options()
            .read(true)
            .share_mode(0)
            .open(install.join("assets/old.txt"))
            .unwrap();

        let mut app = hidden_command("ping.exe")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let installing = staging.installer(&release, 2, false).start(app.id()).await;
        app.kill().unwrap();
        app.wait().unwrap();
        installing.unwrap();
        let failed = staging.dir.join(FAILED);
        wait_for("the installer didn't fail", || failed.exists()).await;
        drop(held);

        let read = |path: &Path| std::fs::read_to_string(path).unwrap();
        assert_eq!(read(&install.join("changed.txt")), "old");
        assert_eq!(read(&install.join("assets/old.txt")), "old");
        assert_eq!(read(&install.join("kept.txt")), "kept");
        // Nothing of a folder that couldn't be moved went
        assert_eq!(read(&install.join("assets/a.txt")), "a");
        assert_eq!(
            std::fs::read_dir(install.join("assets")).unwrap().count(),
            2
        );
        let old_exe = std::fs::metadata(system("hostname.exe")).unwrap().len();
        assert_eq!(std::fs::metadata(install.join(EXE)).unwrap().len(), old_exe);
        assert!(!staging.take_failure().unwrap().is_empty());
        // Still ready to install
        assert!(staging.tidy(Some(&release)));
        assert_eq!(read(&staging.ready(&release).join("changed.txt")), "new");
    }

    /// Another Bono claimed the installation's updates: the installer leaves it alone.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn installer_yields_to_another_app() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("bono app");
        let staging = Staging::at(&install);
        let release = staged("v0.2.0");
        unpacked(&staging, &release);
        installed(&install);
        let other = Staging::at(&install);
        other.claim().unwrap();

        let mut app = hidden_command("ping.exe")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let installing = staging.installer(&release, 2, false).start(app.id()).await;
        app.kill().unwrap();
        app.wait().unwrap();
        installing.unwrap();
        // It gives up after its retries
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        assert_eq!(
            std::fs::read_to_string(install.join("changed.txt")).unwrap(),
            "old"
        );
        assert!(staging.ready(&release).join(EXE).exists());
        assert!(!staging.dir.join("backup").exists());
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn installer_fails_for_a_missing_app() {
        let dir = tempfile::tempdir().unwrap();
        let staging = Staging::at(dir.path());
        let mut gone = hidden_command("whoami.exe")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        gone.wait().unwrap();
        let pid = gone.id();
        drop(gone);
        let installer = staging.installer(&staged("v0.2.0"), RETRIES, false);
        assert!(installer.start(pid).await.is_err());
    }
}
