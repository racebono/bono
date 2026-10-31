// Registers sqlite_zstd_vfs's VFS with the SQLite the program links, instead of loading it as an
// extension (`zstd_vfs.cc` upstream).
// The VFS headers leave choosing sqlite3.h (linked) or sqlite3ext.h (an extension) to their includer
#include <sqlite3.h>
#include "zstd_vfs.h"

extern "C" int bono_zstd_vfs_register(const char **error) {
    // Built against these headers: an older library could lack what they declare
    if (sqlite3_libversion_number() < SQLITE_VERSION_NUMBER) {
        *error = "SQLite is older than the sqlite3.h the zstd VFS was built with";
        return SQLITE_ERROR;
    }
    if (ZSTD_versionNumber() < ZSTD_VERSION_NUMBER) {
        *error = "zstd is older than the zstd.h the zstd VFS was built with";
        return SQLITE_ERROR;
    }
    // SQLite keeps using the VFS until the program ends, so it's never freed
    int rc = (new ZstdVFS())->Register("zstd");
    if (rc != SQLITE_OK) {
        *error = "couldn't register the zstd VFS";
    }
    return rc;
}
