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

    // 1. Build the game in release
    let status = tokio::process::Command::new(&cargo)
        .current_dir(&root)
        .args(["build", "--release", "--package", "lambiase"])
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
    let exe = format!("lambiase{}", env::consts::EXE_SUFFIX);
    tokio::fs::copy(root.join("target/release").join(&exe), dist.join(&exe)).await?;

    // 4. Copy assets
    copy_dir(root.join("assets"), dist.join("assets")).await?;

    println!("packaged into {}", dist.display());
    Ok(())
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
