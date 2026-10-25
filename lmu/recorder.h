#pragma once

// The recorder's side of the plugin, implemented in Rust (src/lib.rs). The game's data goes in converted into
// the structs in include/bono.

#include <bono/scoring.h>
#include <bono/telemetry.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <wchar.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct bono_lmu_recorder bono_lmu_recorder;

// Starts recording into a new database in the folder `dir` (`len` UTF-16 units), replacing one left by an
// earlier run. Null if it can't.
bono_lmu_recorder* bono_lmu_open(const wchar_t* dir, size_t len);
// Restarts a failed writer without deleting the recording.
void bono_lmu_restart(bono_lmu_recorder* recorder);
// Writes what's queued, stops recording and deletes the database.
void bono_lmu_close(bono_lmu_recorder* recorder);
// Deletes a database left in the folder `dir` (`len` UTF-16 units).
void bono_lmu_remove_database(const wchar_t* dir, size_t len);

// Called for every telemetry update of every car, before converting it: returns whether to convert it and pass
// it to bono_lmu_push_telemetry (opponents are only kept at every Nth update). `speed` in m/s.
bool bono_lmu_wants_telemetry(bono_lmu_recorder* recorder, int64_t vehicle_id, double session_time,
                                  double speed);
// Queues a telemetry update. Fills in is_player, and lap_distance from the scoring updates.
void bono_lmu_push_telemetry(bono_lmu_recorder* recorder, const bono_telemetry_s* telemetry);
// Queues a scoring update, moving `session` and the `count` standings out: they're left empty (every field
// NULL).
void bono_lmu_push_scoring(bono_lmu_recorder* recorder, bono_session_info_s* session,
                               bono_standing_s* standings, size_t count);
// Deletes everything recorded so far, e.g. when a session ends.
void bono_lmu_clear(bono_lmu_recorder* recorder);
// Whether recording stopped on an error: nothing more is recorded until it is restarted.
bool bono_lmu_failed(bono_lmu_recorder* recorder);

#ifdef __cplusplus
}
#endif
