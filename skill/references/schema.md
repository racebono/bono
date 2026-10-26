# bono telemetry database - schema and gotchas

Source of truth: bono's `include/bono/telemetry.h`, `scoring.h` and `forecast.h` (column
lists) and `lmu/lmu.cpp` (the Le Mans Ultimate plugin: how the game's values are converted). Every
simulator writes the same schema; fields a simulator does not provide are NULL.

## Contents
1. Tables and how rows are written
2. Gotchas (read these before writing SQL)
3. Column guide (`telemetry`)
4. Column guide (`telemetry_wheel`)
5. Column guide (`session`)
6. Column guide (`standings`)
7. Query patterns

## 1. Tables and how rows are written

Where the file is: the LMU plugin creates `UserData\bono\telemetry.db` in the game's folder
when the first session loads, and deletes it when the game closes (a database left by a crash, or
still open in a reader when the game closed, is deleted the next time the pilot drives). The app
only uses it while Steam reports the game running. It is in WAL mode.

- `telemetry` - one row per car per recorded telemetry update (every car on track, not just the
  player). The player's car is recorded at every update; **opponents only at every Nth update** (N is
  the app's "opponent frequency" setting, 5 by default), so their rows are fewer and up to a few
  hundredths of a second older than the player's row of the same moment. Every recorded row is complete.
  `id INTEGER PRIMARY KEY` grows with time, so **`id` order is time order** and is the only indexed
  column. Updates are batched into transactions; rows of the same moment are close together in
  `id` but not guaranteed contiguous, so find "every car now" by looking a few thousand ids around
  the player's newest row and taking each car's row closest in `session_time`.
- `telemetry_wheel` - four rows per `telemetry` row: `telemetry_id` -> `telemetry.id`,
  `wheel` 0 = front left, 1 = front right, 2 = rear left, 3 = rear right. Primary key
  `(telemetry_id, wheel)`, so joining on a specific telemetry id is fast.
- `car_state` - the columns that change only now and then (driver aids and settings, compounds, pit
  stops, track-limit steps, damage, last impact; see section 3). A row (`telemetry_id`, `vehicle_id`,
  `session_time` + those columns) is added **only when one of them changes for a car**, and at the start
  of each session. A car's value at telemetry row X is its latest `car_state` row with
  `telemetry_id <= X` (indexed on `(vehicle_id, telemetry_id)`):
  `SELECT * FROM car_state WHERE vehicle_id = ? AND telemetry_id <= ? ORDER BY telemetry_id DESC LIMIT 1`.

- `session` - the state of the session (track, session type, phase, clock, flags, weather, track
  limits rules). Written about five times a second, **but a row is only added when a "compared"
  column changes** (see section 5), so the newest row can be minutes old.
- `standings` - one car's standing (official position, driver, class, laps, lap and sector times,
  pits, penalties, flag...). Same rule: a row is added for a car **only when one of its compared
  columns changes** (position, laps, sector, pit state, flag, ...), so each car has its own trickle
  of rows. The newest row per car is `MAX(id)` per `vehicle_id`.
