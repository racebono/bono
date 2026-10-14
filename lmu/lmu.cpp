// Le Mans Ultimate plugin: the game loads this DLL from its Plugins folder and calls it with telemetry and
// scoring updates. This file implements the game's plugin interface and converts its data into the structs in
// include/lambiase; the recorder in src/lib.rs (Rust, in the same DLL) writes them into the database.
//
// Ported from my-engineer's sim/lmu/lmu.cpp.
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX

#include <Windows.h>
#include <algorithm>
#include <climits>
#include <cmath>
#include <cstddef>
#include <cstring>
#include <filesystem>
#include <iterator>
#include <mutex>
#include <new>
#include <optional>
#include <string>
#include <utility>
#include <vector>

#include "InternalsPlugin.hpp"
#include "recorder.h"

static double kelvin_to_celsius(double kelvin) {
    return kelvin - 273.15;
}

// LMU's car axes are +x left, +y up, +z back; lambiase_telemetry_s uses forward, right and up.
static void convert(const TelemInfoV01& in, lambiase_telemetry_s& out) {
    out.vehicle_id = in.mID;
    out.session_time = in.mElapsedTime;
    out.time_since_last_update = in.mDeltaTime;
    out.lap = in.mLapNumber;
    out.lap_start_time = in.mLapStartET;
    // The sign bit of mCurrentSector marks the pit lane.
    out.sector = in.mCurrentSector & 0x7FFFFFFF;
    out.in_pit_lane = in.mCurrentSector < 0;
    out.lap_invalidated = in.mLapInvalidated;
    out.delta_best = in.mDeltaBest;
    out.gap_to_car_ahead = in.mTimeGapCarAhead;
    out.gap_to_car_behind = in.mTimeGapCarBehind;
    out.gap_to_place_ahead = in.mTimeGapPlaceAhead;
    out.gap_to_place_behind = in.mTimeGapPlaceBehind;
    out.state.track_limits_steps = in.mTrackLimitsSteps;

    out.position_x = in.mPos.x;
    out.position_y = in.mPos.y;
    out.position_z = in.mPos.z;
    // mOri rows convert car vectors to world x, y and z, so column 2 is the car's backward axis and
    // column 0 its left axis, in world coordinates.
    out.heading = std::atan2(in.mOri[0].z, in.mOri[2].z);
    out.pitch = std::atan2(-in.mOri[1].z, std::hypot(in.mOri[0].z, in.mOri[2].z));
    out.roll = std::atan2(in.mOri[1].x, std::hypot(in.mOri[0].x, in.mOri[2].x));
    out.speed = std::hypot(in.mLocalVel.x, in.mLocalVel.y, in.mLocalVel.z);
    out.velocity_longitudinal = -in.mLocalVel.z;
    out.velocity_lateral = -in.mLocalVel.x;
    out.velocity_vertical = in.mLocalVel.y;
    out.acceleration_longitudinal = -in.mLocalAccel.z;
    out.acceleration_lateral = -in.mLocalAccel.x;
    out.acceleration_vertical = in.mLocalAccel.y;
    // LMU rotations already match: +x pitches up, +y yaws right, +z rolls right.
    out.pitch_rate = in.mLocalRot.x;
    out.yaw_rate = in.mLocalRot.y;
    out.roll_rate = in.mLocalRot.z;
    out.pitch_acceleration = in.mLocalRotAccel.x;
    out.yaw_acceleration = in.mLocalRotAccel.y;
    out.roll_acceleration = in.mLocalRotAccel.z;

    out.throttle = in.mUnfilteredThrottle;
    out.brake = in.mUnfilteredBrake;
    out.steering = in.mUnfilteredSteering;
    out.clutch = in.mUnfilteredClutch;
    out.throttle_filtered = in.mFilteredThrottle;
    out.brake_filtered = in.mFilteredBrake;
    out.steering_filtered = in.mFilteredSteering;
    out.clutch_filtered = in.mFilteredClutch;
    out.steering_shaft_torque = in.mSteeringShaftTorque;

    out.gear = in.mGear;
    out.engine_rpm = in.mEngineRPM;
    out.engine_torque = in.mEngineTorque;
    out.clutch_rpm = in.mClutchRPM;
    out.engine_water_temperature = in.mEngineWaterTemp;
    out.engine_oil_temperature = in.mEngineOilTemp;
    out.engine_overheating = in.mOverheating;
    out.turbo_boost_pressure = in.mTurboBoostPressure;
    out.state.ignition = in.mIgnitionStarter;
    out.anti_stall_active = in.mAntiStallActivated != 0;

    out.fuel = in.mFuel;
    out.state.fuel_capacity = in.mFuelCapacity;
    out.virtual_energy = in.mVirtualEnergy;
    out.battery_charge = in.mBatteryChargeFraction;
    out.state_of_charge = in.mSoC;
    out.regeneration_power = in.mRegen;
    out.electric_motor_torque = in.mElectricBoostMotorTorque;
    out.electric_motor_rpm = in.mElectricBoostMotorRPM;
    out.electric_motor_temperature = in.mElectricBoostMotorTemperature;
    out.electric_motor_water_temperature = in.mElectricBoostWaterTemperature;
    out.electric_motor_state = in.mElectricBoostMotorState;
    out.lift_and_coast_progress = in.mLiftAndCoastProgress;

    out.state.traction_control = in.mTC;
    out.state.traction_control_slip = in.mTCSlip;
    out.state.traction_control_cut = in.mTCCut;
    out.traction_control_active = in.mTCActive;
    out.state.abs = in.mABS;
    out.abs_active = in.mABSActive;
    out.state.engine_map = in.mMotorMap;
    out.state.brake_migration = in.mMigration;
    out.state.rear_brake_bias = in.mRearBrakeBias;
    out.state.front_anti_roll_bar = in.mFrontAntiSway;
    out.state.rear_anti_roll_bar = in.mRearAntiSway;
    out.state.speed_limiter_on = in.mSpeedLimiter != 0;
    out.speed_limiter_active = in.mSpeedLimiterActive;
    out.state.headlights = in.mHeadlights;
    out.state.wipers = in.mWiperState;
    out.state.front_tire_compound = in.mFrontTireCompoundIndex;
    out.state.rear_tire_compound = in.mRearTireCompoundIndex;
    out.state.scheduled_pit_stops = in.mScheduledStops;

    out.front_third_spring_deflection = in.mFront3rdDeflection;
    out.rear_third_spring_deflection = in.mRear3rdDeflection;
    out.front_wing_height = in.mFrontWingHeight;
    out.front_ride_height = in.mFrontRideHeight;
    out.rear_ride_height = in.mRearRideHeight;
    out.drag = in.mDrag;
    out.front_downforce = in.mFrontDownforce;
    out.rear_downforce = in.mRearDownforce;
    out.front_flap_active = in.mFrontFlapActivated != 0;
    out.rear_flap_active = in.mRearFlapActivated != 0;
    out.state.rear_flap_status = in.mRearFlapLegalStatus;

    out.state.parts_detached = in.mDetached;
    out.state.dent_severity_0 = in.mDentSeverity[0];
    out.state.dent_severity_1 = in.mDentSeverity[1];
    out.state.dent_severity_2 = in.mDentSeverity[2];
    out.state.dent_severity_3 = in.mDentSeverity[3];
    out.state.dent_severity_4 = in.mDentSeverity[4];
    out.state.dent_severity_5 = in.mDentSeverity[5];
    out.state.dent_severity_6 = in.mDentSeverity[6];
    out.state.dent_severity_7 = in.mDentSeverity[7];
    out.state.last_impact_time = in.mLastImpactET;
    out.state.last_impact_magnitude = in.mLastImpactMagnitude;
    out.state.last_impact_position_x = in.mLastImpactPos.x;
    out.state.last_impact_position_y = in.mLastImpactPos.y;
    out.state.last_impact_position_z = in.mLastImpactPos.z;

    for (std::size_t i = 0; i < std::size(out.wheels); ++i) {
        const TelemWheelV01& w = in.mWheel[i];
        lambiase_wheel_telemetry_s& o = out.wheels[i];
        o.suspension_deflection = w.mSuspensionDeflection;
        o.suspension_force = w.mSuspForce;
        o.ride_height = w.mRideHeight;
        o.wheel_vertical_position = w.mWheelYLocation;
        o.camber = w.mCamber;
        o.toe = w.mToe;
        o.brake_temperature = w.mBrakeTemp;
        o.brake_pressure = w.mBrakePressure;
        o.rotation_speed = w.mRotation;
        o.lateral_patch_velocity = w.mLateralPatchVel;
        o.longitudinal_patch_velocity = w.mLongitudinalPatchVel;
        o.lateral_ground_velocity = w.mLateralGroundVel;
        o.longitudinal_ground_velocity = w.mLongitudinalGroundVel;
        o.lateral_force = w.mLateralForce;
        o.longitudinal_force = w.mLongitudinalForce;
        o.tire_load = w.mTireLoad;
        o.sliding_fraction = w.mGripFract;
        o.surface_type = w.mSurfaceType;
        o.tire_pressure = w.mPressure;
        o.tire_surface_temperature_left = kelvin_to_celsius(w.mTemperature[0]);
        o.tire_surface_temperature_center = kelvin_to_celsius(w.mTemperature[1]);
        o.tire_surface_temperature_right = kelvin_to_celsius(w.mTemperature[2]);
        o.tire_inner_temperature_left = kelvin_to_celsius(w.mTireInnerLayerTemperature[0]);
        o.tire_inner_temperature_center = kelvin_to_celsius(w.mTireInnerLayerTemperature[1]);
        o.tire_inner_temperature_right = kelvin_to_celsius(w.mTireInnerLayerTemperature[2]);
        o.tire_carcass_temperature = kelvin_to_celsius(w.mTireCarcassTemperature);
        o.tire_optimal_temperature = w.mOptimalTemp;
        o.tire_wear = w.mWear;
        o.tire_vertical_deflection = w.mVerticalTireDeflection;
        o.tire_compound = w.mCompoundIndex;
        o.tire_compound_type = w.mCompoundType;
        o.tire_flat = w.mFlat;
        o.wheel_detached = w.mDetached;
    }
}

