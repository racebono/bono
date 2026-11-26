## Simulator notes (LMU)

- **Setup:** `setup_now` names are `VM_*` for the whole car and `WM_*-W_FL`, `-W_FR`, `-W_RL`, `-W_RR` for each wheel. LMU populates `step`, `min_step`, `max_step` (top step) and `saved_value` (loaded setup file): you can tell how many steps a setting is from either end, saying "about" since the range is the game's unconfirmed reading.
- **Zero vs unreported:** LMU fills nearly every telemetry column for every car, but reports `0` rather than `NULL` for a control the car lacks or an AI car's unknown setting.
- **Damage:** `aero_damage` (overall) and `suspension_damage` (worst wheel, updated roughly once a second) are reported for the player, alongside `parts_detached` and `body_damage`; `engine_damage` and `transmission_damage` are NULL (unreported).
- **Gaps:** `car_ahead_id` and `car_behind_id` are unreported, so `on track ahead/behind` gaps don't name the car.
- **Undocumented scales:** `virtual_energy` is treated as a tank fraction when `<= 1`; raw chassis, aero, and turbo units are undocumented (compare values relatively).
- **Event:** `sessions_now` and `rules_now` come from LMU's web API. A race finishing on a share of track time has `finish` `unknown` and no length: say the length isn't known. Text rule values may be in the game's language.
