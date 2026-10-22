use bindgen::{
    FieldVisibilityKind,
    callbacks::{DeriveInfo, FieldInfo, ParseCallbacks, TypeKind},
};
use std::{
    env,
    fmt::Write,
    path::{Path, PathBuf},
};

const HEADERS: &[&str] = &["forecast.h", "scoring.h", "telemetry.h"];

fn main() {
    let include = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../include");
    println!("cargo::rerun-if-changed={}", include.display());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    sim_bindings(&include, &out);
    sim_columns(&include, &out);
}

/// Generates Rust types for the simulator data structures in `include/bono`, which the
/// simulator plugins fill in from C or C++.
fn sim_bindings(include: &Path, out: &Path) {
    let mut builder = bindgen::Builder::default()
        // Parsed as C: the C++-only helpers in bono.h are behind `__cplusplus`
        .clang_args(["-x", "c", "-std=c11"])
        .clang_arg(format!("-I{}", include.display()))
        .allowlist_type("bono_.*")
        // Written by hand in `src/sim/sys.rs`, so their `value` can be a `MaybeUninit`
        .blocklist_type("bono_opt_.*")
        .derive_default(true)
        .no_copy("bono_string")
        .no_debug("bono_string")
        .no_default("bono_string")
        .no_hash("bono_string")
        .no_partialeq("bono_string")
        .parse_callbacks(Box::new(PrivateFields))
        .parse_callbacks(Box::new(OptionalDerives))
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for header in HEADERS {
        builder = builder.header(include.join("bono").join(header).to_string_lossy());
    }

    builder
        .generate()
        .expect("Error generating bindings for include/bono")
        .write_to_file(out.join("sim.rs"))
        .expect("Error writing bindings for include/bono");
}

/// Turns each `SIM_*_COLUMNS(X)` list in the headers into a Rust macro of the same name in
/// lowercase, which calls back the macro it's given with the columns, the way the C lists call `X`:
/// `sim_forecast_columns!(table!(args))` expands to `table! { args [(f64, session_time, true), ...] }`.
/// Lists without a `compared` flag get `true`. Written to `columns.rs`.
fn sim_columns(include: &Path, out: &Path) {
    let mut code = String::new();
    for header in HEADERS {
        let path = include.join("bono").join(header);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Error reading {}: {e}", path.display()));

        for (name, columns) in column_lists(&text) {
            writeln!(code, "macro_rules! {} {{", name.to_lowercase()).unwrap();
            writeln!(
                code,
                "    ($m:ident!($($args:tt)*)) => {{ $m! {{ $($args)* ["
            )
            .unwrap();
            for (ty, column, compared) in columns {
                writeln!(code, "        ({ty}, {column}, {compared}),").unwrap();
            }
            writeln!(code, "    ] }} }};\n}}").unwrap();
        }
    }

    std::fs::write(out.join("columns.rs"), code).expect("Error writing columns.rs");
}

/// Each `#define SIM_*_COLUMNS(X)` in `text` with its `X(type, name[, compared])` entries.
fn column_lists(text: &str) -> Vec<(String, Vec<(String, String, String)>)> {
    let mut lists = Vec::new();
    let mut lines = text.lines();

    while let Some(line) = lines.next() {
        let Some(define) = line.trim_start().strip_prefix("#define ") else {
            continue;
        };
        let Some((name, mut body)) = define
            .split_once("(X)")
            .filter(|(name, _)| name.starts_with("SIM_") && name.ends_with("_COLUMNS"))
            .map(|(name, body)| (name.to_owned(), body.to_owned()))
        else {
            continue;
        };

        // The definition goes on while lines end in a backslash
        while body.trim_end().ends_with('\\') {
            body.push('\n');
            body.push_str(lines.next().expect("Unterminated column list"));
        }

        let body = strip_comments(&body);
        let mut columns = Vec::new();
        let mut rest = body.as_str();
        while let Some(at) = rest.find("X(") {
            let starts_entry = rest[..at]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_');
            rest = &rest[at + 2..];
            if !starts_entry {
                continue;
            }

            let (args, after) = rest.split_once(')').expect("Unterminated column entry");
            rest = after;
            let args = args.split(',').map(str::trim).collect::<Vec<_>>();
            match args[..] {
                [ty, column] => columns.push((ty.into(), column.into(), "true".into())),
                [ty, column, compared] => columns.push((ty.into(), column.into(), compared.into())),
                _ => panic!("Unexpected entry in {name}: X({})", args.join(", ")),
            }
        }

        lists.push((name, columns));
    }

    return lists;
}

fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        let end = rest[start..].find("*/").expect("Unterminated comment");
        rest = &rest[start + end + 2..];
    }
    out.push_str(rest);
    return out;
}

/// Types whose fields are private, so outside `src/sim/sys.rs` (where the bindings are included)
/// they're only reachable through the methods written there.
const PRIVATE_FIELDS: &[&str] = &["bono_string"];

#[derive(Debug)]
struct PrivateFields;

impl ParseCallbacks for PrivateFields {
    fn field_visibility(&self, info: FieldInfo<'_>) -> Option<FieldVisibilityKind> {
        PRIVATE_FIELDS
            .contains(&info.type_name)
            .then_some(FieldVisibilityKind::Private)
    }
}

/// Structs with no strings, which can be `Copy` (`bono_opt_string` owns its string).
const COPY_STRUCTS: &[&str] = &["bono_wheel_telemetry", "bono_car_state", "bono_telemetry"];

/// bindgen can't see the traits of the hand-written `bono_opt_*` types (they're
/// blocklisted), so it derives nothing for the structs containing them: add the derives back.
#[derive(Debug)]
struct OptionalDerives;

impl ParseCallbacks for OptionalDerives {
    fn add_derives(&self, info: &DeriveInfo<'_>) -> Vec<String> {
        if info.kind != TypeKind::Struct
            || !info.name.starts_with("bono_")
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
