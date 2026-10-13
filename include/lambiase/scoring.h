#pragma once

#include "lambiase.h"

// Simulator-independent scoring: the state of the session and the standings of
// every car. Like telemetry, each simulator converts its own data into these
// structs, fields it does not provide stay empty (NULL), and units follow the
// conventions in telemetry.h. Times are seconds; a time that is not set yet
// (no lap completed, no sector time) is NULL.
//
// Scoring is only written when it changes: each list entry is X(type, name,
// compared), and a new row is written when any `compared` column differs from
// the last row written. The other columns change on every update (clocks, gaps,
// distances), so they are stored with each row but never trigger one. `type` is
// f64, i64, boolean or string (the lambiase_opt_* types in lambiase.h).

// clang-format off
#define SIM_SESSION_COLUMNS(X)                                                                            \
    X(f64, session_time, false)                                                                           \
    X(f64, time_remaining, false)                                                                         \
    X(f64, time_of_day, false)               /* as reported by the simulator */                          \
    X(string, track, true)                                                                                \
    X(f64, track_length, true)               /* meters */                                                \
    X(string, session_type, true)            /* test, practice, qualifying, warmup, race */              \
    X(i64, session_number, true)             /* 1-based within its type, e.g. the second practice */     \
    X(string, phase, true)                   /* before_start, reconnaissance, grid_walk, formation_lap, countdown, green, full_course_yellow, stopped, over, paused */ \
    X(f64, session_end_time, true)           /* session time the session ends */                          \
    X(i64, max_laps, true)                                                                                \
    X(f64, event_start_time_of_day, true)    /* seconds since midnight */                                 \
    X(i64, vehicle_count, true)                                                                           \
    X(i64, max_vehicles, true)                                                                            \
    X(string, full_course_yellow, true)      /* none, pending, pits_closed, pit_lead_lap, pits_open, last_lap, resume, race_halt */ \
    X(boolean, local_yellow_1, true)         /* per track sector, in the order the simulator reports */   \
    X(boolean, local_yellow_2, true)                                                                      \
    X(boolean, local_yellow_3, true)                                                                      \
    X(i64, start_light, true)                                                                             \
    X(i64, red_lights, true)                                                                              \
    X(boolean, in_realtime, true)            /* the player is driving rather than in menus */             \
    X(string, player_name, true)                                                                          \
    X(string, server_name, true)                                                                          \
    X(string, online_role, true)             /* server, client, server_and_client */                      \
    X(boolean, fixed_setup, true)                                                                         \
    X(f64, rain, true)                       /* 0-1 */                                                    \
    X(f64, cloud_darkness, true)             /* 0-1 */                                                    \
    X(i64, cloud_coverage, true)             /* as reported by the simulator */                           \
    X(f64, ambient_temperature, true)                                                                     \
    X(f64, track_temperature, true)                                                                       \
    X(f64, wind_x, true)                     /* m/s, world coordinates */                                 \
    X(f64, wind_y, true)                                                                                  \
    X(f64, wind_z, true)                                                                                  \
    X(f64, wind_max_speed, true)             /* m/s, the weather system's maximum (ground) wind speed */  \
    X(f64, cloudiness, true)                 /* 0 clear - 1 dark, the weather system's general cloudiness */ \
    X(f64, track_wetness_min, true)          /* 0-1, on the racing line */                                \
    X(f64, track_wetness_max, true)                                                                       \
    X(f64, track_wetness_average, true)                                                                   \
    X(i64, track_grip_level, true)           /* as reported by the simulator */                           \
    X(i64, track_limits_steps_per_penalty, true)                                                          \
    X(i64, track_limits_steps_per_point, true)                                                            \
    X(string, pit_window_status, true)       /* the player's: disabled, closed, open, stopped, completed */   \
    X(i64, pit_window_start_lap, true)       /* the window, in laps-based sessions */                       \
    X(i64, pit_window_end_lap, true)                                                                      \
    X(f64, pit_window_start_time, true)      /* the window, in session time, in timed sessions */          \
    X(f64, pit_window_end_time, true)   

