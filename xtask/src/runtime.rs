use anyhow::Context;
use std::path::Path;

/// Put native libraries next to bono.exe so packaging does not depend on the SDK's PATH.
pub(crate) async fn copy_windows_runtimes(release: &Path, dist: &Path) -> anyhow::Result<()> {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn packages_native_runtimes() {
        let root = tempfile::tempdir().unwrap();
        let release = root.path().join("release");
        let dist = root.path().join("dist");
        for dir in [&release, &dist] {
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
        copy_windows_runtimes(&release, &dist).await.unwrap();
        for name in ["DirectML.dll", "onnxruntime.dll"] {
            assert_eq!(fs::read_to_string(dist.join(name)).unwrap(), name);
        }
        assert_eq!(fs::read_dir(dist).unwrap().count(), 2);
    }
}

/// Runtime dependencies which the linker imports before Bono can select a backend.
/// Never copy the Windows OS DLLs: only the redistributable CRT and Vulkan loader.
pub(crate) fn copy_platform_runtimes(release: &Path, dist: &Path) -> anyhow::Result<()> {
    let redist = msvc_redist_dir()?;
    let vulkan = std::env::var_os("VULKAN_SDK")
        .map(std::path::PathBuf::from)
        .map(|sdk| sdk.join("Bin/vulkan-1.dll"))
        .filter(|dll| dll.is_file())
        .or_else(|| std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .map(|root| root.join("System32/vulkan-1.dll"))
            .filter(|dll| dll.is_file()))
        .context("Missing x64 Vulkan loader: install the Vulkan runtime or provide VULKAN_SDK/Bin/vulkan-1.dll")?;
    package_platform_runtimes(release, &redist, &vulkan, dist)
}

fn msvc_redist_dir() -> anyhow::Result<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("VCToolsRedistDir") {
        return Ok(dir.into());
    }
    // vswhere works for Community, Professional, Enterprise and Build Tools, including
    // installations outside Program Files. Use the same installation as the C++ tools.
    let vswhere = std::path::PathBuf::from(
        std::env::var_os("ProgramFiles(x86)").context("ProgramFiles(x86) is not set")?,
    )
    .join("Microsoft Visual Studio/Installer/vswhere.exe");
    let output = std::process::Command::new(&vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ])
        .output()
        .context("Cannot locate Visual C++ redistributables; set VCToolsRedistDir")?;
    anyhow::ensure!(
        output.status.success(),
        "vswhere failed; set VCToolsRedistDir"
    );
    let install = String::from_utf8(output.stdout)?;
    anyhow::ensure!(
        !install.trim().is_empty(),
        "No Visual C++ installation found; set VCToolsRedistDir"
    );
    let vc = std::path::PathBuf::from(install.trim()).join("VC");
    let version =
        std::fs::read_to_string(vc.join("Auxiliary/Build/Microsoft.VCRedistVersion.default.txt"))
            .context("Cannot locate the toolchain's CRT redistributables; set VCToolsRedistDir")?;
    Ok(vc.join("Redist/MSVC").join(version.trim()))
}

fn package_platform_runtimes(
    release: &Path,
    redist: &Path,
    vulkan: &Path,
    dist: &Path,
) -> anyhow::Result<()> {
    use std::fs;
    let crt = fs::read_dir(redist.join("x64"))
        .with_context(|| format!("Cannot read x64 CRT directory {}", redist.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.is_dir()
                && path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with("Microsoft.VC") && name.ends_with(".CRT")
                })
        })
        .context("Missing x64 Microsoft.VC*.CRT directory")?;
    // Fail rather than silently depending on the developer's globally installed VC runtime.
    // The LMU plugin uses the bundled redistributable installer in the game process.
    for name in [
        "msvcp140.dll",
        "msvcp140_1.dll",
        "msvcp140_2.dll",
        "vcruntime140.dll",
        "vcruntime140_1.dll",
    ] {
        anyhow::ensure!(
            crt.join(name).is_file(),
            "Missing Visual C++ runtime: {}",
            crt.join(name).display()
        );
    }
    let directml = release.join("DirectML.dll");
    anyhow::ensure!(
        directml.is_file(),
        "Missing build runtime: {}",
        directml.display()
    );
    anyhow::ensure!(
        vulkan.is_file(),
        "Missing Vulkan loader: {}",
        vulkan.display()
    );
    let installer = redist.join("vc_redist.x64.exe");
    anyhow::ensure!(
        installer.is_file(),
        "Missing Visual C++ installer: {}",
        installer.display()
    );
    for directory in [dist.to_path_buf(), dist.join("phonemizer")] {
        fs::create_dir_all(&directory)?;
        // Keep companion CRT libraries from the same redistributable version together.
        for entry in fs::read_dir(&crt)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"))
            {
                fs::copy(entry.path(), directory.join(entry.file_name()))?;
            }
        }
    }
    fs::copy(directml, dist.join("DirectML.dll"))?;
    fs::copy(vulkan, dist.join("vulkan-1.dll"))
        .with_context(|| format!("Cannot package Vulkan loader {}", vulkan.display()))?;
    fs::create_dir_all(dist.join("prerequisites"))?;
    fs::copy(installer, dist.join("prerequisites/vc_redist.x64.exe"))?;
    fs::write(
        dist.join("prerequisites/README.txt"),
        "Bono and its phonemizer include app-local x64 Visual C++ runtime DLLs.\r\n\
         Before using the Le Mans Ultimate plugin, run vc_redist.x64.exe here.\r\n\
         The plugin is loaded by the game, outside Bono's DLL search directory.\r\n\
         Windows and compatible GPU drivers are still required; drivers are not bundled.\r\n\
         Whisper and Piper models are downloaded through Bono's setup into user preferences.\r\n",
    )?;
    Ok(())
}

