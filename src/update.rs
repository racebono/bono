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
use reqwest::{StatusCode, header};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
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
    sha256: Option<String>,
}

#[derive(Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    name: Option<String>,
    draft: bool,
    prerelease: bool,
    target_commitish: String,
    created_at: String,
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
    /// The release, if it has this platform's build in full.
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
                .and_then(|digest| digest.strip_prefix("sha256:"))
                .map(str::to_owned),
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
    let mut newest = None::<(SystemTime, &GitHubRelease, Release)>;
    for github in releases {
        if github.draft || !github.prerelease || !github.tag_name.starts_with("nightly-") {
            continue;
        }
        let Some(release) = github.release() else {
            continue;
        };
        let created = humantime::parse_rfc3339(&github.created_at)
            .with_context(|| format!("Unexpected release date {:?}", github.created_at))?;
        if newest.as_ref().is_none_or(|(time, ..)| created > *time) {
            newest = Some((created, github, release));
        }
    }
    Ok(newest
        .filter(|(_, github, _)| !github.target_commitish.eq_ignore_ascii_case(commit))
        .map(|(.., release)| release))
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

/// Where downloads are kept, and the installation they replace.
pub struct Staging {
    install: PathBuf,
    dir: PathBuf,
}

impl Staging {
    /// For the installation `bono.exe` runs from.
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self::at(sdl3::filesystem::get_base_path()?))
    }

    fn at(install: &Path) -> Self {
        // Without a trailing separator, which would escape the quote around it on the
        // installer's command line
        let install = install.components().collect::<PathBuf>();
        Self {
            dir: install.join("update"),
            install,
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

    /// Starts downloading `release`'s archive.
    pub fn download(&self, client: &reqwest::Client, release: &Release) -> Download {
        let request = client
            .get(&release.url)
            .header(header::USER_AGENT, concat!("bono/", env!("BONO_VERSION")));
        Download::new(
            request,
            self.zip(release),
            Some(release.size),
            release.sha256.clone().map(DownloadDigest::Sha256),
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
            if Some(&path) != ready.as_ref() {
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
        install(
            &self.ready(release),
            &self.install,
            &self.dir.join("install.log"),
            pid,
        )
        .await
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

/// The installer: waits for the app to exit, copies the new version over the installation and
/// opens it. Files the new version doesn't have are left alone: the installation's folder may
/// hold more than the app. Copying is retried while a file is still in use. `bono.exe` goes
/// last, only once everything else is in place: if anything fails, the old version opens again,
/// still offering to install the new one. Paths come in through the environment, so they need
/// no quoting.
#[cfg(windows)]
const INSTALLER: &str = r#"
$ErrorActionPreference = 'Stop'
$app = Get-Process -Id $env:BONO_UPDATE_PID
# Holding a handle to the app keeps its ID from going to another process before it's waited on
$null = $app.Handle
[Console]::Out.WriteLine('waiting')
[Console]::Out.Flush()
$app.WaitForExit()
$ErrorActionPreference = 'Continue'
$source = $env:BONO_UPDATE_SOURCE
$target = $env:BONO_UPDATE_TARGET
# Robocopy fails with an exit code of 8 or more
robocopy $source $target /E /XF bono.exe /R:30 /W:1 /NP ('/LOG:' + $env:BONO_UPDATE_LOG) | Out-Null
if ($LASTEXITCODE -lt 8) {
    robocopy $source $target bono.exe /R:30 /W:1 /NP ('/LOG+:' + $env:BONO_UPDATE_LOG) | Out-Null
}
Start-Process -FilePath (Join-Path $target 'bono.exe') -WorkingDirectory $target
"#;

#[cfg(windows)]
async fn install(source: &Path, target: &Path, log: &Path, pid: u32) -> anyhow::Result<()> {
    use base64::Engine as _;
    use tokio::io::AsyncBufReadExt as _;

    /// How long PowerShell may take to start waiting for the app
    const START: std::time::Duration = std::time::Duration::from_secs(30);
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // An encoded command needs no quoting either
    let script = (INSTALLER.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let system = std::env::var_os("SystemRoot").context("SystemRoot isn't set")?;
    let mut child = tokio::process::Command::new(
        Path::new(&system).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
    )
    .args(["-NoProfile", "-NonInteractive", "-EncodedCommand"])
    .arg(base64::engine::general_purpose::STANDARD.encode(script))
    .env("BONO_UPDATE_PID", pid.to_string())
    .env("BONO_UPDATE_SOURCE", source)
    .env("BONO_UPDATE_TARGET", target)
    .env("BONO_UPDATE_LOG", log)
    .current_dir(target)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .creation_flags(CREATE_NO_WINDOW)
    .spawn()
    .context("Unable to start the installer")?;

    let mut stdout = tokio::io::BufReader::new(child.stdout.take().context("No installer output")?);
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

#[cfg(not(windows))]
async fn install(_: &Path, _: &Path, _: &Path, _: u32) -> anyhow::Result<()> {
    anyhow::bail!("Updates can't be installed on this platform")
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
            created_at: created.into(),
            assets: ASSET.into_iter().map(asset).collect(),
        }
    }

    #[test]
    fn parses_github_releases() {
        let json = r#"{"tag_name":"v1.2.3","name":"","draft":false,"prerelease":false,
            "target_commitish":"master","created_at":"2026-11-03T04:45:29Z","body":"notes",
            "assets":[{"name":"bono-windows-x86_64.zip","state":"uploaded","size":7,
            "browser_download_url":"https://example.com/a.zip","digest":null,"id":1}]}"#;
        let github = serde_json::from_str::<GitHubRelease>(json).unwrap();
        if ASSET.is_none() {
            return assert_eq!(github.release(), None);
        }
        let release = github.release().unwrap();
        assert_eq!(
            (release.tag.as_str(), release.name.as_str()),
            ("v1.2.3", "v1.2.3")
        );
        assert_eq!((release.size, release.sha256), (7, None));
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
        assert_eq!(newest.sha256.as_deref(), Some("abc"));
        assert_eq!(pick_nightly(&releases, "CCC").unwrap(), None);

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
            let newest = newest.expect("no nightly");
            println!("{newest:?}");
            assert!(newest.sha256.is_some());
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
            sha256: None,
        }
    }

    #[test]
    fn unpacks_and_tidies() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let staging = Staging::at(dir.path());
        let release = staged("v0.2.0");
        std::fs::create_dir_all(&staging.dir).unwrap();

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

        assert!(staging.tidy(Some(&release)));
        assert_eq!(std::fs::read_dir(&staging.dir).unwrap().count(), 1);
        assert!(!staging.tidy(Some(&staged("v0.3.0"))));
        assert_eq!(std::fs::read_dir(&staging.dir).unwrap().count(), 0);
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
    fn install_folder_has_no_trailing_separator() {
        let base = std::env::temp_dir().join("bono app");
        let mut trailing = base.clone().into_os_string();
        trailing.push(std::path::MAIN_SEPARATOR_STR);
        assert_eq!(Staging::at(Path::new(&trailing)).install, base);
    }

    /// The real installer: it waits for the app to exit before copying the new version over the
    /// installation, keeps files it doesn't replace, and opens the new version.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn installer_waits_for_the_app_then_copies() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("bono app");
        let staging = Staging::at(&install);
        let release = staged("v0.2.0");
        let ready = staging.ready(&release);
        std::fs::create_dir_all(ready.join("assets")).unwrap();
        std::fs::write(ready.join("assets/new.txt"), "new").unwrap();
        std::fs::write(ready.join("changed.txt"), "new").unwrap();
        // Something that opens and exits by itself, standing in for the app
        let system = std::env::var_os("SystemRoot").unwrap();
        let whoami = Path::new(&system).join(r"System32\whoami.exe");
        std::fs::copy(&whoami, ready.join(EXE)).unwrap();
        std::fs::write(install.join("changed.txt"), "old").unwrap();
        std::fs::write(install.join("kept.txt"), "kept").unwrap();

        // Standing in for the running app
        let mut app = std::process::Command::new(Path::new(&system).join(r"System32\ping.exe"))
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let installing = staging.install(&release, app.id()).await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let early = install.join(EXE).exists();
        app.kill().unwrap();
        app.wait().unwrap();
        installing.unwrap();
        assert!(!early, "copied before the app exited");

        let copied = install.join(EXE);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !(copied.exists() && install.join("changed.txt").exists()) {
            assert!(
                std::time::Instant::now() < deadline,
                "nothing was installed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        // Robocopy may still be finishing
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let done = std::fs::read_to_string(install.join("changed.txt")).unwrap() == "new"
                && install.join("assets/new.txt").exists()
                && std::fs::metadata(&copied).unwrap().len()
                    == std::fs::metadata(&whoami).unwrap().len();
            if done {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the install didn't finish"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(
            std::fs::read_to_string(install.join("kept.txt")).unwrap(),
            "kept"
        );
        assert!(staging.dir.join("install.log").exists());
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn installer_fails_for_a_missing_app() {
        let dir = tempfile::tempdir().unwrap();
        let staging = Staging::at(dir.path());
        let system = std::env::var_os("SystemRoot").unwrap();
        let mut gone = std::process::Command::new(Path::new(&system).join(r"System32\whoami.exe"))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        gone.wait().unwrap();
        let pid = gone.id();
        drop(gone);
        assert!(staging.install(&staged("v0.2.0"), pid).await.is_err());
    }
}
