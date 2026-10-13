#pragma once

#include "lambiase.h"

// Simulator-independent telemetry. Each simulator converts its own data into
// these structs, so every simulator shares the same database schema. Fields a
// simulator does not provide stay empty and are stored as NULL.
//
// Conventions:
//   - Units are SI (meters, seconds, m/s, radians, Newtons, kPa), temperatures
//   are Celsius.
//   - Car-relative axes: longitudinal is positive forward, lateral positive
//   right, vertical positive up.
//   - Rotations: pitch is positive nose up, yaw positive turning right, roll
//   positive leaning right.
//   - Pedals and fractions range 0-1, steering ranges -1 (left) to 1 (right).
//   - World position is in the simulator's own world coordinates, with y
//   pointing up.
//
// Each list entry is X(type, name); the name is both the struct field and the
// database column, and type is f64, i64, boolean or string (the
// lambiase_opt_* types in lambiase.h).

// clang-format off
#define SIM_TELEMETRY_COLUMNS(X)                                                                         \
    /* Timing */                                                                                         \
    X(i64, vehicle_id)                      /* the simulator's slot id for the car */                    \
    X(boolean, is_player)                   /* the car the local player is driving */                    \
    X(f64, session_time)                                                                                 \
    X(f64, time_since_last_update)                                                                       \
    X(i64, lap)                                                                                          \
    X(f64, lap_start_time)                  /* session time the current lap started */                   \
    X(f64, lap_distance)                    /* metres from the start/finish line (LMU: estimated, see the plugin) */ \
    X(i64, sector)                          /* zero-based */                                             \
    X(boolean, in_pit_lane)                                                                              \
    X(boolean, lap_invalidated)                                                                          \
    X(f64, delta_best)                      /* seconds against the best lap */                           \
    X(f64, gap_to_car_ahead)                /* seconds, on track */                                      \
    X(f64, gap_to_car_behind)                                                                            \
    X(f64, gap_to_place_ahead)              /* seconds, in the standings */                              \
    X(f64, gap_to_place_behind)                                                                          \
    /* Position and motion */                                                                            \
    X(f64, position_x)                                                                                   \
    X(f64, position_y)                                                                                   \
    X(f64, position_z)                                                                                   \
    X(f64, heading)                                                                                      \
    X(f64, pitch)                                                                                        \
    X(f64, roll)                                                                                         \
    X(f64, speed)                                                                                        \
    X(f64, velocity_longitudinal)                                                                        \
    X(f64, velocity_lateral)                                                                             \
    X(f64, velocity_vertical)                                                                            \
    X(f64, acceleration_longitudinal)                                                                    \
    X(f64, acceleration_lateral)                                                                         \
    X(f64, acceleration_vertical)                                                                        \
    X(f64, pitch_rate)                                                                                   \
    X(f64, yaw_rate)                                                                                     \
    X(f64, roll_rate)                                                                                    \
    X(f64, pitch_acceleration)                                                                           \
    X(f64, yaw_acceleration)                                                                             \
    X(f64, roll_acceleration)                                                                            \
    /* Driver inputs, as applied by the driver and after assists */                                      \
    X(f64, throttle)                                                                                     \
    X(f64, brake)                                                                                        \
    X(f64, steering)                                                                                     \
    X(f64, clutch)                                                                                       \
    X(f64, throttle_filtered)                                                                            \
    X(f64, brake_filtered)                                                                               \
    X(f64, steering_filtered)                                                                            \
    X(f64, clutch_filtered)                                                                              \
    X(f64, steering_shaft_torque)           /* Nm */                                                     \
    /* Powertrain */                                                                                     \
    X(i64, gear)                            /* -1 reverse, 0 neutral */                                  \
    X(f64, engine_rpm)                                                                                   \
    X(f64, engine_torque)                   /* Nm */                                                     \
    X(f64, clutch_rpm)                                                                                   \
    X(f64, engine_water_temperature)                                                                     \
    X(f64, engine_oil_temperature)                                                                       \
    X(boolean, engine_overheating)                                                                       \
    X(f64, turbo_boost_pressure)                                                                         \
    X(boolean, anti_stall_active)                                                                        \
    /* Fuel, energy and hybrid */                                                                        \
    X(f64, fuel)                            /* liters */                                                 \
    X(f64, virtual_energy)                                                                               \
    X(f64, battery_charge)                  /* fraction */                                               \
    X(f64, state_of_charge)                                                                              \
    X(f64, regeneration_power)              /* kW */                                                     \
    X(f64, electric_motor_torque)           /* Nm, negative while regenerating */                        \
    X(f64, electric_motor_rpm)                                                                           \
    X(f64, electric_motor_temperature)                                                                   \
    X(f64, electric_motor_water_temperature)                                                             \
    X(i64, electric_motor_state)            /* 0 unavailable, 1 inactive, 2 propulsion, 3 regeneration */ \
    X(i64, lift_and_coast_progress)                                                                      \
    /* Driver aids and car settings */                                                                   \
    X(boolean, traction_control_active)                                                                  \
    X(boolean, abs_active)                                                                               \
    X(boolean, speed_limiter_active)                                                                     \
    /* Chassis and aerodynamics */                                                                       \
    X(f64, front_third_spring_deflection)                                                                \
    X(f64, rear_third_spring_deflection)                                                                 \
    X(f64, front_wing_height)                                                                            \
    X(f64, front_ride_height)                                                                            \
    X(f64, rear_ride_height)                                                                             \
    X(f64, drag)                            /* N */                                                      \
    X(f64, front_downforce)                 /* N */                                                      \
    X(f64, rear_downforce)                  /* N */                                                      \
    X(boolean, front_flap_active)                                                                        \
    X(boolean, rear_flap_active)

