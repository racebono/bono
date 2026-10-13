use bindgen::{
    FieldVisibilityKind,
    callbacks::{DeriveInfo, FieldInfo, ParseCallbacks, TypeKind},
};
use std::{env, path::PathBuf};

fn main() {
    sim_bindings();
}

/// Generates Rust types for the simulator data structures in `include/lambiase`, which the
/// simulator plugins fill in from C or C++.
fn sim_bindings() {
    let include = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("include");
    println!("cargo::rerun-if-changed={}", include.display());

    let mut builder = bindgen::Builder::default()
        // Parsed as C: the C++-only helpers in lambiase.h are behind `__cplusplus`
        .clang_args(["-x", "c", "-std=c11"])
        .clang_arg(format!("-I{}", include.display()))
        .allowlist_type("lambiase_.*")
        // Written by hand in `src/sim/sys.rs`, so their `value` can be a `MaybeUninit`
        .blocklist_type("lambiase_opt_.*")
        .derive_default(true)
        .no_copy("lambiase_string")
        .no_debug("lambiase_string")
        .no_default("lambiase_string")
        .no_hash("lambiase_string")
        .no_partialeq("lambiase_string")
        .parse_callbacks(Box::new(PrivateFields))
        .parse_callbacks(Box::new(OptionalDerives))
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for header in ["forecast.h", "scoring.h", "telemetry.h"] {
        builder = builder.header(include.join("lambiase").join(header).to_string_lossy());
    }

    builder
        .generate()
        .expect("Error generating bindings for include/lambiase")
        .write_to_file(PathBuf::from(env::var("OUT_DIR").unwrap()).join("sim.rs"))
        .expect("Error writing bindings for include/lambiase");
}

/// Types whose fields are private, so outside `src/sim/sys.rs` (where the bindings are included)
/// they're only reachable through the methods written there.
const PRIVATE_FIELDS: &[&str] = &["lambiase_string"];

#[derive(Debug)]
struct PrivateFields;

impl ParseCallbacks for PrivateFields {
    fn field_visibility(&self, info: FieldInfo<'_>) -> Option<FieldVisibilityKind> {
        PRIVATE_FIELDS
            .contains(&info.type_name)
            .then_some(FieldVisibilityKind::Private)
    }
}

/// Structs with no strings, which can be `Copy` (`lambiase_opt_string` owns its string).
const COPY_STRUCTS: &[&str] = &[
    "lambiase_wheel_telemetry",
    "lambiase_car_state",
    "lambiase_telemetry",
];

/// bindgen can't see the traits of the hand-written `lambiase_opt_*` types (they're
/// blocklisted), so it derives nothing for the structs containing them: add the derives back.
#[derive(Debug)]
struct OptionalDerives;

impl ParseCallbacks for OptionalDerives {
    fn add_derives(&self, info: &DeriveInfo<'_>) -> Vec<String> {
        if info.kind != TypeKind::Struct
            || !info.name.starts_with("lambiase_")
            || PRIVATE_FIELDS.contains(&info.name)
        {
            return Vec::new();
        }

        let mut derives = vec!["Debug".to_owned(), "Clone".to_owned()];
        if COPY_STRUCTS.contains(&info.name) {
            derives.push("Copy".to_owned());
        }
        return derives;
    }
}
