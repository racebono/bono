#pragma once

#include <array>
#include <map>
#include <optional>

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
// database column.

// clang-format off
#define SIM_TELEMETRY_COLUMNS(X)                                                                         \
    /* Timing */                                                                                         \
    X(long long, vehicle_id)                /* the simulator's slot id for the car */                    \
    X(bool, is_player)                      /* the car the local player is driving */                    \
    X(double, session_time)                                                                              \
    X(double, time_since_last_update)                                                                    \
    X(long long, lap)                                                                                    \
    X(double, lap_start_time)               /* session time the current lap started */                   \
    X(double, lap_distance)                 /* metres from the start/finish line (LMU: estimated, see the plugin) */ \
    X(long long, sector)                    /* zero-based */                                             \
    X(bool, in_pit_lane)                                                                                 \
    X(bool, lap_invalidated)                                                                             \
    X(double, delta_best)                   /* seconds against the best lap */                           \
    X(double, gap_to_car_ahead)             /* seconds, on track */                                      \
    X(double, gap_to_car_behind)                                                                         \
    X(double, gap_to_place_ahead)           /* seconds, in the standings */                              \
    X(double, gap_to_place_behind)                                                                       \
    /* Position and motion */                                                                            \
    X(double, position_x)                                                                                \
    X(double, position_y)                                                                                \
    X(double, position_z)                                                                                \
    X(double, heading)                                                                                   \
    X(double, pitch)                                                                                     \
    X(double, roll)                                                                                      \
    X(double, speed)                                                                                     \
    X(double, velocity_longitudinal)                                                                     \
    X(double, velocity_lateral)                                                                          \
    X(double, velocity_vertical)                                                                         \
    X(double, acceleration_longitudinal)                                                                 \
    X(double, acceleration_lateral)                                                                      \
    X(double, acceleration_vertical)                                                                     \
    X(double, pitch_rate)                                                                                \
    X(double, yaw_rate)                                                                                  \
    X(double, roll_rate)                                                                                 \
    X(double, pitch_acceleration)                                                                        \
    X(double, yaw_acceleration)                                                                          \
    X(double, roll_acceleration)                                                                         \
    /* Driver inputs, as applied by the driver and after assists */                                      \
    X(double, throttle)                                                                                  \
    X(double, brake)                                                                                     \
    X(double, steering)                                                                                  \
    X(double, clutch)                                                                                    \
    X(double, throttle_filtered)                                                                         \
    X(double, brake_filtered)                                                                            \
    X(double, steering_filtered)                                                                         \
    X(double, clutch_filtered)                                                                           \
    X(double, steering_shaft_torque)        /* Nm */                                                     \
    /* Powertrain */                                                                                     \
    X(long long, gear)                      /* -1 reverse, 0 neutral */                                  \
    X(double, engine_rpm)                                                                                \
    X(double, engine_torque)                /* Nm */                                                     \
    X(double, clutch_rpm)                                                                                \
    X(double, engine_water_temperature)                                                                  \
    X(double, engine_oil_temperature)                                                                    \
    X(bool, engine_overheating)                                                                          \
    X(double, turbo_boost_pressure)                                                                      \
    X(bool, anti_stall_active)                                                                           \
    /* Fuel, energy and hybrid */                                                                        \
    X(double, fuel)                         /* liters */                                                 \
    X(double, virtual_energy)                                                                            \
    X(double, battery_charge)               /* fraction */                                               \
    X(double, state_of_charge)                                                                           \
    X(double, regeneration_power)           /* kW */                                                     \
    X(double, electric_motor_torque)        /* Nm, negative while regenerating */                        \
    X(double, electric_motor_rpm)                                                                        \
    X(double, electric_motor_temperature)                                                                \
    X(double, electric_motor_water_temperature)                                                          \
    X(long long, electric_motor_state)      /* 0 unavailable, 1 inactive, 2 propulsion, 3 regeneration */ \
    X(long long, lift_and_coast_progress)                                                                \
    /* Driver aids and car settings */                                                                   \
    X(bool, traction_control_active)                                                                     \
    X(bool, abs_active)                                                                                  \
    X(bool, speed_limiter_active)                                                                        \
    /* Chassis and aerodynamics */                                                                       \
    X(double, front_third_spring_deflection)                                                             \
    X(double, rear_third_spring_deflection)                                                              \
    X(double, front_wing_height)                                                                         \
    X(double, front_ride_height)                                                                         \
    X(double, rear_ride_height)                                                                          \
    X(double, drag)                         /* N */                                                      \
    X(double, front_downforce)              /* N */                                                      \
    X(double, rear_downforce)               /* N */                                                      \
    X(bool, front_flap_active)                                                                           \
    X(bool, rear_flap_active)

