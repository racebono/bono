# Vendored sources

Only the files `build.rs` compiles or includes, unmodified, each with its license.

| Folder | Project | Version | License |
| --- | --- | --- | --- |
| `sqlite_zstd_vfs` | https://github.com/mlin/sqlite_zstd_vfs (`src/SQLiteNestedVFS.h`, `src/zstd_vfs.h`) | `bf84ddb30a352efdfaa4ad40b2e9a29a0b83de84` | Apache-2.0 |
| `sqlite_web_vfs` | https://github.com/mlin/sqlite_web_vfs (`src/SQLiteVFS.h`, `src/ThreadPool.h`) | `2a435382d2d9088920013439ace7467624ca3925` | Apache-2.0 |
| `SQLiteCpp` | https://github.com/mlin/SQLiteCpp (`include/`, `src/`) | `c709740566d49db87b4260b020b3322a27cf860e` | MIT |
| `readerwriterqueue` | https://github.com/cameron314/readerwriterqueue (`readerwriterqueue.h`, `atomicops.h`) | `8b2176698e9bdaba653cdc20c32b54737a934b47` (v1.0.7) | Simplified BSD |
| `sqlite3` | `sqlite3.h` of SQLite 3.53.2, as bundled by `libsqlite3-sys` 0.38.2 | 3.53.2 | Public domain |
| `zstd` | `zstd.h`, `zdict.h`, `zstd_errors.h` of zstd 1.5.7, as bundled by `zstd-sys` 2.1.2 | 1.5.7 | BSD |

The commits are the ones sqlite_zstd_vfs's `CMakeLists.txt` pins. The SQLite and zstd headers
must be no newer than the libraries `rusqlite` and `zstd-sys` link: `register` checks it.
