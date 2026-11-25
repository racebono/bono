mod runtime;

use anyhow::{Context, bail};
use futures::{TryStreamExt, stream::FuturesUnordered};
use std::{
    env,
    path::{Path, PathBuf},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match env::args().nth(1).as_deref() {
        Some("dist") => dist().await,
        _ => {
            eprintln!("usage: cargo xtask dist");
            std::process::exit(1);
        }
    }
}

fn project_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

async fn release_dir(cargo: &str, root: &Path) -> anyhow::Result<PathBuf> {
    let output = tokio::process::Command::new(cargo)
        .current_dir(root)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .await
        .context("failed to run cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let target = metadata["target_directory"]
        .as_str()
        .context("cargo metadata did not return target_directory")?;
    Ok(PathBuf::from(target).join("release"))
}

async fn dist() -> anyhow::Result<()> {
    let root = project_root();
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    let release = release_dir(&cargo, root).await?;

    // 1. Build the game and its phonemizer in release, and on Windows the Le Mans Ultimate plugin
    let mut packages = vec!["--package", "bono", "--package", "bono-phonemizer"];
    if cfg!(windows) {
        packages.extend(["--package", "bono-lmu"]);
    }

    let status = tokio::process::Command::new(&cargo)
        .current_dir(&root)
        .args(["build", "--locked", "--release"])
        .args(packages)
        .status()
        .await?;

    if !status.success() {
        bail!("cargo build failed");
    }

    // 2. Fresh dist/ folder
    let dist = root.join("dist");
    let _ = tokio::fs::remove_dir_all(&dist).await;
    tokio::fs::create_dir_all(&dist).await?;

    // 3. Copy the binary (handles .exe on Windows)
    let exe = format!("bono{}", env::consts::EXE_SUFFIX);
    tokio::fs::copy(release.join(&exe), dist.join(&exe)).await?;

    // 4. Copy assets
    copy_dir(root.join("assets"), dist.join("assets")).await?;

    // 5. The phonemizer, in its own folder: it's GPL (it links espeak-ng), so it's shipped as a
    // separate program with its license, next to the espeak-ng data it loads
    let phonemizer = dist.join("phonemizer");
    tokio::fs::create_dir_all(&phonemizer).await?;
    let exe = format!("bono-phonemizer{}", env::consts::EXE_SUFFIX);
    tokio::fs::copy(release.join(&exe), phonemizer.join(&exe)).await?;
    tokio::fs::copy(root.join("phonemizer/COPYING"), phonemizer.join("COPYING")).await?;

    let Some(espeak_data) = newest_espeak_data(&release.join("build")).await? else {
        bail!("espeak-ng-data not found in espeak-rs-sys's build output");
    };
    copy_dir(espeak_data, phonemizer.join("espeak-ng-data")).await?;

    // 6. Windows app-local runtimes and the LMU plugin (for the game's Plugins folder)
    if cfg!(windows) {
        runtime::copy_windows_runtimes(&release, &dist).await?;
        tokio::fs::create_dir_all(dist.join("plugins")).await?;
        tokio::fs::copy(
            release.join("bono_lmu.dll"),
            dist.join("plugins/bono_lmu.dll"),
        )
        .await?;
        runtime::copy_platform_runtimes(&release, &dist)?;
    }

    println!("packaged into {}", dist.display());
    Ok(())
}

/// The most recently built espeak-rs-sys `out/share/espeak-ng-data` in `build_dir`: in
/// `espeak-rs-sys-{hash}/out`, or `espeak-rs-sys/{hash}/out` where Cargo nests build outputs by
/// package.
async fn newest_espeak_data(build_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let mut newest = None::<(std::time::SystemTime, PathBuf)>;
    let mut builds = Vec::new();
    let mut it = tokio::fs::read_dir(build_dir).await?;

    while let Some(entry) = it.next_entry().await? {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "espeak-rs-sys" {
            let mut nested = tokio::fs::read_dir(entry.path()).await?;
            while let Some(build) = nested.next_entry().await? {
                builds.push(build.path());
            }
        } else if name.starts_with("espeak-rs-sys-") {
            builds.push(entry.path());
        }
    }

    for build in builds {
        let data = build.join("out/share/espeak-ng-data");
        let Ok(metadata) = tokio::fs::metadata(data.join("phondata")).await else {
            continue;
        };

        let modified = metadata.modified()?;
        if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
            newest = Some((modified, data));
        }
    }

    Ok(newest.map(|(_, data)| data))
}

fn copy_dir(
    src: PathBuf,
    dst: PathBuf,
) -> impl Future<Output = std::io::Result<()>> + Send + 'static {
    return async move {
        tokio::fs::create_dir_all(&dst).await?;
        let mut it = tokio::fs::read_dir(src).await?;
        let mut fut = FuturesUnordered::new();

        while let Some(entry) = it.next_entry().await? {
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if entry.file_type().await?.is_dir() {
                fut.push(tokio::spawn(copy_dir(from, to)));
            } else {
                fut.push(tokio::task::spawn_blocking(|| {
                    std::fs::copy(from, to).map(|_| ())
                }));
            }
        }

        while fut.try_next().await?.transpose()?.is_some() {}
        Ok(())
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn release_dir_uses_cargo_target_directory() {
        let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join(".cargo")).unwrap();
        std::fs::create_dir(project.path().join("src")).unwrap();
        std::fs::write(project.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(
            project.path().join("Cargo.toml"),
            r#"[package]
name = "target-dir-test"
version = "0.1.0"
[workspace]
"#,
        )
        .unwrap();
        std::fs::write(
            project.path().join(".cargo/config.toml"),
            r#"[build]
target-dir = "custom-target"
"#,
        )
        .unwrap();
        let release = release_dir(&cargo, project.path()).await.unwrap();
        let target = env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("custom-target"));
        assert_eq!(release, project.path().join(target).join("release"));
    }

    #[tokio::test]
    async fn finds_espeak_data_in_either_build_layout() {
        for build in ["espeak-rs-sys-0123abcd", "espeak-rs-sys/0123abcd"] {
            let dir = tempfile::tempdir().unwrap();
            let data = dir.path().join(build).join("out/share/espeak-ng-data");
            std::fs::create_dir_all(&data).unwrap();
            std::fs::write(data.join("phondata"), "").unwrap();
            // Another package's output, and a build without the data
            std::fs::create_dir_all(dir.path().join("espeak-rs-sys-ffff/out")).unwrap();
            std::fs::create_dir_all(dir.path().join("other-sys/0123abcd/out/share")).unwrap();

            let found = newest_espeak_data(dir.path()).await.unwrap();
            assert_eq!(found, Some(data), "{build}");
        }
    }
}