namespace {
    // LMU fills fixed-size char arrays; empty ones become NULL.
    template<std::size_t N>
    std::optional<std::string> text(const char (&chars)[N]) {
        const std::size_t length = strnlen(chars, N);
        if (length == 0) return std::nullopt;
        return std::string(chars, length);
    }

    // LMU reports times it does not have yet as 0 or -1.
    std::optional<double> time_or_null(double seconds) {
        if (seconds > 0) return seconds;
        return std::nullopt;
    }

    // LMU's sector times are cumulative (sector 2 includes sector 1); this gives the sector on its own.
    std::optional<double> difference(double later, double earlier) {
        if (later > 0 && earlier > 0 && later > earlier) return later - earlier;
        return std::nullopt;
    }

    std::optional<std::string> game_phase(unsigned char phase) {
        switch (phase) {
            case 0:
                return "before_start";
            case 1:
                return "reconnaissance";
            case 2:
                return "grid_walk";
            case 3:
                return "formation_lap";
            case 4:
                return "countdown";
            case 5:
                return "green";
            case 6:
                return "full_course_yellow";
            case 7:
                return "stopped";
            case 8:
                return "over";
            case 9:
                return "paused";
            case 10:
                return "under_yellow";  // Per-car phases only, from here on.
            case 11:
                return "under_blue";
            default:
                return std::nullopt;
        }
    }

