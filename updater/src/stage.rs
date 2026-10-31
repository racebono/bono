use crate::{
    BUNDLE, HELPER, lock, no_links, read_json,
    release::{Candidate, Identity},
    safe_path, write_json,
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::io::AsyncWriteExt;

pub const MAX_ARCHIVE: u64 = 4 * 1024 * 1024 * 1024;
const MAX_EXPANDED: u64 = 12 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bundle {
    pub identity: Identity,
    pub files: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ready {
    pub generation: String,
    pub identity: Identity,
}

pub fn hash(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn validate_bundle(root: &Path, expected: &Identity) -> anyhow::Result<Bundle> {
    no_links(&root.join(BUNDLE))?;
    let bundle: Bundle = read_json(&root.join(BUNDLE))?;
    ensure!(
        &bundle.identity == expected && expected.valid(),
        "Bundle release identity mismatch"
    );
    for required in [
        "bono.exe",
        HELPER,
        "phonemizer/bono-phonemizer.exe",
        "plugins/bono_lmu.dll",
    ] {
        ensure!(
            bundle.files.contains_key(required),
            "Bundle missing {required}"
        );
    }
    ensure!(
        bundle.files.keys().any(|p| p.starts_with("assets/")),
        "Bundle missing assets"
    );
    ensure!(
        bundle
            .files
            .keys()
            .any(|p| p.ends_with(".dll") && !p.contains('/')),
        "Bundle missing runtime DLLs"
    );
    let mut seen = BTreeSet::new();
    for (name, digest) in &bundle.files {
        safe_path(name)?;
        ensure!(
            name.to_ascii_lowercase() != BUNDLE && seen.insert(name.to_ascii_lowercase()),
            "Conflicting bundle path"
        );
        let path = root.join(name);
        no_links(&path)?;
        ensure!(
            fs::metadata(&path)?.is_file() && hash(&path)? == *digest,
            "Bundle file checksum mismatch: {name}"
        );
    }
    Ok(bundle)
}

pub fn ready(root: &Path, installed: &Identity) -> anyhow::Result<Option<Ready>> {
    if !root.join("ready.json").exists() {
        return Ok(None);
    }
    let ready: Ready = read_json(&root.join("ready.json"))?;
    generation_path(root, &ready)?;
    if !ready.identity.newer_than(installed) {
        return Ok(None);
    }
    validate_bundle(
        &generation_path(root, &ready)?.join("bundle"),
        &ready.identity,
    )?;
    Ok(Some(ready))
}

/// Restore installable state under the update lock. Cleanup follows the durable pointer,
/// even when that generation belongs to another channel or is no longer newer than this build.
pub fn restore(root: &Path, installed: &Identity) -> anyhow::Result<Option<Ready>> {
    let durable: Option<Ready> = if root.join("ready.json").exists() {
        Some(read_json(&root.join("ready.json"))?)
    } else {
        None
    };
    let installable = ready(root, installed)?;
    cleanup(root, durable.as_ref())?;
    Ok(installable)
}

pub fn generation_path(root: &Path, ready: &Ready) -> anyhow::Result<PathBuf> {
    safe_path(&ready.generation)?;
    ensure!(
        ready.generation.starts_with("generation-") && !ready.generation.contains('/'),
        "Invalid generation"
    );
    let path = root.join(&ready.generation);
    no_links(&path)?;
    Ok(path)
}

pub fn cleanup(root: &Path, keep: Option<&Ready>) -> anyhow::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("generation-") && keep.is_none_or(|r| r.generation != name) {
            no_links(&entry.path())?;
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            }
        }
    }
    Ok(())
}