#[cfg(test)]
mod platform_tests {
    use super::*;
    use std::fs;

    #[test]
    fn packages_each_process_runtime_and_requires_complete_sources() {
        let temp = tempfile::tempdir().unwrap();
        let release = temp.path().join("release");
        let redist = temp.path().join("redist");
        let crt = redist.join("x64/Microsoft.VC145.CRT");
        let dist = temp.path().join("dist");
        fs::create_dir_all(&release).unwrap();
        fs::create_dir_all(&crt).unwrap();
        let dlls = [
            "msvcp140.dll",
            "msvcp140_1.dll",
            "msvcp140_2.dll",
            "vcruntime140.dll",
            "vcruntime140_1.dll",
        ];
        for name in dlls {
            fs::write(crt.join(name), name).unwrap();
        }
        fs::write(redist.join("vc_redist.x64.exe"), "installer").unwrap();
        fs::write(release.join("DirectML.dll"), "directml").unwrap();
        let vulkan = temp.path().join("vulkan-1.dll");
        fs::write(&vulkan, "vulkan").unwrap();
        package_platform_runtimes(&release, &redist, &vulkan, &dist).unwrap();
        for directory in [&dist, &dist.join("phonemizer")] {
            for name in dlls {
                assert_eq!(fs::read_to_string(directory.join(name)).unwrap(), name);
            }
        }
        assert!(!dist.join("plugins").exists());
        assert_eq!(
            fs::read_to_string(dist.join("DirectML.dll")).unwrap(),
            "directml"
        );
        assert_eq!(
            fs::read_to_string(dist.join("vulkan-1.dll")).unwrap(),
            "vulkan"
        );
        assert!(dist.join("prerequisites/vc_redist.x64.exe").is_file());
        for name in dlls {
            fs::remove_file(crt.join(name)).unwrap();
            assert!(
                package_platform_runtimes(&release, &redist, &vulkan, &dist)
                    .unwrap_err()
                    .to_string()
                    .contains(name)
            );
            fs::write(crt.join(name), name).unwrap();
        }
        fs::remove_file(&vulkan).unwrap();
        assert!(
            package_platform_runtimes(&release, &redist, &vulkan, &dist)
                .unwrap_err()
                .to_string()
                .contains("Missing Vulkan loader")
        );
        fs::write(&vulkan, "vulkan").unwrap();
        fs::remove_file(redist.join("vc_redist.x64.exe")).unwrap();
        assert!(
            package_platform_runtimes(&release, &redist, &vulkan, &dist)
                .unwrap_err()
                .to_string()
                .contains("vc_redist.x64.exe")
        );
        fs::write(redist.join("vc_redist.x64.exe"), "installer").unwrap();
        fs::remove_file(release.join("DirectML.dll")).unwrap();
        assert!(
            package_platform_runtimes(&release, &redist, &vulkan, &dist)
                .unwrap_err()
                .to_string()
                .contains("DirectML.dll")
        );
    }
}

#[cfg(all(test, windows))]
mod installed_runtime_tests {
    #[test]
    #[ignore = "requires a Windows release build, Visual C++ redistributables and Vulkan runtime"]
    fn packages_installed_runtime_sources() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let dist = tempfile::tempdir().unwrap();
        super::copy_platform_runtimes(&root.join("target/release"), dist.path()).unwrap();
        for name in [
            "DirectML.dll",
            "vulkan-1.dll",
            "msvcp140.dll",
            "phonemizer/vcruntime140.dll",
            "prerequisites/vc_redist.x64.exe",
        ] {
            assert!(dist.path().join(name).is_file(), "{name}");
        }
    }
}
