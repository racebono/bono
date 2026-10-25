use std::{
    env::{var, var_os},
    path::{Path, PathBuf},
};

pub fn main() -> std::io::Result<()> {
    let out_dir = PathBuf::from(var_os("OUT_DIR").unwrap());
    // Only rerun when a build input changes.
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=BONO_COMMIT");

    // The app's version, with the commit as build metadata (`0.1.0+abc1234`) in nightly builds,
    // which set `BONO_COMMIT`
    let mut version = var("CARGO_PKG_VERSION").unwrap();
    if let Some(commit) = var("BONO_COMMIT").ok().filter(|commit| !commit.is_empty()) {
        version = format!("{version}+{}", &commit[..commit.len().min(7)]);
    }
    println!("cargo::rustc-env=BONO_VERSION={version}");

    if var_os("CARGO_CFG_WINDOWS").is_some() {
        race_room(&out_dir)?;
        icon();
    }

    Ok(())
}

// Embeds the favicon as the executable's icon
fn icon() {
    println!("cargo::rerun-if-changed=assets/favicon.ico");
    winresource::WindowsResource::new()
        .set_icon("assets/favicon.ico")
        .compile()
        .expect("Unable to embed the executable's icon");
}

fn race_room(out_dir: &Path) -> std::io::Result<()> {
    let header = "include/r3e/r3e.h";
    println!("cargo::rerun-if-changed={header}");

    // The bindgen::Builder is the main entry point
    // to bindgen, and lets you build up options for
    // the resulting bindings.
    let bindings = bindgen::Builder::default()
        // The input header we would like to generate
        // bindings for.
        .header(header)
        .allowlist_item("r3e_.*")
        .allowlist_item("R3E_.*")
        .prepend_enum_name(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        // Finish the builder and generate the bindings.
        .generate()
        // Unwrap the Result and panic on failure.
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    bindings.write_to_file(out_dir.join("rrre.rs"))?;

    Ok(())
}
