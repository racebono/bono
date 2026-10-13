#pragma once

#include <optional>
#include <span>
#include <string>
#include <vector>

// The weather forecast for the event's sessions, as the simulator gives it (LMU
// only): a few points through each session. Stored in the `forecast` table, a
// whole set of rows at a time, only when the forecast changes.
namespace sim {
// clang-format off
#define SIM_FORECAST_COLUMNS(X)                                                                          \
    X(double, session_time)                 /* when the forecast was read (not compared) */             \
    X(std::string, session_type)            /* practice, qualifying, warmup, race: as in session.session_type */ \
    X(double, session_fraction)             /* how far through that session: 0 start, 0.25, 0.5, 0.75, 1 finish */ \
    X(long long, sky)                       /* the game's sky number: 0 clear, 1 light clouds, 2 partly cloudy, 3 mostly cloudy, 4 overcast, 5 overcast with drizzle, 6 overcast with light rain; higher is wetter */ \
    X(std::string, sky_text)                /* the game's description of the sky, in the game's language */ \
    X(double, rain_chance)                  /* 0-1 */                                                    \
    X(double, temperature)                  /* air, Celsius */                                           \
    X(double, humidity)                     /* 0-1 */                                                    \
    X(double, wind_speed)                   /* m/s */                                                    \
    X(std::string, wind_direction)          /* N, NE, E, SE, S, SW, W or NW, as the game gives it */
// clang-format on

#define SIM_DECLARE_FIELD(type, name) std::optional<type> name;

// One point of the forecast.
struct ForecastPoint {
  SIM_FORECAST_COLUMNS(SIM_DECLARE_FIELD)
};

#undef SIM_DECLARE_FIELD

} // namespace sim
