use crate::ARCHIVE;
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, time::Duration};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub channel: String,
    pub tag: String,
}

fn stable(tag: &str) -> Option<semver::Version> {
    let version = semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()?;
    (version.pre.is_empty() && version.build.is_empty()).then_some(version)
}

fn nightly(tag: &str) -> Option<Vec<u64>> {
    let parts = tag.strip_prefix("nightly-")?.split('-').collect::<Vec<_>>();
    if parts.len() != 5 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return None;
    }
    let n = parts
        .iter()
        .map(|p| p.parse().ok())
        .collect::<Option<Vec<u64>>>()?;
    let leap = n[0] % 4 == 0 && (n[0] % 100 != 0 || n[0] % 400 == 0);
    let days = match n[1] {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    (n[0] >= 2020 && n[2] > 0 && n[2] <= days && n[3] > 0 && n[4] > 0).then_some(n)
}

impl Identity {
    pub fn valid(&self) -> bool {
        match self.channel.as_str() {
            "stable" => stable(&self.tag).is_some(),
            "nightly" => nightly(&self.tag).is_some(),
            _ => false,
        }
    }
    pub fn compare(&self, other: &Self) -> Option<Ordering> {
        if self.channel != other.channel {
            return None;
        }
        match self.channel.as_str() {
            "stable" => Some(stable(&self.tag)?.cmp(&stable(&other.tag)?)),
            "nightly" => Some(nightly(&self.tag)?.cmp(&nightly(&other.tag)?)),
            _ => None,
        }
    }
    pub fn newer_than(&self, other: &Self) -> bool {
        self.compare(other) == Some(Ordering::Greater)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}
#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub draft: bool,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Candidate {
    pub identity: Identity,
    pub archive: Asset,
    pub checksum: Asset,
}

pub fn select(current: &Identity, releases: Vec<Release>) -> Option<Candidate> {
    let mut best: Option<Candidate> = None;
    for release in releases {
        let identity = Identity {
            channel: if release.prerelease {
                "nightly"
            } else {
                "stable"
            }
            .into(),
            tag: release.tag_name,
        };
        if release.draft || !identity.newer_than(current) {
            continue;
        }
        let archive = release
            .assets
            .iter()
            .find(|a| a.name == ARCHIVE && a.size > 0 && a.size <= crate::stage::MAX_ARCHIVE);
        let checksum = release
            .assets
            .iter()
            .find(|a| a.name == format!("{ARCHIVE}.sha256") && a.size <= 1024);
        if let (Some(archive), Some(checksum)) = (archive, checksum) {
            if best
                .as_ref()
                .is_none_or(|b| identity.newer_than(&b.identity))
            {
                best = Some(Candidate {
                    identity,
                    archive: archive.clone(),
                    checksum: checksum.clone(),
                });
            }
        }
    }
    best
}

pub fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("Bono-Updater")
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(1800))
        .https_only(true)
        .build()?)
}

pub async fn check(current: &Identity) -> anyhow::Result<Option<Candidate>> {
    ensure!(current.valid(), "Not an official release build");
    let client = client()?;
    check_pages(
        current,
        &client,
        "https://api.github.com/repos/racebono/bono/releases",
    )
    .await
}

async fn check_pages(
    current: &Identity,
    client: &reqwest::Client,
    endpoint: &str,
) -> anyhow::Result<Option<Candidate>> {
    let mut releases = Vec::new();
    // GitHub's list endpoint is paginated; do not use /latest, which excludes nightlies.
    for page in 1..=100 {
        let response = client
            .get(format!("{endpoint}?per_page=100&page={page}"))
            .header("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(30))
            .send()
            .await?
            .error_for_status()?;
        let batch: Vec<Release> = response.json().await.context("Invalid release response")?;
        let done = batch.len() < 100;
        releases.extend(batch);
        if done {
            return Ok(select(current, releases));
        }
    }
    anyhow::bail!("Release listing exceeded pagination limit; please retry later")
}

pub fn asset_url(asset: &Asset, tag: &str) -> anyhow::Result<()> {
    let url = reqwest::Url::parse(&asset.browser_download_url)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.path() == format!("/racebono/bono/releases/download/{tag}/{}", asset.name)
            && url.query().is_none()
            && url.fragment().is_none(),
        "Unexpected release asset URL"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(channel: &str, tag: &str) -> Identity {
        Identity {
            channel: channel.into(),
            tag: tag.into(),
        }
    }
    #[test]
    fn versions_and_channels() {
        assert!(id("stable", "v1.10.0").newer_than(&id("stable", "1.9.0")));
        assert!(!id("stable", "1.0.0").newer_than(&id("stable", "v1.0.0")));
        assert!(!id("nightly", "nightly-2026-10-31-10-1").newer_than(&id("stable", "1.0.0")));
        assert!(
            id("nightly", "nightly-2026-10-31-10-2")
                .newer_than(&id("nightly", "nightly-2026-10-31-9-8"))
        );
        for bad in [
            "nightly-2026-02-30-1-1",
            "nightly-foo",
            "nightly-2026-13-01-1-1",
        ] {
            assert!(!id("nightly", bad).valid());
        }
        assert!(!id("stable", "1.1.0-beta.1").valid());
    }
    #[tokio::test]
    async fn follows_pages_and_rejects_api_errors() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let draft = serde_json::json!({"tag_name":"v3.0.0","draft":true,"prerelease":false,"assets":[]});
            let page1 = serde_json::to_string(&vec![draft; 100]).unwrap();
            for (page, body) in [page1, "[]".into()].into_iter().enumerate() {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buf = [0; 4096];
                let n = socket.read(&mut buf).unwrap();
                assert!(String::from_utf8_lossy(&buf[..n]).contains(&format!("page={}", page + 1)));
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert!(
            check_pages(&id("stable", "v1.0.0"), &client, &format!("http://{addr}"))
                .await
                .unwrap()
                .is_none()
        );
        worker.join().unwrap();
    }
    #[test]
    fn only_official_asset_urls_are_allowed() {
        for url in [
            "http://github.com/racebono/bono/releases/download/v1.0.0/x",
            "https://evil.example/x",
            "https://github.com/other/bono/releases/download/v1.0.0/x",
        ] {
            assert!(
                asset_url(
                    &Asset {
                        name: "x".into(),
                        browser_download_url: url.into(),
                        size: 1
                    },
                    "v1.0.0"
                )
                .is_err()
            );
        }
    }
    #[test]
    fn selection_skips_incomplete_and_other_channels() {
        let releases = serde_json::from_value(serde_json::json!([
            {"tag_name":"v1.1.0","draft":false,"prerelease":false,"assets":[{"name":ARCHIVE,"size":1,"browser_download_url":"x"},{"name":format!("{ARCHIVE}.sha256"),"size":64,"browser_download_url":"y"}]},
            {"tag_name":"v2.0.0","draft":false,"prerelease":false,"assets":[]},
            {"tag_name":"nightly-2026-10-31-1-1","draft":false,"prerelease":true,"assets":[]}
        ])).unwrap();
        assert_eq!(
            select(&id("stable", "1.0.0"), releases)
                .unwrap()
                .identity
                .tag,
            "v1.1.0"
        );
    }
}
