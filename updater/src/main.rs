#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
fn main() {
    let result = (|| -> anyhow::Result<()> {
        let executable = std::env::current_exe()?;
        let directory = executable
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Missing helper directory"))?;
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        let parent = match args.as_slice() {
            [] => None,
            [flag, pid] if flag == "--install" => Some(pid.parse()?),
            _ => anyhow::bail!("Invalid helper arguments"),
        };
        bono_updater::install::run_helper(directory, parent)
    })();
    if let Err(error) = result {
        eprintln!("Bono update failed: {error:#}");
        std::process::exit(1);
    }
}
