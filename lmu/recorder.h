#pragma once

// The recorder's side of the plugin, implemented in Rust (src/lib.rs). The game's data goes in converted into
// the structs in include/lambiase.

#include <lambiase/scoring.h>
#include <lambiase/telemetry.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <wchar.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct lambiase_lmu_recorder lambiase_lmu_recorder;

// Starts recording into a new database in the folder `dir` (`len` UTF-16 units), replacing one left by an
// earlier run. Null if it can't.
lambiase_lmu_recorder* lambiase_lmu_open(const wchar_t* dir, size_t len);
// Writes what's queued, stops recording and deletes the database.
void lambiase_lmu_close(lambiase_lmu_recorder* recorder);
// Deletes a database left in the folder `dir` (`len` UTF-16 units).
void lambiase_lmu_remove_database(const wchar_t* dir, size_t len);

// Called for every telemetry update of every car, before converting it: returns whether to convert it and pass
// it to lambiase_lmu_push_telemetry (opponents are only kept at every Nth update). `speed` in m/s.
bool lambiase_lmu_wants_telemetry(lambiase_lmu_recorder* recorder, int64_t vehicle_id, double session_time,
                                  double speed);
// Queues a telemetry update. Fills in is_player, and lap_distance from the scoring updates.
void lambiase_lmu_push_telemetry(lambiase_lmu_recorder* recorder, const lambiase_telemetry_s* telemetry);
// Queues a scoring update, moving `session` and the `count` standings out: they're left empty (every field
// NULL).
void lambiase_lmu_push_scoring(lambiase_lmu_recorder* recorder, lambiase_session_info_s* session,
                               lambiase_standing_s* standings, size_t count);
// Deletes everything recorded so far, e.g. when a session ends.
void lambiase_lmu_clear(lambiase_lmu_recorder* recorder);
// Whether recording stopped on an error: nothing more is recorded until it's closed and opened again.
bool lambiase_lmu_failed(lambiase_lmu_recorder* recorder);

#ifdef __cplusplus
}
#endif