- `forecast` - LMU only: the game's weather forecast for every session of the event (practice,
  qualifying, warmup, race), read from LMU's local web API every 30 s and written as a whole set of
  rows (same `generation`, the set's first row id) only when it changes. Columns: `generation`,
  `session_time` (session clock when read, which can reset or stop),
  `session_type` (as in `session.session_type`), `session_fraction` (0 start, 0.25, 0.5, 0.75, 1 finish
  of that session), `sky` (0 clear, 1 light clouds, 2 partly cloudy, 3 mostly cloudy, 4 overcast, 5
  overcast with drizzle, 6 overcast with light rain; higher is wetter), `sky_text` (the game's own
  words, in the game's language), `rain_chance` (0-1), `temperature` (air, C), `humidity` (0-1),
  `wind_speed` (m/s), `wind_direction` (N, NE, ... NW). Select the newest forecast with
  `WHERE generation = (SELECT generation FROM forecast ORDER BY id DESC LIMIT 1)`.
  A point's time is `session_fraction * session.session_end_time`; tell the
  pilot when as time from now (that time minus the session's current `session_time`), never as the
  fraction or the session clock.

Next to the database the bono app may also leave `track.json`: the circuit's description from
Lovely Sim Racing's lovely-track-data (CC BY-NC-SA 4.0), saved at the start of each session when the data
has the track. `source_track` is the session's `track` it was saved for (ignore the file if it differs).
Positions (`turn[].start/marker/end`, `straight[].start/end`, `sector[].marker`, `pitentry`, `pitexit`) are
fractions of the lap from the start/finish line; multiply by `session.track_length` for metres and compare
with telemetry `lap_distance`. `turn[].direction` is 0 left, 1 right; `turn[].scale` is 1 (hairpin) to 6
(wide). `time[0][class]` lists estimated seconds lost to a `pit` stop, a `dt` (drive-through) and a `sng10`
(10 s stop-and-go). The `track` command prints all of it.

In the first seconds of a session the scoring tables can still be empty; the tool then estimates
positions from telemetry.

## 2. Gotchas

- **Player car**: `is_player = 1` in both `telemetry` and `standings` (the plugin takes it from
  scoring, so telemetry rows written before the first scoring update have 0 for every car).
- **Scoring snapshots go stale**: in `session` and `standings`, the non-compared columns (marked
  "snapshot" below: `session_time`, `time_remaining`, `time_of_day`, `time_behind_*`, `lap_distance`,
  `time_into_lap`, `estimated_lap_time`, `fuel_fraction`, `lateral_position`, `track_edge`) were true
  at the moment the row was written, not now. For time left use `session_end_time - <telemetry
  session_time now>`; for live gaps and distances use telemetry.
- **Sessions in the scoring tables**: there is no session id. A new session shows up as
  `session_time` dropping back towards 0 (and, in `session`, a new `session_type` /
  `session_number`). Walk back from the newest row and stop where `session_time` jumps up.
- **Sector numbering**: `standings.sector` is zero-based like telemetry (the plugin converts LMU's
  1, 2, 0). `session.local_yellow_1..3` are in the order LMU reports its sector flags, which has not
  been checked against the track sectors - say "a yellow on track", not which sector, unless verified.
- **Official lap times**: `standings.last_lap_time`, `best_lap_time` and the sector columns come
  straight from the game. `best_lap_time` only counts laps whose time counts; the telemetry derived
  lap table can differ by a few milliseconds.
- **Several sessions in one file**: `session_time` restarts near 0 when a new session starts
  (practice -> qualifying -> race). Always scope to the current session: `id >= <first id>`
  (the tool's `sql` command prints it).
- **Unindexed scans are slow**: anything not filtered by `id` scans the whole table, which grows by
  (cars x update rate) rows per second. Filter by `id` range first; `ORDER BY id DESC LIMIT 1` for
  "latest" is instant.
- **Lap times are not stored in telemetry** (the game's own are in `standings`). From telemetry,
  lap N's time = `lap_start_time` of lap N+1 minus `lap_start_time` of lap N for the same vehicle. The current lap's elapsed time is `session_time - lap_start_time`.
- **`lap_invalidated`** is the current lap's state; a lap was invalid if it was 1 at any point
  during it (`MAX(lap_invalidated)` per lap).
- **`in_pit_lane`** covers the pit lane, which normally straddles the start/finish line, so both
  the in-lap and the out-lap show it. A refuel shows as fuel/virtual energy *increasing*.
- **`tire_wear`**: the schema comment says "fraction worn", but LMU reports fraction *remaining*
  (1.0 = new, falls during a stint). Decide from the data: if it decreases over a stint it is
  remaining. The tool does this automatically.
- **Tyre temperatures** (`*_left/center/right`) are the left, centre and right edge of the tread as
  seen from behind the car, in Celsius. Inside edge = right edge on the left-hand tyres and left
  edge on the right-hand tyres. With negative camber the inside should run a few degrees hotter;
  inside much hotter = too much camber/pressure too low; centre hottest = pressure high.
- **`tire_inner_temperature_*`** is the temperature of the inner rubber *layer* under the tread
  (left/centre/right), not the inside edge. It lags the surface and is a better read of the tyre's
  real working temperature.
- **`tire_optimal_temperature`** is passed through unconverted; values above ~200 are Kelvin
  (subtract 273.15).
- **`virtual_energy`**: LMU's per-stint energy allowance. Values in 0-1 are a fraction (show as %).
- **`rear_brake_bias`** is the fraction on the *rear* axle; drivers talk front bias:
  front % = (1 - rear_brake_bias) x 100.
- **Gaps** are seconds. `gap_to_car_ahead/behind` = physically nearest car on track (any class,
  lapped cars included). `gap_to_place_ahead/behind` = the cars directly ahead/behind in the
  standings.
- **Position**: use `standings.position` (newest row per car). Only databases without the scoring
  tables need the estimate from telemetry: more `lap` first, then earlier `lap_start_time` first.
- **World position**: `position_x/y/z` in the simulator's world, y up. To get another car relative
  to the player with `h = heading`: forward = (-sin h, -cos h), right = (-cos h, sin h) in (x, z).
  `along = dx*fx + dz*fz` (+ ahead), `across = dx*rx + dz*rz` (+ right). A car is "alongside" when
  |along| < ~5.5 m and |across| between ~1 and ~6 m.
- Booleans are stored as 0/1 integers.

## 3. Column guide - `telemetry`

Units are SI (m, s, m/s, rad, N, kPa) and Celsius unless noted. Car axes: longitudinal + forward,
lateral + right, vertical + up. Pitch + nose up, yaw + turning right, roll + leaning right.

| Group | Columns | Notes |
|---|---|---|
| Identity | `id`, `vehicle_id`, `is_player` | `vehicle_id` is the sim's slot id (can be reused online) |
| Timing | `session_time`, `time_since_last_update`, `lap`, `lap_start_time`, `lap_distance` (metres from the start/finish line; in LMU estimated from the last scoring update plus speed x time, so it can be a few metres out and jumps back to the scoring value about five times a second; empty before a car's first scoring update), `sector` (0-based), `in_pit_lane`, `lap_invalidated`, `delta_best` (s vs best lap, + = slower), `gap_to_car_ahead`, `gap_to_car_behind`, `gap_to_place_ahead`, `gap_to_place_behind`; in `car_state`: `track_limits_steps` (accumulated track-limit points x steps-per-point) | |
| Motion | `position_x/y/z`, `heading`, `pitch`, `roll`, `speed` (m/s; x3.6 = km/h), `velocity_longitudinal/lateral/vertical`, `acceleration_longitudinal/lateral/vertical` (m/s^2; /9.81 = g), `pitch_rate`, `yaw_rate`, `roll_rate`, `*_acceleration` | |
| Inputs | `throttle`, `brake`, `clutch` (0-1), `steering` (-1 left .. 1 right), `*_filtered` (after aids), `steering_shaft_torque` (Nm) | |
| Powertrain | `gear` (-1 R, 0 N), `engine_rpm`, `engine_torque`, `clutch_rpm`, `engine_water_temperature`, `engine_oil_temperature`, `engine_overheating`, `turbo_boost_pressure`, `anti_stall_active`; in `car_state`: `ignition` | |
| Energy | `fuel` (L), `virtual_energy`, `battery_charge` (0-1), `state_of_charge`, `regeneration_power` (kW), `electric_motor_torque/rpm/temperature/water_temperature`, `electric_motor_state` (0 n/a, 1 inactive, 2 propulsion, 3 regen), `lift_and_coast_progress` | |
| Aids / setup | `traction_control_active`, `abs_active`, `speed_limiter_active`; in `car_state`: `traction_control`, `traction_control_slip`, `traction_control_cut`, `abs`, `engine_map`, `brake_migration`, `rear_brake_bias`, `front_anti_roll_bar`, `rear_anti_roll_bar`, `speed_limiter_on`, `headlights`, `wipers`, `front_tire_compound`, `rear_tire_compound`, `scheduled_pit_stops` | a setting change is a new `car_state` row |
| Aero / chassis | `front_third_spring_deflection`, `rear_third_spring_deflection`, `front_wing_height`, `front_ride_height`, `rear_ride_height`, `drag`, `front_downforce`, `rear_downforce` (N), `front_flap_active`, `rear_flap_active`; in `car_state`: `rear_flap_status` | |
| Fuel (in `car_state`) | `fuel_capacity` (L), `fuel_per_lap` (L, the sim's own estimate), `virtual_energy_per_lap` (fraction of a full tank, the sim's estimate) | LMU only reports the capacity |
| Damage (all in `car_state`) | `engine_damage`, `transmission_damage`, `aerodynamics_damage`, `suspension_damage` (0 none - 1 destroyed; not reported by LMU), `parts_detached`, `dent_severity_0..7` (8 locations; 0 none, 1 some, 2 more), `last_impact_time` (session time), `last_impact_magnitude`, `last_impact_position_x/y/z` | a new impact = `last_impact_time` changes |

## 4. Column guide - `telemetry_wheel`

`suspension_deflection`, `suspension_force` (N), `ride_height`, `wheel_vertical_position`, `camber`,
`toe` (rad), `brake_temperature` (C), `brake_pressure`, `rotation_speed` (rad/s),
`lateral/longitudinal_patch_velocity`, `lateral/longitudinal_ground_velocity`,
`lateral_force`, `longitudinal_force`, `tire_load` (N), `sliding_fraction` (share of the contact
patch sliding - high values = tyre overworked), `surface_type` (0 dry, 1 wet, 2 grass, 3 dirt,
4 gravel, 5 kerb, 6 special), `tire_pressure` (kPa), `tire_surface_temperature_left/center/right`,
`tire_inner_temperature_left/center/right`, `tire_carcass_temperature`, `tire_optimal_temperature`,
`tire_wear`, `tire_vertical_deflection`, `tire_compound`, `tire_compound_type`, `tire_flat`,
`wheel_detached`.

## 5. Column guide - `session`

C = compared (a change adds a row); S = snapshot (stored with each row, never triggers one).

| Columns | Notes |
|---|---|
| `session_time` S, `time_remaining` S, `time_of_day` S | clock at the moment of the row |
| `track` C, `track_length` C (m) | |
| `session_type` C, `session_number` C | test, practice, qualifying, warmup, race; number is 1-based within the type (practice 2) |
| `phase` C | before_start, reconnaissance, grid_walk, formation_lap, countdown, green, full_course_yellow, stopped (red flag), over, paused |
| `session_end_time` C | session time at which the session ends; NULL if not timed |
| `max_laps` C | NULL when the race is not limited by laps |
| `event_start_time_of_day` C, `vehicle_count` C, `max_vehicles` C | |
| `full_course_yellow` C | none, pending, pits_closed, pit_lead_lap, pits_open, last_lap, resume, race_halt |
| `local_yellow_1..3` C | a local yellow somewhere in that sector (order as LMU reports it, unverified) |
| `start_light` C, `red_lights` C | start lights |
| `in_realtime` C | the player is driving rather than in menus |
| `player_name` C, `server_name` C, `online_role` C, `fixed_setup` C | |
| `rain` C (0-1), `cloud_darkness` C (0-1), `cloud_coverage` C | current conditions; the forecast is in the `forecast` table |
| `ambient_temperature` C, `track_temperature` C (C) | |
| `wind_x/y/z` C (m/s, world axes) | |
| `wind_max_speed` C (m/s), `cloudiness` C (0 clear - 1 dark) | from the game's weather system rather than scoring; NULL if the game never shared it |
| `track_wetness_min/max/average` C (0-1, on the racing line), `track_grip_level` C | |
| `pit_window_status` C | the player's pit window: disabled, closed, open, stopped (stop in progress), completed; not reported by LMU |
| `pit_window_start_lap` C, `pit_window_end_lap` C, `pit_window_start_time` C, `pit_window_end_time` C | when the window is open: laps in lap-based races, session time (s) in timed ones |
| `track_limits_steps_per_penalty` C, `track_limits_steps_per_point` C | compare with `car_state.track_limits_steps`: points left before a penalty = (per_penalty - steps) / per_point |

## 6. Column guide - `standings`

| Columns | Notes |
|---|---|
| `session_time` S | when the row was written |
| `vehicle_id` C, `is_player` C | same id as `telemetry.vehicle_id` |
| `driver` C, `vehicle` C, `vehicle_class` C, `team` C | names as the game shows them; several classes = multiclass race |
| `position` C (overall, 1-based), `qualifying_position` C | class position = rank by `position` among cars of the same `vehicle_class` |
| `laps_completed` C, `sector` C (0-based) | |
| `mandatory_pit_stop` C | none, unserved_two_tires, unserved_four_tires, served (not reported by LMU) |
| `finish_status` C | none, finished, dnf, dnq (did not qualify), dns (did not start), disqualified |
| `lap_start_time` C | session time the current lap started |
| `last_lap_time` C, `last_sector_1/2/3` C | the last completed lap and its sectors |
| `best_lap_time` C, `best_lap_sector_1` C, `best_sector_1` C, `best_sectors_1_and_2` C | best lap, and best sector 1 / sectors 1+2 of any lap |
| `current_sector_1/2` C | sectors of the lap in progress |
| `laps_behind_leader` C, `laps_behind_next` C | |
| `pit_stops` C, `penalties` C (outstanding) | |
| `in_pits` C, `pit_state` C, `in_garage` C, `pit_lap_distance` C | pit_state: none, requested, entering, stopped, exiting |
| `control` C | player, ai, remote, replay |
| `phase` C | the car's own phase: the session phases plus after_formation, under_yellow, under_blue |
| `flag` C | green or blue (blue = being lapped, let the faster car by) |
| `under_yellow` C, `lap_counts` C, `lap_time_counts` C, `scored` C, `headlights` C, `drs_active` C, `attack_mode` C | |
| `time_behind_leader` S, `time_behind_next` S | seconds, at the moment of the row |
| `lap_distance` S, `time_into_lap` S, `estimated_lap_time` S | metres around the lap, etc. |
| `fuel_fraction` S | fuel or battery left, 0-1 (other cars too) |
| `lateral_position` S, `track_edge` S | metres from the centre line, and to the edge on the car's side |

## 7. Query patterns

Replace `:S` with the session's first id and `:V` with the vehicle.

```sql
-- newest standing of every car in the session (the tool also stops at the session boundary)
SELECT * FROM standings WHERE id IN (SELECT MAX(id) FROM standings GROUP BY vehicle_id) ORDER BY position;

-- latest session state
SELECT * FROM session ORDER BY id DESC LIMIT 1;

-- a rival's last laps as the game timed them (one row per lap change)
SELECT laps_completed, last_lap_time, last_sector_1, last_sector_2, last_sector_3
FROM standings WHERE vehicle_id = :V GROUP BY laps_completed ORDER BY laps_completed DESC LIMIT 5;

-- latest player row
SELECT * FROM telemetry WHERE is_player = 1 ORDER BY id DESC LIMIT 1;

-- lap times for one car in the session
SELECT lap, MAX(lap_start_time) AS start, MAX(lap_invalidated) AS invalid, MAX(in_pit_lane) AS pit
FROM telemetry WHERE vehicle_id = :V AND id >= :S GROUP BY lap ORDER BY lap;
-- then time(lap) = start(lap + 1) - start(lap)

-- top speed per lap
SELECT lap, MAX(speed) * 3.6 AS kmh FROM telemetry WHERE vehicle_id = :V AND id >= :S GROUP BY lap;

-- tyres at the latest player row
SELECT w.* FROM telemetry_wheel w
WHERE w.telemetry_id = (SELECT id FROM telemetry WHERE is_player = 1 ORDER BY id DESC LIMIT 1);

-- when did a setting change (e.g. brake bias) this session
SELECT c.session_time, c.rear_brake_bias, t.lap
FROM car_state c JOIN telemetry t ON t.id = c.telemetry_id
WHERE c.vehicle_id = :V AND c.telemetry_id >= :S ORDER BY c.id;
```


## RaceRoom penalty and service detail

Additional nullable `standings` columns (not supplied by LMU):

- `penalty_type`: drive_through, stop_and_go, pit_stop, time_deduction, slow_down, disqualified; `penalty_reason_code` preserves the API reason scoped to that type, and `penalty_reason` decodes known valid reasons. Unknown codes remain available for review.
- `penalty_drive_through`, `penalty_stop_and_go`, `penalty_pit_stop`, `penalty_time_deduction`, `penalty_slow_down`: per-type values, never counts. Drive-through is active at zero; stop-and-go is seconds to stay; slowdown is seconds left to give back. Negative sentinels become NULL (none pending or unavailable); NULL does not establish why a signal is absent. All simultaneous types are retained. Existing `penalties` still counts nonnegative per-type entries.
- `pit_state`: none, requested, entering, stopped, exiting, for the player only.
- `pit_menu_state`: JSON array of nullable integer states, indexed as preset, penalty, driver change, fuel, front tires, rear tires, body, front wing, rear wing, suspension, top button, bottom button. Actions: 0 unselected, 1 selected; preset/buttons: 1 selectable. These describe intent, not completed work.
- `pit_action`: integer bitmask; 0 none, 1 preparing, 2 serving penalty, 4 driver change, 8 refueling, 16 front tires, 32 rear tires, 64 body, 128 front wing, 256 rear wing, 512 suspension. Preserve simultaneous bits. Serving is not proof a penalty has cleared.
- `pit_total_duration`, `pit_elapsed_time`, `pit_min_duration_total`, `pit_min_duration_left`: seconds, including zero; unavailable/nonfinite values become NULL. Snapshots can change, and countdowns alone never authorize leaving.

All additions participate in scoring change detection so menu changes, countdowns and serving transitions are recorded even with unchanged penalty count. Engineer status/quick alerts and strategy expose the player detail; SQL exposes every car's penalty history. Existing rising-edge penalty announcements continue to depend only on the count, avoiding repeated calls for countdown changes. Live availability still needs validation for each car/session.
