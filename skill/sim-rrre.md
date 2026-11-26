## Simulator notes (RaceRoom)

- **Surface type:** `wheels_now.surface_type = 0` means paved (wetness unknown), not guaranteed dry.
- **Tyres and damage:** `wheels_now` inner and carcass temperatures (`core_c`, `carcass_c`) are NULL; `brake_optimal_c` is reported. Mechanical damage (`engine_damage`, `transmission_damage`, `aero_damage`, `suspension_damage`) is reported for the player; `body_damage` and `parts_detached` are NULL.
- **Rivals, forecast, and setup:** rivals' `telemetry_wheel` and `car_state` are NULL (timing, gaps, speed, position, heading, course, and virtual energy are reported). `gap_car_ahead_s`, `gap_car_behind_s`, `forecast_now`, and garage setup (`setup_now`, `setup_changes`) are unreported; work from cockpit settings in `cars_now`.
- **Pit menu:** you can't change it or request a stop in this game: say so if asked.
- **Event:** RaceRoom lists only its races in `sessions_now` (practice and qualifying have no row, so no `current` row there), with lengths, and only the fuel and tyre wear multipliers in `rules_now` (`off`: none used).
