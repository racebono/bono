#pragma once

#include <map>
#include <optional>
#include <span>
#include <sqlite/sqlite.hpp>
#include <string>

// Simulator-independent scoring: the state of the session and the standings of every car. Like telemetry,
// each simulator converts its own data into these structs, fields it does not provide stay empty (NULL),
// and units follow the conventions in telemetry.hpp. Times are seconds; a time that is not set yet (no lap
// completed, no sector time) is NULL.
//
// Scoring is only written when it changes: each list entry is X(type, name, compared), and a new row is
// written when any `compared` column differs from the last row written. The other columns change on every
// update (clocks, gaps, distances), so they are stored with each row but never trigger one.

// clang-format off
#define SIM_SESSION_COLUMNS(X)                                                                            \
    X(double, session_time, false)                                                                        \
    X(double, time_remaining, false)                                                                      \
    X(double, time_of_day, false)            /* as reported by the simulator */                          \
    X(std::string, track, true)                                                                           \
    X(double, track_length, true)            /* meters */                                                \
    X(std::string, session_type, true)       /* test, practice, qualifying, warmup, race */              \
    X(long long, session_number, true)       /* 1-based within its type, e.g. the second practice */     \
    X(std::string, phase, true)              /* before_start, reconnaissance, grid_walk, formation_lap, countdown, green, full_course_yellow, stopped, over, paused */ \
    X(double, session_end_time, true)        /* session time the session ends */                          \
    X(long long, max_laps, true)                                                                          \
    X(double, event_start_time_of_day, true) /* seconds since midnight */                                 \
    X(long long, vehicle_count, true)                                                                     \
    X(long long, max_vehicles, true)                                                                      \
    X(std::string, full_course_yellow, true) /* none, pending, pits_closed, pit_lead_lap, pits_open, last_lap, resume, race_halt */ \
    X(bool, local_yellow_1, true)            /* per track sector, in the order the simulator reports */   \
    X(bool, local_yellow_2, true)                                                                         \
    X(bool, local_yellow_3, true)                                                                         \
    X(long long, start_light, true)                                                                       \
    X(long long, red_lights, true)                                                                        \
    X(bool, in_realtime, true)               /* the player is driving rather than in menus */             \
    X(std::string, player_name, true)                                                                     \
    X(std::string, server_name, true)                                                                     \
    X(std::string, online_role, true)        /* server, client, server_and_client */                      \
    X(bool, fixed_setup, true)                                                                            \
    X(double, rain, true)                    /* 0-1 */                                                    \
    X(double, cloud_darkness, true)          /* 0-1 */                                                    \
    X(long long, cloud_coverage, true)       /* as reported by the simulator */                           \
    X(double, ambient_temperature, true)                                                                  \
    X(double, track_temperature, true)                                                                    \
    X(double, wind_x, true)                  /* m/s, world coordinates */                                 \
    X(double, wind_y, true)                                                                               \
    X(double, wind_z, true)                                                                               \
    X(double, wind_max_speed, true)          /* m/s, the weather system's maximum (ground) wind speed */  \
    X(double, cloudiness, true)              /* 0 clear - 1 dark, the weather system's general cloudiness */ \
    X(double, track_wetness_min, true)       /* 0-1, on the racing line */                                \
    X(double, track_wetness_max, true)                                                                    \
    X(double, track_wetness_average, true)                                                                \
    X(long long, track_grip_level, true)     /* as reported by the simulator */                           \
    X(long long, track_limits_steps_per_penalty, true)                                                    \
    X(long long, track_limits_steps_per_point, true)                                                      \
    X(std::string, pit_window_status, true)  /* the player's: disabled, closed, open, stopped, completed */   \
    X(long long, pit_window_start_lap, true) /* the window, in laps-based sessions */                       \
    X(long long, pit_window_end_lap, true)                                                                \
    X(double, pit_window_start_time, true)   /* the window, in session time, in timed sessions */          \
    X(double, pit_window_end_time, true)