    lambiase_session_info_s convert(const ScoringInfoV01& in) {
        lambiase_session_info_s out;
        out.session_time = in.mCurrentET;
        out.time_remaining = in.mSessionTimeRemaining;
        out.time_of_day = in.mTimeOfDay;
        out.track = text(in.mTrackName);
        out.track_length = in.mLapDist;
        // mSession: 0 test day, 1-4 practice, 5-8 qualifying, 9 warmup, 10-13 race.
        if (in.mSession == 0) {
            out.session_type = "test";
        } else if (in.mSession >= 1 && in.mSession <= 4) {
            out.session_type = "practice";
            out.session_number = in.mSession;
        } else if (in.mSession >= 5 && in.mSession <= 8) {
            out.session_type = "qualifying";
            out.session_number = in.mSession - 4;
        } else if (in.mSession == 9) {
            out.session_type = "warmup";
        } else if (in.mSession >= 10 && in.mSession <= 13) {
            out.session_type = "race";
            out.session_number = in.mSession - 9;
        }
        out.phase = game_phase(in.mGamePhase);
        out.session_end_time = time_or_null(in.mEndET);
        // An unlimited lap count is reported as the largest long.
        if (in.mMaxLaps > 0 && in.mMaxLaps < LONG_MAX) out.max_laps = in.mMaxLaps;
        out.event_start_time_of_day = in.mStartET;
        out.vehicle_count = in.mNumVehicles;
        if (in.mMaxPlayers > 0) out.max_vehicles = in.mMaxPlayers;
        switch (in.mYellowFlagState) {
            case 0:
                out.full_course_yellow = "none";
                break;
            case 1:
                out.full_course_yellow = "pending";
                break;
            case 2:
                out.full_course_yellow = "pits_closed";
                break;
            case 3:
                out.full_course_yellow = "pit_lead_lap";
                break;
            case 4:
                out.full_course_yellow = "pits_open";
                break;
            case 5:
                out.full_course_yellow = "last_lap";
                break;
            case 6:
                out.full_course_yellow = "resume";
                break;
            case 7:
                out.full_course_yellow = "race_halt";
                break;
            default:
                break;
        }
        out.local_yellow_1 = in.mSectorFlag[0] != 0;
        out.local_yellow_2 = in.mSectorFlag[1] != 0;
        out.local_yellow_3 = in.mSectorFlag[2] != 0;
        out.start_light = in.mStartLight;
        out.red_lights = in.mNumRedLights;
        out.in_realtime = in.mInRealtime;
        out.player_name = text(in.mPlayerName);
        out.server_name = text(in.mServerName);
        switch (in.mGameMode) {
            case 1:
                out.online_role = "server";
                break;
            case 2:
                out.online_role = "client";
                break;
            case 3:
                out.online_role = "server_and_client";
                break;
            default:
                break;
        }
        out.fixed_setup = in.mIsFixedSetup;
        out.rain = in.mRaining;
        out.cloud_darkness = in.mDarkCloud;
        out.cloud_coverage = in.mCloudCoverage;
        out.ambient_temperature = in.mAmbientTemp;
        out.track_temperature = in.mTrackTemp;
        out.wind_x = in.mWind.x;
        out.wind_y = in.mWind.y;
        out.wind_z = in.mWind.z;
        out.track_wetness_min = in.mMinPathWetness;
        out.track_wetness_max = in.mMaxPathWetness;
        out.track_wetness_average = in.mAvgPathWetness;
        out.track_grip_level = in.mTrackGripLevel;
        out.track_limits_steps_per_penalty = in.mTrackLimitsStepsPerPenalty;
        out.track_limits_steps_per_point = in.mTrackLimitsStepsPerPoint;
        return out;
    }