fn extract(archive: &Path, dest: &Path) -> anyhow::Result<()> {
    let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
    ensure!(zip.len() <= 100_000, "Too many archive entries");
    let mut seen = BTreeSet::new();
    let mut expanded = 0u64;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().trim_end_matches('/').to_owned();
        safe_path(&name)?;
        ensure!(
            entry.enclosed_name().is_some() && seen.insert(name.to_ascii_lowercase()),
            "Conflicting archive path"
        );
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            ensure!(
                kind == 0 || kind == 0o100000 || kind == 0o040000,
                "Unsupported archive entry"
            );
        }
        expanded = expanded
            .checked_add(entry.size())
            .context("Archive size overflow")?;
        ensure!(
            expanded <= MAX_EXPANDED,
            "Archive expands beyond size limit"
        );
        let target = dest.join(&name);
        no_links(&target)?;
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        fs::create_dir_all(target.parent().unwrap())?;
        let expected = entry.size();
        let mut output = File::create(&target)?;
        let copied = std::io::copy(&mut (&mut entry).take(expected + 1), &mut output)?;
        ensure!(copied == expected, "Incorrect extracted file size");
        output.flush()?;
        output.sync_all()?;
    }
    Ok(())
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    max: u64,
    progress: Option<&AtomicU64>,
) -> anyhow::Result<u64> {
    let mut response = client.get(url).send().await?.error_for_status()?;
    ensure!(
        response.content_length().is_none_or(|n| n <= max),
        "Download exceeds size limit"
    );
    let mut file = tokio::fs::File::create(path).await?;
    let mut total = 0;
    while let Some(bytes) = response.chunk().await? {
        total += bytes.len() as u64;
        ensure!(total <= max, "Download exceeds size limit");
        file.write_all(&bytes).await?;
        if let Some(progress) = progress {
            progress.store(total, Ordering::Relaxed);
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(total)
}

pub async fn download(
    root: PathBuf,
    candidate: Candidate,
    progress: Arc<AtomicU64>,
) -> anyhow::Result<Ready> {
    crate::release::asset_url(&candidate.archive, &candidate.identity.tag)?;
    crate::release::asset_url(&candidate.checksum, &candidate.identity.tag)?;
    ensure!(candidate.identity.valid(), "Invalid update identity");
    let setup_root = root.clone();
    let (guard, generation) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let guard = lock(&setup_root)?;
        let generation = tempfile::Builder::new()
            .prefix("generation-")
            .tempdir_in(&setup_root)?;
        Ok((guard, generation))
    })
    .await??;
    let client = crate::release::client()?;
    fetch(
        &client,
        &candidate.checksum.browser_download_url,
        &generation.path().join("checksum"),
        1024,
        None,
    )
    .await?;
    let actual_size = fetch(
        &client,
        &candidate.archive.browser_download_url,
        &generation.path().join("archive.zip"),
        MAX_ARCHIVE,
        Some(&progress),
    )
    .await?;
    ensure!(
        actual_size == candidate.archive.size,
        "Incomplete archive download"
    );
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        promote(&root, generation, candidate.identity)
    })
    .await?
}

fn promote(
    root: &Path,
    generation: tempfile::TempDir,
    identity: Identity,
) -> anyhow::Result<Ready> {
    let archive = generation.path().join("archive.zip");
    let checksum = fs::read_to_string(generation.path().join("checksum"))?;
    let expected = checksum
        .split_whitespace()
        .next()
        .context("Empty checksum")?;
    ensure!(
        expected.len() == 64
            && expected.bytes().all(|b| b.is_ascii_hexdigit())
            && hash(&archive)?.eq_ignore_ascii_case(expected),
        "Download checksum mismatch"
    );
    fs::create_dir(generation.path().join("bundle"))?;
    extract(&archive, &generation.path().join("bundle"))?;
    validate_bundle(&generation.path().join("bundle"), &identity)?;
    // Persist the generation BEFORE switching ready.json. Failed promotion leaves the old ready pointer intact.
    let path = generation.keep();
    let ready = Ready {
        generation: path.file_name().unwrap().to_string_lossy().into_owned(),
        identity: identity,
    };
    write_json(&root.join("ready.json"), &ready)?;
    // Cleanup failure must not turn a committed, valid download into a failed download.
    let _ = cleanup(&root, Some(&ready));
    Ok(ready)
}

