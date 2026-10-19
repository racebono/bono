use anyhow::{Context, bail};
use std::path::Path;

/// Put native libraries next to bono.exe so packaging does not depend on the SDK's PATH.
pub(crate) async fn copy_windows_runtimes(
    release: &Path,
    cuda: &Path,
    dist: &Path,
) -> anyhow::Result<()> {
    let mut entries = tokio::fs::read_dir(release).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type().await?.is_file()
            && name.to_ascii_lowercase().ends_with(".dll")
            && !name.eq_ignore_ascii_case("bono_lmu.dll")
        {
            tokio::fs::copy(entry.path(), dist.join(name.as_ref())).await?;
        }
    }

    // CUDA 13 uses bin/x64; older toolkits put their x64 runtimes directly in bin.
    let bin = if cuda.join("bin/x64").is_dir() {
        cuda.join("bin/x64")
    } else {
        cuda.join("bin")
    };
    let mut found = [false; 3];
    let prefixes = ["cudart64_", "cublas64_", "cublaslt64_"];
    let mut entries = tokio::fs::read_dir(&bin)
        .await
        .with_context(|| format!("Cannot read CUDA runtime directory {}", bin.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let lower = name.to_string_lossy().to_ascii_lowercase();
        if !entry.file_type().await?.is_file() || !lower.ends_with(".dll") {
            continue;
        }
        for (index, prefix) in prefixes.iter().enumerate() {
            if lower.starts_with(prefix) {
                tokio::fs::copy(entry.path(), dist.join(&name)).await?;
                found[index] = true;
            }
        }
    }
    for (present, prefix) in found.into_iter().zip(prefixes) {
        if !present {
            bail!("Missing CUDA runtime: {prefix}*.dll in {}", bin.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    async fn package(
        layout: &str,
        missing: Option<&str>,
    ) -> (tempfile::TempDir, anyhow::Result<()>) {
        let root = tempfile::tempdir().unwrap();
        let release = root.path().join("release");
        let cuda = root.path().join("cuda");
        let bin = cuda.join(layout);
        let dist = root.path().join("dist");
        for dir in [&release, &bin, &dist] {
            fs::create_dir_all(dir).unwrap();
        }
        for name in [
            "DirectML.dll",
            "onnxruntime.dll",
            "BONO_LMU.DLL",
            "bono.exe",
        ] {
            fs::write(release.join(name), name).unwrap();
        }
        fs::create_dir(release.join("directory.dll")).unwrap();
        for name in [
            "cudart64_13.dll",
            "cublas64_13.dll",
            "cublasLt64_13.dll",
            "unrelated.dll",
        ] {
            if missing != Some(name) {
                fs::write(bin.join(name), name).unwrap();
            }
        }
        let result = copy_windows_runtimes(&release, &cuda, &dist).await;
        (root, result)
    }

    #[tokio::test]
    async fn packages_native_and_cuda_runtimes_for_both_toolkit_layouts() {
        for layout in ["bin/x64", "bin"] {
            let (root, result) = package(layout, None).await;
            result.unwrap();
            let dist = root.path().join("dist");
            for name in [
                "DirectML.dll",
                "onnxruntime.dll",
                "cudart64_13.dll",
                "cublas64_13.dll",
                "cublasLt64_13.dll",
            ] {
                assert_eq!(fs::read_to_string(dist.join(name)).unwrap(), name);
            }
            assert_eq!(fs::read_dir(dist).unwrap().count(), 5);
        }
    }

    #[tokio::test]
    async fn rejects_each_missing_cuda_runtime_family() {
        for name in ["cudart64_13.dll", "cublas64_13.dll", "cublasLt64_13.dll"] {
            let (_root, result) = package("bin/x64", Some(name)).await;
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Missing CUDA runtime:")
            );
        }
    }

    #[tokio::test]
    async fn rejects_missing_cuda_directory_with_its_path() {
        let (root, result) = package("bin/x64", None).await;
        result.unwrap();
        let cuda = root.path().join("absent-sdk");
        let error = copy_windows_runtimes(
            &root.path().join("release"),
            &cuda,
            &root.path().join("dist"),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Cannot read CUDA runtime directory")
        );
        assert!(error.to_string().contains("absent-sdk"));
    }

    #[tokio::test]
    async fn does_not_fill_missing_x64_runtimes_from_another_architecture() {
        let (root, result) = package("bin/x64", Some("cublas64_13.dll")).await;
        assert!(result.is_err());
        fs::write(root.path().join("cuda/bin/cublas64_13.dll"), "wrong DLL").unwrap();
        let error = copy_windows_runtimes(
            &root.path().join("release"),
            &root.path().join("cuda"),
            &root.path().join("dist"),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Missing CUDA runtime: cublas64_*.dll")
        );
    }
}