    lambiase_standing_s convert(const VehicleScoringInfoV01& in, double session_time) {
        lambiase_standing_s out;
        out.session_time = session_time;
        out.vehicle_id = in.mID;
        out.is_player = in.mIsPlayer;
        out.driver = text(in.mDriverName);
        out.vehicle = text(in.mVehicleName);
        out.vehicle_class = text(in.mVehicleClass);
        out.team = text(in.mPitGroup);
        out.position = in.mPlace;
        if (in.mQualification > 0) out.qualifying_position = in.mQualification;
        out.laps_completed = in.mTotalLaps;
        // LMU numbers sectors 1, 2, 0 (sector 3 is 0); telemetry uses zero-based 0, 1, 2.
        if (in.mSector >= 0 && in.mSector <= 2) out.sector = (in.mSector + 2) % 3;
        switch (in.mFinishStatus) {
            case 0:
                out.finish_status = "none";
                break;
            case 1:
                out.finish_status = "finished";
                break;
            case 2:
                out.finish_status = "dnf";
                break;
            case 3:
                out.finish_status = "disqualified";
                break;
            default:
                break;
        }
        out.lap_start_time = in.mLapStartET;
        out.last_lap_time = time_or_null(in.mLastLapTime);
        out.last_sector_1 = time_or_null(in.mLastSector1);
        out.last_sector_2 = difference(in.mLastSector2, in.mLastSector1);
        out.last_sector_3 = difference(in.mLastLapTime, in.mLastSector2);
        out.best_lap_time = time_or_null(in.mBestLapTime);
        out.best_lap_sector_1 = time_or_null(in.mBestLapSector1);
        out.best_sector_1 = time_or_null(in.mBestSector1);
        out.best_sectors_1_and_2 = time_or_null(in.mBestSector2);
        out.current_sector_1 = time_or_null(in.mCurSector1);
        out.current_sector_2 = difference(in.mCurSector2, in.mCurSector1);
        out.laps_behind_leader = in.mLapsBehindLeader;
        out.laps_behind_next = in.mLapsBehindNext;
        out.pit_stops = in.mNumPitstops;
        out.penalties = in.mNumPenalties;
        out.in_pits = in.mInPits;
        switch (in.mPitState) {
            case 0:
                out.pit_state = "none";
                break;
            case 1:
                out.pit_state = "requested";
                break;
            case 2:
                out.pit_state = "entering";
                break;
            case 3:
                out.pit_state = "stopped";
                break;
            case 4:
                out.pit_state = "exiting";
                break;
            default:
                break;
        }
        out.in_garage = in.mInGarageStall;
        out.pit_lap_distance = in.mPitLapDist;
        switch (in.mControl) {
            case 0:
                out.control = "player";
                break;
            case 1:
                out.control = "ai";
                break;
            case 2:
                out.control = "remote";
                break;
            case 3:
                out.control = "replay";
                break;
            default:
                break;
        }
        // The car's own phase: the session phases, plus 9 for after the formation lap.
        out.phase = in.mIndividualPhase == 9 ? std::optional<std::string>("after_formation")
                                             : game_phase(in.mIndividualPhase);
        if (in.mFlag == 0) out.flag = "green";
        if (in.mFlag == 6) out.flag = "blue";
        out.under_yellow = in.mUnderYellow;
        out.lap_counts = in.mCountLapFlag >= 1;
        out.lap_time_counts = in.mCountLapFlag == 2;
        out.scored = in.mServerScored != 0;
        out.headlights = in.mHeadlights != 0;
        out.drs_active = in.mDRSState;
        out.attack_mode = in.mAttackMode;
        out.time_behind_leader = in.mTimeBehindLeader;
        out.time_behind_next = in.mTimeBehindNext;
        out.lap_distance = in.mLapDist;
        out.time_into_lap = in.mTimeIntoLap;
        out.estimated_lap_time = time_or_null(in.mEstimatedLapTime);
        out.fuel_fraction = in.mFuelFraction / 255.0;
        out.lateral_position = in.mPathLateral;
        out.track_edge = in.mTrackEdge;
        return out;
    }