// Columns that change only now and then (driver aids and car settings, damage, pit stops): written to the
// car_state table only when one of them changes for a car, instead of with every telemetry update. A car's
// value at a telemetry row is its latest car_state row at or before that row.
#define SIM_CAR_STATE_COLUMNS(X)                                                                         \
    /* Timing */                                                                                         \
    X(i64, track_limits_steps)                                                                           \
    /* Powertrain */                                                                                     \
    X(i64, ignition)                        /* 0 off, 1 ignition, 2 ignition and starter */              \
    /* Driver aids and car settings */                                                                   \
    X(i64, traction_control)                                                                             \
    X(i64, traction_control_slip)                                                                        \
    X(i64, traction_control_cut)                                                                         \
    X(i64, abs)                                                                                          \
    X(i64, engine_map)                                                                                   \
    X(i64, brake_migration)                                                                              \
    X(f64, rear_brake_bias)                 /* fraction of braking on the rear axle */                   \
    X(i64, front_anti_roll_bar)                                                                          \
    X(i64, rear_anti_roll_bar)                                                                           \
    X(boolean, speed_limiter_on)                                                                         \
    X(boolean, headlights)                                                                               \
    X(i64, wipers)                                                                                       \
    X(i64, front_tire_compound)             /* index of the compound for this car */                     \
    X(i64, rear_tire_compound)                                                                           \
    X(i64, scheduled_pit_stops)                                                                          \
    /* Fuel */                                                                                           \
    X(f64, fuel_capacity)                   /* liters */                                                 \
    X(f64, fuel_per_lap)                    /* liters, the simulator's own estimate */                   \
    X(f64, virtual_energy_per_lap)          /* fraction of a full tank, the simulator's estimate */      \
    /* Chassis and aerodynamics */                                                                       \
    X(i64, rear_flap_status)                /* 0 not allowed, 1 detected but not allowed yet, 2 allowed */ \
    /* Damage */                                                                                         \
    X(boolean, parts_detached)                                                                           \
    X(i64, dent_severity_0)                 /* 8 locations around the car: 0 none, 1 some, 2 more */     \
    X(i64, dent_severity_1)                                                                              \
    X(i64, dent_severity_2)                                                                              \
    X(i64, dent_severity_3)                                                                              \
    X(i64, dent_severity_4)                                                                              \
    X(i64, dent_severity_5)                                                                              \
    X(i64, dent_severity_6)                                                                              \
    X(i64, dent_severity_7)                                                                              \
    X(f64, last_impact_time)                /* session time */                                           \
    X(f64, last_impact_magnitude)                                                                        \
    X(f64, last_impact_position_x)                                                                       \
    X(f64, last_impact_position_y)                                                                       \
    X(f64, last_impact_position_z)                                                                       \
    X(f64, engine_damage)                   /* 0 none - 1 destroyed */                                   \
    X(f64, transmission_damage)                                                                          \
    X(f64, aerodynamics_damage)                                                                          \
    X(f64, suspension_damage)   

