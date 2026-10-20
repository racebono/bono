mod runtime;

use anyhow::bail;
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

async fn dist() -> anyhow::Result<()> {
    let root = project_root();
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());

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
    tokio::fs::copy(root.join("target/release").join(&exe), dist.join(&exe)).await?;

    // 4. Copy assets
    copy_dir(root.join("assets"), dist.join("assets")).await?;

    // 5. The phonemizer, in its own folder: it's GPL (it links espeak-ng), so it's shipped as a
    // separate program with its license, next to the espeak-ng data it loads
    let phonemizer = dist.join("phonemizer");
    tokio::fs::create_dir_all(&phonemizer).await?;
    let exe = format!("bono-phonemizer{}", env::consts::EXE_SUFFIX);
    tokio::fs::copy(
        root.join("target/release").join(&exe),
        phonemizer.join(&exe),
    )
    .await?;
    tokio::fs::copy(root.join("phonemizer/COPYING"), phonemizer.join("COPYING")).await?;

    let Some(espeak_data) = newest_espeak_data(&root.join("target/release/build")).await? else {
        bail!("espeak-ng-data not found in espeak-rs-sys's build output");
    };
    copy_dir(espeak_data, phonemizer.join("espeak-ng-data")).await?;

    // 6. Windows app-local runtimes and the LMU plugin (for the game's Plugins folder)
    if cfg!(windows) {
        let cuda = env::var_os("CUDA_PATH")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("CUDA_PATH is required to package CUDA runtime DLLs"))?;
        runtime::copy_windows_runtimes(&root.join("target/release"), &cuda, &dist).await?;
        tokio::fs::create_dir_all(dist.join("plugins")).await?;
        tokio::fs::copy(
            root.join("target/release/bono_lmu.dll"),
            dist.join("plugins/bono_lmu.dll"),
        )
        .await?;
        runtime::copy_platform_runtimes(&root.join("target/release"), &dist)?;
    }

    println!("packaged into {}", dist.display());
    Ok(())
}

/// The most recently built `espeak-rs-sys-*/out/share/espeak-ng-data` in `build_dir`.
async fn newest_espeak_data(build_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let mut newest = None::<(std::time::SystemTime, PathBuf)>;
    let mut it = tokio::fs::read_dir(build_dir).await?;

    while let Some(entry) = it.next_entry().await? {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("espeak-rs-sys-")
        {
            continue;
        }

        let data = entry.path().join("out/share/espeak-ng-data");
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