    // What LMU's weather access adds to the weather in the scoring info.
    struct Weather {
        double cloudiness = 0.0;
        double wind_max_speed = 0.0;
    };

    class Plugin : public InternalsPluginV07 {
    public:
        ~Plugin() override {
            close();
        }

        // Every vehicle, not just the player's.
        long WantsTelemetryUpdates() override {
            return 2;
        }

        bool WantsScoringUpdates() override {
            return true;
        }

        // For reading the weather only: AccessWeather passes the current weather in, and returning false
        // leaves it unchanged.
        bool WantsWeatherAccess() override {
            return true;
        }

        bool AccessWeather(double /*track_node_size*/, WeatherControlInfoV01& info) override {
            std::lock_guard lock(_weather_mutex);
            _weather = Weather{.cloudiness = info.mCloudiness, .wind_max_speed = info.mWindMaxSpeed};
            return false;
        }

        // mPath[0] is LMU's UserData folder; it may be relative to the game's working folder.
        void SetEnvironment(const EnvironmentInfoV01& info) override {
            if (info.mPath[0] && *info.mPath[0]) {
                std::error_code ec;
                const std::filesystem::path user_data = std::filesystem::absolute(info.mPath[0], ec);
                if (!ec) _user_data = user_data;
            }
            // A database left from an earlier run (the game crashed) would look like a session to the app
            // until the next one starts.
            if (!_recorder) {
                try {
                    const std::wstring dir = database_dir().native();
                    lambiase_lmu_remove_database(dir.data(), dir.size());
                } catch (...) {
                }
            }
        }

