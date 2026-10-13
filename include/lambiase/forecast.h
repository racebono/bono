#pragma once

#include "lambiase.h"

// The weather forecast for the event's sessions, as the simulator gives it (LMU
// only): a few points through each session. Stored in the `forecast` table, a
// whole set of rows at a time, only when the forecast changes.
//
// Each list entry is X(type, name), where type is f64, i64, boolean or string
// (the lambiase_opt_* types in lambiase.h).

// clang-format off
#define SIM_FORECAST_COLUMNS(X)                                                                          \
    X(f64, session_time)                    /* when the forecast was read (not compared) */             \
    X(string, session_type)                 /* practice, qualifying, warmup, race: as in session.session_type */ \
    X(f64, session_fraction)                /* how far through that session: 0 start, 0.25, 0.5, 0.75, 1 finish */ \
    X(i64, sky)                             /* the game's sky number: 0 clear, 1 light clouds, 2 partly cloudy, 3 mostly cloudy, 4 overcast, 5 overcast with drizzle, 6 overcast with light rain; higher is wetter */ \
    X(string, sky_text)                     /* the game's description of the sky, in the game's language */ \
    X(f64, rain_chance)                     /* 0-1 */                                                    \
    X(f64, temperature)                     /* air, Celsius */                                           \
    X(f64, humidity)                        /* 0-1 */                                                    \
    X(f64, wind_speed)                      /* m/s */                                                    \
    X(string, wind_direction)               /* N, NE, E, SE, S, SW, W or NW, as the game gives it */
// clang-format on

#define SIM_DECLARE_FIELD(type, name) lambiase_opt_##type##_s name;

// One point of the forecast.
typedef struct lambiase_forecast_point {
  SIM_FORECAST_COLUMNS(SIM_DECLARE_FIELD)
} lambiase_forecast_point_s;

#undef SIM_DECLARE_FIELD
