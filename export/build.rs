fn main() {
    println!("cargo:rerun-if-changed=src/register.cc");
    println!("cargo:rerun-if-changed=vendor");

    let sqlitecpp = [
        "Backup.cpp",
        "Column.cpp",
        "Database.cpp",
        "Exception.cpp",
        "Savepoint.cpp",
        "Statement.cpp",
        "Transaction.cpp",
    ];
    cc::Build::new()
        .cpp(true)
        .std("c++14")
        .files(sqlitecpp.map(|file| format!("vendor/SQLiteCpp/src/{file}")))
        .file("src/register.cc")
        .include("vendor/sqlite3")
        .include("vendor/zstd")
        .include("vendor/SQLiteCpp/include")
        .include("vendor/readerwriterqueue")
        .include("vendor/sqlite_web_vfs")
        .include("vendor/sqlite_zstd_vfs")
        // Upstream builds a release build; its debug build checks with costly asserts
        .define("NDEBUG", None)
        // C++ exceptions, which the VFS throws and catches (MSVC needs it asked for)
        .flag_if_supported("/EHsc")
        // Third-party code: its warnings aren't ours to fix
        .warnings(false)
        .cargo_warnings(false)
        .compile("bono_zstd_vfs");
}