#define SIM_STANDING_COLUMNS(X)                                                                           \
    X(f64, session_time, false)                                                                           \
    X(i64, vehicle_id, true)                 /* the same id as telemetry.vehicle_id */                    \
    X(boolean, is_player, true)                                                                           \
    X(string, driver, true)                                                                               \
    X(string, vehicle, true)                                                                              \
    X(string, vehicle_class, true)                                                                        \
    X(string, team, true)                                                                                 \
    X(i64, position, true)                   /* overall, 1-based */                                       \
    X(i64, qualifying_position, true)                                                                     \
    X(i64, laps_completed, true)                                                                          \
    X(i64, sector, true)                     /* zero-based, as in telemetry */                            \
    X(string, finish_status, true)           /* none, finished, dnf, dnq, dns, disqualified */            \
    X(f64, lap_start_time, true)             /* session time the current lap started */                  \
    X(f64, last_lap_time, true)                                                                           \
    X(f64, last_sector_1, true)              /* sector times of the last lap */                           \
    X(f64, last_sector_2, true)                                                                           \
    X(f64, last_sector_3, true)                                                                           \
    X(f64, best_lap_time, true)                                                                           \
    X(f64, best_lap_sector_1, true)          /* sector 1 of the best lap */                               \
    X(f64, best_sector_1, true)              /* best sector 1 of any lap */                               \
    X(f64, best_sectors_1_and_2, true)       /* best combined sectors 1 and 2 of any lap */               \
    X(f64, current_sector_1, true)           /* sector times of the lap in progress */                    \
    X(f64, current_sector_2, true)                                                                        \
    X(i64, laps_behind_leader, true)                                                                      \
    X(i64, laps_behind_next, true)                                                                        \
    X(i64, pit_stops, true)                                                                               \
    X(string, mandatory_pit_stop, true)      /* none, unserved_two_tires, unserved_four_tires, served */          \
    X(i64, penalties, true)                  /* outstanding */                                            \
    X(boolean, in_pits, true)                                                                             \
    X(string, pit_state, true)               /* none, requested, entering, stopped, exiting */            \
    X(boolean, in_garage, true)                                                                           \
    X(f64, pit_lap_distance, true)           /* meters around the lap where the car's pit box is */       \
    X(string, control, true)                 /* player, ai, remote, replay */                             \
    X(string, phase, true)                   /* the car's own phase: the session phases plus after_formation, under_yellow, under_blue */ \
    X(string, flag, true)                    /* the flag shown to the car: green, blue, black, white, checkered */ \
    X(boolean, under_yellow, true)                                                                        \
    X(boolean, lap_counts, true)                                                                          \
    X(boolean, lap_time_counts, true)                                                                     \
    X(boolean, scored, true)                                                                              \
    X(boolean, headlights, true)                                                                          \
    X(boolean, drs_active, true)                                                                          \
    X(i64, attack_mode, true)                /* as reported by the simulator */                           \
    X(f64, time_behind_leader, false)                                                                     \
    X(f64, time_behind_next, false)                                                                       \
    X(f64, lap_distance, false)              /* meters around the lap */                                  \
    X(f64, time_into_lap, false)             /* estimated */                                              \
    X(f64, estimated_lap_time, false)                                                                     \
    X(f64, fuel_fraction, false)             /* fuel or battery left, 0-1 */                              \
    X(f64, lateral_position, false)          /* meters from the approximate track center line */          \
    X(f64, track_edge, false)                /* meters from the center line to the edge on the car's side */
// clang-format on

#define SIM_DECLARE_SCORING_FIELD(type, name, compared)                          lambiase_opt_##type##_s name;

// The state of the session, in the `session` table.
typedef struct lambiase_session_info {
  SIM_SESSION_COLUMNS(SIM_DECLARE_SCORING_FIELD)
} lambiase_session_info_s;

// The standing of one car, in the `standings` table.
typedef struct lambiase_standing {
  SIM_STANDING_COLUMNS(SIM_DECLARE_SCORING_FIELD)
} lambiase_standing_s;

#undef SIM_DECLARE_SCORING_FIELD