/// Package-owned files only; used by xtask and installation rollback.
pub fn manifest(root: &Path, identity: Identity) -> anyhow::Result<Bundle> {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> anyhow::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            no_links(&entry.path())?;
            if entry.file_type()?.is_dir() {
                walk(root, &entry.path(), files)?;
            } else {
                let name = entry
                    .path()
                    .strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/");
                safe_path(&name)?;
                if name != BUNDLE {
                    files.insert(name, hash(&entry.path())?);
                }
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files)?;
    Ok(Bundle { identity, files })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_windows_aliases_and_traversal() {
        for name in [
            "../a", "/a", "C:/x", "a\\x", "a/../x", "a:ads", "a.", "CON.txt", "a//b",
        ] {
            assert!(safe_path(name).is_err(), "{name}");
        }
        assert!(safe_path("assets/test.json").is_ok());
    }
    #[test]
    fn ready_commit_and_cleanup_preserve_previous_download() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for name in ["generation-old", "generation-partial", "unrelated"] {
            fs::create_dir(root.join(name)).unwrap();
        }
        let old = Ready {
            generation: "generation-old".into(),
            identity: Identity {
                channel: "stable".into(),
                tag: "v2.0.0".into(),
            },
        };
        write_json(&root.join("ready.json"), &old).unwrap();
        assert_eq!(
            read_json::<Ready>(&root.join("ready.json"))
                .unwrap()
                .generation,
            "generation-old"
        );
        cleanup(root, Some(&old)).unwrap();
        assert!(root.join("generation-old").exists());
        assert!(!root.join("generation-partial").exists());
        assert!(root.join("unrelated").exists());
    }
    fn downloaded(root: &Path, identity: Identity, corrupt: bool) -> tempfile::TempDir {
        let bundle = tempfile::tempdir().unwrap();
        for name in [
            "bono.exe",
            HELPER,
            "runtime.dll",
            "assets/a",
            "plugins/bono_lmu.dll",
            "phonemizer/bono-phonemizer.exe",
        ] {
            let path = bundle.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "payload").unwrap();
        }
        write_json(
            &bundle.path().join(BUNDLE),
            &manifest(bundle.path(), identity).unwrap(),
        )
        .unwrap();
        let generation = tempfile::Builder::new()
            .prefix("generation-")
            .tempdir_in(root)
            .unwrap();
        let archive = generation.path().join("archive.zip");
        let mut zip = zip::ZipWriter::new(File::create(&archive).unwrap());
        let files = manifest(
            bundle.path(),
            Identity {
                channel: "development".into(),
                tag: String::new(),
            },
        )
        .unwrap()
        .files;
        for name in files
            .keys()
            .map(String::as_str)
            .chain(std::iter::once(BUNDLE))
        {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&fs::read(bundle.path().join(name)).unwrap())
                .unwrap();
        }
        zip.finish().unwrap();
        fs::write(
            generation.path().join("checksum"),
            if corrupt {
                "0".repeat(64)
            } else {
                hash(&archive).unwrap()
            },
        )
        .unwrap();
        generation
    }
    #[test]
    fn validated_replacement_commits_before_removing_old_generation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let installed = Identity {
            channel: "stable".into(),
            tag: "v1.0.0".into(),
        };
        let v2 = Identity {
            channel: "stable".into(),
            tag: "v2.0.0".into(),
        };
        let old = promote(root, downloaded(root, v2.clone(), false), v2.clone()).unwrap();
        let v3 = Identity {
            channel: "stable".into(),
            tag: "v3.0.0".into(),
        };
        assert!(promote(root, downloaded(root, v3.clone(), true), v3.clone()).is_err());
        assert_eq!(
            ready(root, &installed).unwrap().unwrap().generation,
            old.generation
        );
        assert!(root.join(&old.generation).exists());
        let new = promote(root, downloaded(root, v3.clone(), false), v3.clone()).unwrap();
        assert!(!root.join(old.generation).exists());
        assert_eq!(
            ready(root, &installed).unwrap().unwrap().generation,
            new.generation
        );
        assert!(ready(root, &v3).unwrap().is_none());
        assert!(
            ready(
                root,
                &Identity {
                    channel: "nightly".into(),
                    tag: "nightly-2026-10-31-1-1".into()
                }
            )
            .unwrap()
            .is_none()
        );
    }
    #[test]
    fn rejects_unsafe_and_conflicting_archive_entries() {
        for names in [
            vec!["../escape"],
            vec!["assets/A", "assets/a"],
            vec!["assets/a", "assets/a/b"],
            vec!["file:ads"],
        ] {
            let temp = tempfile::tempdir().unwrap();
            let archive = temp.path().join("test.zip");
            let mut zip = zip::ZipWriter::new(File::create(&archive).unwrap());
            for name in names {
                zip.start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(b"x").unwrap();
            }
            zip.finish().unwrap();
            assert!(extract(&archive, &temp.path().join("out")).is_err());
            assert!(!temp.path().join("escape").exists());
        }
    }
    #[tokio::test]
    async fn network_failures_and_size_limits_are_errors() {
        use std::io::{Read, Write};
        for response in [
            "HTTP/1.1 500 Error\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nshort",
            "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n",
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let worker = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buf = [0; 4096];
                let _ = socket.read(&mut buf);
                socket.write_all(response.as_bytes()).unwrap();
            });
            let temp = tempfile::tempdir().unwrap();
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            assert!(
                fetch(
                    &client,
                    &format!("http://{addr}"),
                    &temp.path().join("file"),
                    100,
                    None
                )
                .await
                .is_err()
            );
            worker.join().unwrap();
        }
    }
    #[test]
    fn operation_lock_excludes_other_instances() {
        let temp = tempfile::tempdir().unwrap();
        let guard = lock(temp.path()).unwrap();
        assert!(lock(temp.path()).is_err());
        drop(guard);
        assert!(lock(temp.path()).is_ok());
    }

    #[test]
    fn restoring_another_channel_preserves_durable_generation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let stable = Identity {
            channel: "stable".into(),
            tag: "v2.0.0".into(),
        };
        let staged = promote(
            root,
            downloaded(root, stable.clone(), false),
            stable.clone(),
        )
        .unwrap();
        let nightly = Identity {
            channel: "nightly".into(),
            tag: "nightly-2026-10-31-1-1".into(),
        };
        assert!(restore(root, &nightly).unwrap().is_none());
        assert!(root.join(&staged.generation).exists());
        assert_eq!(
            read_json::<Ready>(&root.join("ready.json"))
                .unwrap()
                .generation,
            staged.generation
        );
        let old_stable = Identity {
            channel: "stable".into(),
            tag: "v1.0.0".into(),
        };
        assert_eq!(
            restore(root, &old_stable).unwrap().unwrap().generation,
            staged.generation
        );
        assert!(restore(root, &stable).unwrap().is_none());
        assert!(root.join(&staged.generation).exists());
    }
}