#define SIM_WHEEL_TELEMETRY_COLUMNS(X)                                                                   \
    /* Suspension and brakes */                                                                          \
    X(f64, suspension_deflection)                                                                        \
    X(f64, suspension_force)                /* pushrod load, N */                                       \
    X(f64, ride_height)                                                                                  \
    X(f64, wheel_vertical_position)         /* relative to the car */                                   \
    X(f64, camber)                                                                                       \
    X(f64, toe)                                                                                          \
    X(f64, brake_temperature)                                                                            \
    X(f64, brake_pressure)                                                                               \
    /* Wheel and contact patch */                                                                        \
    X(f64, rotation_speed)                  /* rad/s */                                                 \
    X(f64, lateral_patch_velocity)          /* sign as reported by the simulator */                     \
    X(f64, longitudinal_patch_velocity)                                                                  \
    X(f64, lateral_ground_velocity)                                                                      \
    X(f64, longitudinal_ground_velocity)                                                                 \
    X(f64, lateral_force)                                                                                \
    X(f64, longitudinal_force)                                                                           \
    X(f64, tire_load)                                                                                    \
    X(f64, sliding_fraction)                /* fraction of the contact patch that is sliding */         \
    X(i64, surface_type)                    /* 0 dry, 1 wet, 2 grass, 3 dirt, 4 gravel, 5 rumble strip, 6 special */ \
    /* Tire */                                                                                           \
    X(f64, tire_pressure)                                                                                \
    X(f64, tire_surface_temperature_left)                                                                \
    X(f64, tire_surface_temperature_center)                                                              \
    X(f64, tire_surface_temperature_right)                                                               \
    X(f64, tire_inner_temperature_left)                                                                  \
    X(f64, tire_inner_temperature_center)                                                                \
    X(f64, tire_inner_temperature_right)                                                                 \
    X(f64, tire_carcass_temperature)                                                                     \
    X(f64, tire_optimal_temperature)        /* unit as reported by the simulator */                     \
    X(f64, tire_wear)                       /* fraction worn */                                         \
    X(f64, tire_vertical_deflection)                                                                     \
    X(i64, tire_compound)                                                                                \
    X(i64, tire_compound_type)                                                                           \
    X(boolean, tire_flat)                                                                                \
    X(boolean, wheel_detached)
// clang-format on

#define SIM_DECLARE_FIELD(type, name) lambiase_opt_##type##_s name;

typedef struct lambiase_wheel_telemetry {
  SIM_WHEEL_TELEMETRY_COLUMNS(SIM_DECLARE_FIELD)
} lambiase_wheel_telemetry_s;

// The columns of one car that change only now and then.
typedef struct lambiase_car_state {
  SIM_CAR_STATE_COLUMNS(SIM_DECLARE_FIELD)
} lambiase_car_state_s;

// One update of one car.
typedef struct lambiase_telemetry {
  SIM_TELEMETRY_COLUMNS(SIM_DECLARE_FIELD)

  // Written to car_state only when it changes.
  lambiase_car_state_s state;

  // Front left, front right, rear left, rear right.
  lambiase_wheel_telemetry_s wheels[4];
} lambiase_telemetry_s;

#undef SIM_DECLARE_FIELD