#define SIM_STANDING_COLUMNS(X)                                                                           \
    X(double, session_time, false)                                                                        \
    X(long long, vehicle_id, true)           /* the same id as telemetry.vehicle_id */                    \
    X(bool, is_player, true)                                                                              \
    X(std::string, driver, true)                                                                          \
    X(std::string, vehicle, true)                                                                         \
    X(std::string, vehicle_class, true)                                                                   \
    X(std::string, team, true)                                                                            \
    X(long long, position, true)             /* overall, 1-based */                                       \
    X(long long, qualifying_position, true)                                                               \
    X(long long, laps_completed, true)                                                                    \
    X(long long, sector, true)               /* zero-based, as in telemetry */                            \
    X(std::string, finish_status, true)      /* none, finished, dnf, dnq, dns, disqualified */            \
    X(double, lap_start_time, true)          /* session time the current lap started */                  \
    X(double, last_lap_time, true)                                                                        \
    X(double, last_sector_1, true)           /* sector times of the last lap */                           \
    X(double, last_sector_2, true)                                                                        \
    X(double, last_sector_3, true)                                                                        \
    X(double, best_lap_time, true)                                                                        \
    X(double, best_lap_sector_1, true)       /* sector 1 of the best lap */                               \
    X(double, best_sector_1, true)           /* best sector 1 of any lap */                               \
    X(double, best_sectors_1_and_2, true)    /* best combined sectors 1 and 2 of any lap */               \
    X(double, current_sector_1, true)        /* sector times of the lap in progress */                    \
    X(double, current_sector_2, true)                                                                     \
    X(long long, laps_behind_leader, true)                                                                \
    X(long long, laps_behind_next, true)                                                                  \
    X(long long, pit_stops, true)                                                                         \
    X(std::string, mandatory_pit_stop, true) /* none, unserved_two_tires, unserved_four_tires, served */          \
    X(long long, penalties, true)            /* outstanding */                                            \
    X(bool, in_pits, true)                                                                                \
    X(std::string, pit_state, true)          /* none, requested, entering, stopped, exiting */            \
    X(bool, in_garage, true)                                                                              \
    X(double, pit_lap_distance, true)        /* meters around the lap where the car's pit box is */       \
    X(std::string, control, true)            /* player, ai, remote, replay */                             \
    X(std::string, phase, true)              /* the car's own phase: the session phases plus after_formation, under_yellow, under_blue */ \
    X(std::string, flag, true)               /* the flag shown to the car: green, blue, black, white, checkered */ \
    X(bool, under_yellow, true)                                                                           \
    X(bool, lap_counts, true)                                                                             \
    X(bool, lap_time_counts, true)                                                                        \
    X(bool, scored, true)                                                                                 \
    X(bool, headlights, true)                                                                             \
    X(bool, drs_active, true)                                                                             \
    X(long long, attack_mode, true)          /* as reported by the simulator */                           \
    X(double, time_behind_leader, false)                                                                  \
    X(double, time_behind_next, false)                                                                    \
    X(double, lap_distance, false)           /* meters around the lap */                                  \
    X(double, time_into_lap, false)          /* estimated */                                              \
    X(double, estimated_lap_time, false)                                                                  \
    X(double, fuel_fraction, false)          /* fuel or battery left, 0-1 */                              \
    X(double, lateral_position, false)       /* meters from the approximate track center line */          \
    X(double, track_edge, false)             /* meters from the center line to the edge on the car's side */
// clang-format on

namespace sim {
#define SIM_DECLARE_SCORING_FIELD(type, name, compared) std::optional<type> name;

    // The state of the session, in the `session` table.
    struct SessionInfo {
        SIM_SESSION_COLUMNS(SIM_DECLARE_SCORING_FIELD)
    };

    // The standing of one car, in the `standings` table.
    struct Standing {
        SIM_STANDING_COLUMNS(SIM_DECLARE_SCORING_FIELD)
    };

#undef SIM_DECLARE_SCORING_FIELD

    // Writes scoring updates, adding rows only for what changed since the last write. Creates its tables
    // if needed.
    class ScoringWriter {
    public:
        explicit ScoringWriter(sqlite::Connection& conn);

        void write(const SessionInfo& session, std::span<const Standing> standings);

        // Forgets what was written, after clear_tables(): the next write adds every row again.
        void reset();

    private:
        sqlite::Statement _insert_session;
        sqlite::Statement _insert_standing;
        std::optional<SessionInfo> _last_session;
        // The last standing written for each vehicle_id.
        std::map<long long, Standing> _last_standings;
    };
}  // namespace sim
