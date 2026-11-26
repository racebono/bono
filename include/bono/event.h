#pragma once

#include "bono.h"

// The event's sessions and rules, as the simulator gives them: every session of
// the event, past, current and upcoming, and the settings it runs under. Stored
// in the `event_session` and `event_rule` tables, a whole set of rows of both at
// a time, only when something changes.
//
// Each list entry is X(type, name), where type is f64, i64, boolean or string
// (the bono_opt_* types in bono.h).

// clang-format off
#define SIM_EVENT_SESSION_COLUMNS(X)                                                                     \
    X(f64, session_time)                    /* when the event was read, on the clock of the last scoring update (not compared) */ \
    X(i64, event_order)                     /* the session's position in the event, from 0 */            \
    X(string, session_type)                 /* test, practice, qualifying, warmup, race: as in session.session_type */ \
    X(i64, session_number)                  /* from 1 within its type: as in session.session_number */   \
    X(string, name)                         /* the game's name for the session */                        \
    X(f64, length_time)                     /* seconds; NULL if it has none, or it isn't known */        \
    X(i64, length_laps)                     /* laps; NULL if unlimited, or it isn't known */             \
    X(string, finish)                       /* time, laps, time_then_laps (the time, then more laps), first_of_time_and_laps (whichever comes first), unknown */ \
    X(boolean, skipped)                     /* the session is skipped */                                 \
    X(f64, start_time_of_day)               /* seconds after midnight; NULL means the default or random */ \
    X(string, track_rubber)                 /* the game's description of the track's rubber */          \
    X(boolean, compound_change_required)    /* the race requires a change of tyre compound */
// clang-format on

// clang-format off
#define SIM_EVENT_RULE_COLUMNS(X)                                                                        \
    X(f64, session_time)                    /* when the event was read, on the clock of the last scoring update (not compared) */ \
    X(string, name)                         /* a Bono name, shared across simulators where the meaning matches */ \
    X(string, session_type)                 /* the session type the rule is for; NULL for the event */   \
    X(string, value)                        /* readable, unit included: "×2", "12 tyres", "unlimited" */
// clang-format on

#define SIM_DECLARE_FIELD(type, name) bono_opt_##type##_s name;

// One session of the event.
typedef struct bono_event_session {
  SIM_EVENT_SESSION_COLUMNS(SIM_DECLARE_FIELD)
} bono_event_session_s;

// One rule of the event, or of one type of its sessions.
typedef struct bono_event_rule {
  SIM_EVENT_RULE_COLUMNS(SIM_DECLARE_FIELD)
} bono_event_rule_s;

#undef SIM_DECLARE_FIELD