// Columns that change only now and then (driver aids and car settings, damage, pit stops): written to the
// car_state table only when one of them changes for a car, instead of with every telemetry update. A car's
// value at a telemetry row is its latest car_state row at or before that row.
#define SIM_CAR_STATE_COLUMNS(X)                                                                         \
    /* Timing */                                                                                         \
    X(long long, track_limits_steps)                                                                     \
    /* Powertrain */                                                                                     \
    X(long long, ignition)                  /* 0 off, 1 ignition, 2 ignition and starter */              \
    /* Driver aids and car settings */                                                                   \
    X(long long, traction_control)                                                                       \
    X(long long, traction_control_slip)                                                                  \
    X(long long, traction_control_cut)                                                                   \
    X(long long, abs)                                                                                    \
    X(long long, engine_map)                                                                             \
    X(long long, brake_migration)                                                                        \
    X(double, rear_brake_bias)              /* fraction of braking on the rear axle */                   \
    X(long long, front_anti_roll_bar)                                                                    \
    X(long long, rear_anti_roll_bar)                                                                     \
    X(bool, speed_limiter_on)                                                                            \
    X(bool, headlights)                                                                                  \
    X(long long, wipers)                                                                                 \
    X(long long, front_tire_compound)       /* index of the compound for this car */                     \
    X(long long, rear_tire_compound)                                                                     \
    X(long long, scheduled_pit_stops)                                                                    \
    /* Fuel */                                                                                           \
    X(double, fuel_capacity)                /* liters */                                                 \
    X(double, fuel_per_lap)                 /* liters, the simulator's own estimate */                   \
    X(double, virtual_energy_per_lap)       /* fraction of a full tank, the simulator's estimate */      \
    /* Chassis and aerodynamics */                                                                       \
    X(long long, rear_flap_status)          /* 0 not allowed, 1 detected but not allowed yet, 2 allowed */ \
    /* Damage */                                                                                         \
    X(bool, parts_detached)                                                                              \
    X(long long, dent_severity_0)           /* 8 locations around the car: 0 none, 1 some, 2 more */     \
    X(long long, dent_severity_1)                                                                        \
    X(long long, dent_severity_2)                                                                        \
    X(long long, dent_severity_3)                                                                        \
    X(long long, dent_severity_4)                                                                        \
    X(long long, dent_severity_5)                                                                        \
    X(long long, dent_severity_6)                                                                        \
    X(long long, dent_severity_7)                                                                        \
    X(double, last_impact_time)             /* session time */                                           \
    X(double, last_impact_magnitude)                                                                     \
    X(double, last_impact_position_x)                                                                    \
    X(double, last_impact_position_y)                                                                    \
    X(double, last_impact_position_z)                                                                    \
    X(double, engine_damage)                /* 0 none - 1 destroyed */                                   \
    X(double, transmission_damage)                                                                       \
    X(double, aerodynamics_damage)                                                                       \
    X(double, suspension_damage)

