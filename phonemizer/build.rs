fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    // espeak-ng looks for its data in the registry (`RegOpenKeyExA`), but espeak-rs-sys doesn't
    // link the library that has it
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        println!("cargo::rustc-link-lib=advapi32");
    }
}