        // The database is created when the first session loads (while the pilot is still in the garage), not
        // when the game starts. It lives in UserData\lambiase, where the app and the skill's scripts look
        // for it while the game runs, holds a whole weekend, and is deleted when the game closes.
        void StartSession() override {
            start_recording();
        }

        // A track loading is a new weekend (its sessions come and go without loading it again): the last
        // weekend's data goes, while the practice, qualifying and race of this one stay together.
        void Load() override {
            if (_recorder) lambiase_lmu_clear(_recorder);
        }

        // In case the plugin missed the session starting.
        void EnterRealtime() override {
            start_recording();
        }

        void Shutdown() override {
            close();
        }

        // Called on the game's simulation thread for every car, so only the updates the recorder keeps are
        // converted.
        void UpdateTelemetry(const TelemInfoV01& info) override {
            if (!_recorder) return;
            const double speed = std::hypot(info.mLocalVel.x, info.mLocalVel.y, info.mLocalVel.z);
            if (!lambiase_lmu_wants_telemetry(_recorder, info.mID, info.mElapsedTime, speed)) return;
            lambiase_telemetry_s telemetry;
            convert(info, telemetry);
            lambiase_lmu_push_telemetry(_recorder, &telemetry);
        }

        // About five times a second.
        void UpdateScoring(const ScoringInfoV01& info) override {
            if (!_recorder || !info.mVehicle) return;
            lambiase_session_info_s session = convert(info);
            {
                std::lock_guard lock(_weather_mutex);
                if (_weather) {
                    session.cloudiness = _weather->cloudiness;
                    session.wind_max_speed = _weather->wind_max_speed;
                }
            }
            std::vector<lambiase_standing_s> standings;
            standings.reserve(static_cast<std::size_t>(std::max(info.mNumVehicles, 0L)));
            for (long i = 0; i < info.mNumVehicles; ++i) {
                standings.push_back(convert(info.mVehicle[i], info.mCurrentET));
            }
            // Moves them out, leaving them empty.
            lambiase_lmu_push_scoring(_recorder, &session, standings.data(), standings.size());
        }

    private:
        std::filesystem::path database_dir() const {
            const std::filesystem::path user_data =
                    _user_data.empty() ? std::filesystem::absolute("UserData") : _user_data;
            return user_data / "lambiase";
        }

        void start_recording() {
            if (_recorder) return;
            try {
                const std::wstring dir = database_dir().native();
                // Null if it can't: nothing is recorded this run, and the game carries on.
                _recorder = lambiase_lmu_open(dir.data(), dir.size());
            } catch (...) {
            }
        }

        // Stops recording and deletes the database. A reader that still has it open (the app only lets go once
        // Steam reports the game closed, which is after this) keeps it from being deleted: the app deletes it
        // once it has let go, and failing that the next run empties it.
        void close() {
            if (_recorder) lambiase_lmu_close(std::exchange(_recorder, nullptr));
        }

        std::filesystem::path _user_data;
        lambiase_lmu_recorder* _recorder = nullptr;
        std::mutex _weather_mutex;
        std::optional<Weather> _weather;
    };
}  // namespace

// The functions LMU looks up when it loads a plugin DLL.
extern "C" {
__declspec(dllexport) const char* __cdecl GetPluginName() {
    return "lambiase";
}

__declspec(dllexport) PluginObjectType __cdecl GetPluginType() {
    return PO_INTERNALS;
}

// Must match the InternalsPluginV0x class the plugin derives from.
__declspec(dllexport) int __cdecl GetPluginVersion() {
    return 7;
}

__declspec(dllexport) PluginObject* __cdecl CreatePluginObject() {
    return new (std::nothrow) Plugin;
}

__declspec(dllexport) void __cdecl DestroyPluginObject(PluginObject* object) {
    delete static_cast<Plugin*>(object);
}
}