#define SIM_WHEEL_TELEMETRY_COLUMNS(X)                                                                   \
    /* Suspension and brakes */                                                                          \
    X(double, suspension_deflection)                                                                     \
    X(double, suspension_force)             /* pushrod load, N */                                       \
    X(double, ride_height)                                                                               \
    X(double, wheel_vertical_position)      /* relative to the car */                                   \
    X(double, camber)                                                                                    \
    X(double, toe)                                                                                       \
    X(double, brake_temperature)                                                                         \
    X(double, brake_pressure)                                                                            \
    /* Wheel and contact patch */                                                                        \
    X(double, rotation_speed)               /* rad/s */                                                 \
    X(double, lateral_patch_velocity)       /* sign as reported by the simulator */                     \
    X(double, longitudinal_patch_velocity)                                                               \
    X(double, lateral_ground_velocity)                                                                   \
    X(double, longitudinal_ground_velocity)                                                              \
    X(double, lateral_force)                                                                             \
    X(double, longitudinal_force)                                                                        \
    X(double, tire_load)                                                                                 \
    X(double, sliding_fraction)             /* fraction of the contact patch that is sliding */         \
    X(long long, surface_type)              /* 0 dry, 1 wet, 2 grass, 3 dirt, 4 gravel, 5 rumble strip, 6 special */ \
    /* Tire */                                                                                           \
    X(double, tire_pressure)                                                                             \
    X(double, tire_surface_temperature_left)                                                             \
    X(double, tire_surface_temperature_center)                                                           \
    X(double, tire_surface_temperature_right)                                                            \
    X(double, tire_inner_temperature_left)                                                               \
    X(double, tire_inner_temperature_center)                                                             \
    X(double, tire_inner_temperature_right)                                                              \
    X(double, tire_carcass_temperature)                                                                  \
    X(double, tire_optimal_temperature)     /* unit as reported by the simulator */                     \
    X(double, tire_wear)                    /* fraction worn */                                         \
    X(double, tire_vertical_deflection)                                                                  \
    X(long long, tire_compound)                                                                          \
    X(long long, tire_compound_type)                                                                     \
    X(bool, tire_flat)                                                                                   \
    X(bool, wheel_detached)
// clang-format on

namespace sim {
#define SIM_DECLARE_FIELD(type, name) std::optional<type> name;

struct WheelTelemetry {
  SIM_WHEEL_TELEMETRY_COLUMNS(SIM_DECLARE_FIELD)
};

// The columns of one car that change only now and then.
struct CarState {
  SIM_CAR_STATE_COLUMNS(SIM_DECLARE_FIELD)

  bool operator==(const CarState &) const = default;
};

// One update of one car.
struct Telemetry {
  SIM_TELEMETRY_COLUMNS(SIM_DECLARE_FIELD)

  // Written to car_state only when it changes.
  CarState state;

  // Front left, front right, rear left, rear right.
  std::array<WheelTelemetry, 4> wheels;
};

#undef SIM_DECLARE_FIELD

// Creates the telemetry tables if they do not exist yet.
void create_tables(sqlite::Connection &conn);

// Deletes every recorded row (telemetry, car state, scoring...), keeping the
// tables, and gives the space back to the file system as far as readers allow.
// Readers keep working and see the database fill up again. The writers on this
// connection must be reset() too, as they skip rows that did not change.
void clear_tables(sqlite::Connection &conn);

// Inserts telemetry with statements prepared once. Creates the tables if
// needed.
class TelemetryWriter {
public:
  explicit TelemetryWriter(sqlite::Connection &conn);

  // Adds a telemetry row and its wheel rows, and a car_state row if the car's
  // state changed since the last one written for it (or a new session started).
  void write(const Telemetry &telemetry);

  // Forgets what was written, after clear_tables(): every car's state is
  // written again.
  void reset();

private:
  struct LastState {
    CarState state;
    double session_time = 0.0;
  };

  sqlite::Connection &_conn;
  sqlite::Statement _insert_car;
  sqlite::Statement _insert_wheel;
  sqlite::Statement _insert_state;
  // The last car_state written for each vehicle_id.
  std::map<long long, LastState> _last_states;
};
} // namespace sim
