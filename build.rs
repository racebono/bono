use std::{
    env::var_os,
    path::{Path, PathBuf},
};

pub fn main() -> Result<(), ureq::Error> {
    let out_dir = PathBuf::from(var_os("OUT_DIR").unwrap());
    // Without it, any change to the package reruns this, downloading the header again
    println!("cargo::rerun-if-changed=build.rs");

    if var_os("CARGO_CFG_WINDOWS").is_some() {
        race_room(&out_dir)?;
    }

    Ok(())
}

fn race_room(out_dir: &Path) -> Result<(), ureq::Error> {
    let header = ureq::get(
        "https://raw.githubusercontent.com/kwstudios-sweden/r3e-api/master/sample-c/src/r3e.h",
    )
    .call()?
    .into_body()
    .read_to_string()?;

    // The bindgen::Builder is the main entry point
    // to bindgen, and lets you build up options for
    // the resulting bindings.
    let bindings = bindgen::Builder::default()
        // The input header we would like to generate
        // bindings for.
        .header_contents("r3e.h", &header)
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
