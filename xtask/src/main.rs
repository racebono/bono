// xtask/src/main.rs
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> Result<()> {
    match env::args().nth(1).as_deref() {
        Some("dist") => dist(),
        _ => {
            eprintln!("usage: cargo xtask dist");
            std::process::exit(1);
        }
    }
}

fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn dist() -> Result<()> {
    let root = project_root();
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    // 1. Build the game in release
    let status = Command::new(&cargo)
        .current_dir(&root)
        .args(["build", "--release", "--package", "lambiase"])
        .status()?;

    if !status.success() {
        return Err("cargo build failed".into());
    }

    // 2. Fresh dist/ folder
    let dist = root.join("dist");
    let _ = fs::remove_dir_all(&dist);
    fs::create_dir_all(&dist)?;

    // 3. Copy the binary (handles .exe on Windows)
    let exe = format!("lambiase{}", env::consts::EXE_SUFFIX);
    fs::copy(root.join("target/release").join(&exe), dist.join(&exe))?;

    // 4. Copy assets
    copy_dir(&root.join("assets"), &dist.join("assets"))?;

    println!("packaged into {}", dist.display());
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}
