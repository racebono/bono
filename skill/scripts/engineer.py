#!/usr/bin/env python3
"""Read-only race-engineer queries over a my_engineer telemetry database (SQLite).

Usage (--db is optional: by default the database is the one a running simulator's my_engineer plugin
is writing, e.g. Le Mans Ultimate's UserData/my_engineer/telemetry.db, found through Steam):
  engineer.py --db PATH warm              # build/refresh the history cache (run once per session)
  engineer.py --db PATH quick             # URGENT: gaps, position, cars alongside, energy, tyres, alerts - latest frame only
  engineer.py --db PATH status            # full snapshot of the player's car
  engineer.py --db PATH laps [--of VEHICLE_ID_OR_DRIVER] [--last K]
  engineer.py --db PATH stint [--race-laps-left N | --race-time-left SECONDS]
  engineer.py --db PATH nearby [--radius METERS] [--trend]
  engineer.py --db PATH field
  engineer.py --db PATH track             # the circuit: turns (names, left/right, how tight), straights, sectors, pit lane, time lost in the pits
  engineer.py --db PATH corners [--lap N] [--ref N|best] [--ref-of DRIVER] [--turn T]   # time lost per corner, braking points, apex and exit speeds
  engineer.py --db PATH sql "SELECT ..."  [--limit N]

Only the Python standard library is used. The database is opened read-only. Latest-frame commands
(quick, nearby, status' live part) are O(1) id lookups. History (laps, stint, field) comes from an
incremental cache in the temp directory, so only rows added since the last call are scanned.
Scoring (the `session` and `standings` tables: track, session, time left, flags, weather, official
positions, driver names, classes, lap and sector times, pit stops, penalties) is read from the newest
rows only, so it costs a few milliseconds too. Databases without those tables still work.
"""
import argparse
import hashlib
import json
import tempfile
import math
import os
import pathlib
import re
import sqlite3
import subprocess
import sys
import time

WHEELS = ["FL", "FR", "RL", "RR"]
FRAME_WINDOW = 110  # max cars LMU maps (104) plus slack
FRAME_SCAN = 1500   # rows searched around the player's latest row for every other car's latest row


# --------------------------------------------------------------------------- helpers
def die(msg):
    print(f"ERROR: {msg}")
    sys.exit(2)


def fmt_lap(sec):
    if sec is None:
        return "-"
    m, s = divmod(sec, 60)
    return f"{int(m)}:{s:06.3f}" if m else f"{s:.3f}"


def fmt(v, nd=1, unit=""):
    if v is None:
        return "-"
    return f"{v:.{nd}f}{unit}"


def pct(v):
    return "-" if v is None else f"{v * 100:.1f}%"


# --------------------------------------------------------------------------- car_state
# Driver aids, car settings, damage, pit stops and track-limit steps change only now and then, so the
# plugin writes them to `car_state` only when they change for a car. A car's values at a telemetry row are
# its latest car_state row at or before that row. Files from before car_state keep them in telemetry.
STATE_KEYS = ("id", "telemetry_id", "vehicle_id", "session_time")
_state_columns = {}


def state_columns(conn):
    """The car_state value columns, or [] when the file has no car_state table."""
    if id(conn) not in _state_columns:
        _state_columns[id(conn)] = [r[1] for r in conn.execute("PRAGMA table_info(car_state)") if r[1] not in STATE_KEYS]
    return _state_columns[id(conn)]


def car_state(conn, vehicle, telemetry_id):
    """A car's car_state row in force at a telemetry row, or None."""
    return conn.execute("SELECT * FROM car_state WHERE vehicle_id = ? AND telemetry_id <= ? "
                        "ORDER BY telemetry_id DESC LIMIT 1", (vehicle, telemetry_id)).fetchone()


def as_dict(row):
    """A sqlite3.Row (or a dict) as a new dict."""
    return dict(row) if isinstance(row, dict) else dict(zip(row.keys(), tuple(row)))


DAMAGE_PARTS = (("engine_damage", "engine"), ("transmission_damage", "gearbox"),
                ("aerodynamics_damage", "aero"), ("suspension_damage", "suspension"))


def damage_parts(p, above=0.0):
    """'engine 12%, aero 30%': the parts damaged more than `above` (0-1), from a with_state row."""
    return ", ".join(f"{name} {p[k] * 100:.0f}%" for k, name in DAMAGE_PARTS if p.get(k) is not None and p[k] > above)


def column(row, name):
    """A column of a sqlite3.Row, or None when the row is missing or the file predates the column."""
    return row[name] if row is not None and name in row.keys() else None


def clock(seconds):
    return f"{int(seconds // 60)}:{int(seconds % 60):02d}"


def pit_window(ctx):
    """(text, closing): the pit window and the player's mandatory stop, and whether a stop still owed has
    to be made within about a lap. text is None when the sim reports neither."""
    sc = ctx.scoring()
    s, me, now = sc.session, sc.player, ctx.player["session_time"]
    parts, closing = [], False
    status = column(s, "pit_window_status")
    if status:
        parts.append(f"pit window {status}")
    start_lap, end_lap = column(s, "pit_window_start_lap"), column(s, "pit_window_end_lap")
    start_t, end_t = column(s, "pit_window_start_time"), column(s, "pit_window_end_time")
    if start_lap is not None and end_lap is not None:
        parts.append(f"laps {start_lap} to {end_lap} (you are on lap {ctx.player['lap']})")
    elif start_t is not None and end_t is not None and now is not None:
        when = (f"opens in {clock(start_t - now)}" if now < start_t else
                f"closes in {clock(end_t - now)}" if now < end_t else "closed")
        parts.append(f"session time {clock(start_t)} to {clock(end_t)}, {when}")
    owed = column(me, "mandatory_pit_stop")
    if owed and owed != "none":
        parts.append("mandatory stop " + owed.replace("_", " "))
        if owed.startswith("unserved") and status in ("open", "closed"):
            lap_time = column(me, "best_lap_time") or column(me, "last_lap_time") or 120
            closing = ((end_lap is not None and ctx.player["lap"] is not None and ctx.player["lap"] >= end_lap - 1) or
                       (end_t is not None and now is not None and 0 <= end_t - now < 1.5 * lap_time))
    return (", ".join(parts) if parts else None), closing


def with_state(conn, row):
    """A telemetry row as a dict, with its car's car_state values at that row added."""
    d = as_dict(row)
    cols = state_columns(conn)
    if cols:
        s = car_state(conn, d["vehicle_id"], d["id"])
        for c in cols:
            d[c] = s[c] if s is not None else None
    return d


# --------------------------------------------------------------------------- where the database is
# Each supported simulator knows where its my_engineer plugin writes the database. Support for another
# simulator is another Simulator subclass in SIMULATORS.
class Simulator:
    name = ""

    def database(self):
        """Path of the database the simulator's plugin is writing right now, or None when the simulator
        is not installed, not running, or the pilot has not been on track yet."""
        raise NotImplementedError


def _steam_key(path):
    import winreg  # Windows only; Steam games are only looked for there
    return winreg.OpenKey(winreg.HKEY_CURRENT_USER, path)


def steam_app_running(app_id):
    """Steam's "Running" flag for a game: set while the game process is alive."""
    try:
        import winreg
        with _steam_key(rf"Software\Valve\Steam\Apps\{app_id}") as key:
            return bool(winreg.QueryValueEx(key, "Running")[0])
    except (ImportError, OSError):
        return False


def _vdf_values(path, key):
    """The values of every `"key"  "value"` line in a Steam .vdf/.acf file, unescaped."""
    try:
        with open(path, encoding="utf-8") as f:
            text = f.read()
    except OSError:
        return []
    pattern = r'^\s*"' + re.escape(key) + r'"\s+"((?:[^"\\]|\\.)*)"'
    return [re.sub(r"\\(.)", r"\1", m) for m in re.findall(pattern, text, re.M)]


_install_dirs = {}


def steam_app_dir(app_id):
    """The folder a Steam game is installed in, found through Steam's library folders; None if Steam or
    the game is not installed."""
    if app_id in _install_dirs:
        return _install_dirs[app_id]
    try:
        import winreg
        with _steam_key(r"Software\Valve\Steam") as key:
            steam = winreg.QueryValueEx(key, "SteamPath")[0]
    except (ImportError, OSError):
        return None
    libraries = [steam] + _vdf_values(os.path.join(steam, "steamapps", "libraryfolders.vdf"), "path")
    for library in libraries:
        folder = _vdf_values(os.path.join(library, "steamapps", f"appmanifest_{app_id}.acf"), "installdir")
        if folder and os.path.isdir(os.path.join(library, "steamapps", "common", folder[0])):
            _install_dirs[app_id] = os.path.normpath(os.path.join(library, "steamapps", "common", folder[0]))
            return _install_dirs[app_id]
    return None


class LeMansUltimate(Simulator):
    """The plugin writes UserData/my_engineer/telemetry.db in the game's folder the first time the pilot
    drives and deletes it when the game closes. A file left behind by a crash is replaced the next time
    the pilot drives, so the file only counts while Steam reports the game running."""
    name = "Le Mans Ultimate"
    STEAM_APP_ID = 2399420

    def database(self):
        if not steam_app_running(self.STEAM_APP_ID):
            return None
        folder = steam_app_dir(self.STEAM_APP_ID)
        path = folder and os.path.join(folder, "UserData", "my_engineer", "telemetry.db")
        return path if path and os.path.exists(path) else None


class RaceRoom(Simulator):
    """RaceRoom has no plugins: the my_engineer app reads its shared memory and writes
    %LOCALAPPDATA%/my_engineer/raceroom/telemetry.db while the game runs, deleting it when the game
    closes. Like LMU's, the file only counts while Steam reports the game running."""
    name = "RaceRoom Racing Experience"
    STEAM_APP_ID = 211500

    def database(self):
        if not steam_app_running(self.STEAM_APP_ID):
            return None
        base = os.environ.get("LOCALAPPDATA") or tempfile.gettempdir()
        path = os.path.join(base, "my_engineer", "raceroom", "telemetry.db")
        return path if os.path.exists(path) else None


SIMULATORS = [LeMansUltimate(), RaceRoom()]


def live_database():
    """(simulator, path) of the first simulator writing a database right now, or (None, None)."""
    for sim in SIMULATORS:
        path = sim.database()
        if path:
            return sim, path
    return None, None


def find_database():
    sim, path = live_database()
    if path is None:
        names = " or ".join(s.name for s in SIMULATORS)
        die(f"no current database: no running {names} with the my_engineer plugin has written one yet. "
            "Is the game running, and has the pilot been out on track? Otherwise ask for the path and pass --db.")
    return path


def connect(path):
    if not os.path.exists(path):
        die(f"database not found: {path}")
    uri = pathlib.Path(path).resolve().as_uri() + "?mode=ro"
    try:
        conn = sqlite3.connect(uri, uri=True, timeout=2.0)
    except sqlite3.OperationalError:
        conn = sqlite3.connect(path, timeout=2.0)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA query_only = 1")
    return conn


def columns(conn, table):
    return {r[1] for r in conn.execute(f"PRAGMA table_info({table})")}


class Ctx:
    """Resolves the player's vehicle and the id range of the current session."""

    def __init__(self, conn, db_path, vehicle=None):
        self.conn = conn
        self.db_path = db_path
        self._history = None
        cols = columns(conn, "telemetry")
        if not cols:
            die("no `telemetry` table in this file - is this the my_engineer database?")
        self.has_is_player = "is_player" in cols
        if vehicle is None:
            if not self.has_is_player:
                ids = [r[0] for r in conn.execute(
                    "SELECT DISTINCT vehicle_id FROM telemetry WHERE id > (SELECT MAX(id) FROM telemetry) - 400")]
                die("this database has no is_player column, so the player's car is unknown. "
                    f"Ask the pilot for their vehicle_id and pass --vehicle. Vehicles in the latest frames: {ids}")
            row = conn.execute("SELECT id, vehicle_id FROM telemetry WHERE is_player = 1 ORDER BY id DESC LIMIT 1").fetchone()
            if row is None:
                die("no rows flagged is_player = 1 (spectating, or the player is not in a car yet)")
            self.vehicle = row["vehicle_id"]
            self.last_id = row["id"]
            self.player_filter = "is_player = 1"
        else:
            row = conn.execute("SELECT id FROM telemetry WHERE vehicle_id = ? ORDER BY id DESC LIMIT 1", (vehicle,)).fetchone()
            if row is None:
                die(f"no rows for vehicle_id {vehicle}")
            self.vehicle = vehicle
            self.last_id = row["id"]
            self.player_filter = f"vehicle_id = {int(vehicle)}"
        self.player = with_state(conn, conn.execute("SELECT * FROM telemetry WHERE id = ?", (self.last_id,)).fetchone())

    def history(self):
        """Per-lap aggregates for the current session, kept in an incremental cache file."""
        if self._history is None:
            self._history = History(self)
        return self._history

    def scoring(self):
        if getattr(self, "_scoring", None) is None:
            self._scoring = Scoring(self)
        return self._scoring

    @property
    def session_start_id(self):
        return self.history().session_start_id

    def frame(self):
        """Latest row of every car around the player's latest row (one logger frame). O(1)."""
        rows = self.conn.execute(
            "SELECT * FROM telemetry WHERE id BETWEEN ? AND ? ORDER BY id",
            (self.last_id - FRAME_SCAN, self.last_id + FRAME_WINDOW)).fetchall()
        t, pid = self.player["session_time"], self.last_id
        best = {}
        for r in rows:  # per car, the row closest in time (then in id) to the player's row
            if r["session_time"] is None:
                continue
            key = (abs(r["session_time"] - t), abs(r["id"] - pid))
            if key[0] < 1.0 and (r["vehicle_id"] not in best or key < best[r["vehicle_id"]][0]):
                best[r["vehicle_id"]] = (key, r)
        return {v: r for v, (_, r) in best.items()}

    def wheels(self, tid):
        return {r["wheel"]: r for r in self.conn.execute(
            "SELECT * FROM telemetry_wheel WHERE telemetry_id = ? ORDER BY wheel", (tid,))}


class Scoring:
    """Latest `session` row and latest `standings` row per car for the current session.

    The plugin writes a scoring row only when one of its 'compared' columns changes (position, laps,
    sector, pit state, flag, ...), so the newest row of a car can be seconds or minutes old. Columns
    that change continuously (time_remaining, time_behind_*, lap_distance, time_into_lap, fuel_fraction)
    are snapshots from that moment - use telemetry for anything live."""

    SCAN = 4000
    MAX_ROWS = 60000

    def __init__(self, ctx):
        self.ctx = ctx
        conn = ctx.conn
        tables = {r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type = 'table'")}
        self.tables = tables
        self.available = {"session", "standings"} <= tables
        self.session = None
        self.cars = {}
        self.player = None
        if not self.available:
            return
        rows = conn.execute("SELECT * FROM session ORDER BY id DESC LIMIT 1").fetchall()
        self.session = rows[0] if rows else None
        expected = (self.session["vehicle_count"] if self.session and self.session["vehicle_count"] else None)
        # walk back from the newest standings row until every car is found or the session starts
        top = conn.execute("SELECT MAX(id) FROM standings").fetchone()[0] or 0
        hi, newer_t, scanned = top, None, 0
        while hi > 0 and scanned < self.MAX_ROWS:
            lo = max(1, hi - self.SCAN + 1)
            boundary = False
            for r in conn.execute("SELECT * FROM standings WHERE id BETWEEN ? AND ? ORDER BY id DESC", (lo, hi)):
                t = r["session_time"]
                if newer_t is not None and t is not None and t > newer_t + 1.0:
                    boundary = True  # session_time jumps up going back in time: previous session
                    break
                if t is not None:
                    newer_t = t
                self.cars.setdefault(r["vehicle_id"], r)
            scanned += hi - lo + 1
            if boundary or (expected and len(self.cars) >= expected):
                break
            hi = lo - 1
        # ctx.vehicle is the is_player car unless --vehicle chose another one.
        self.player = self.cars.get(ctx.vehicle) or next((r for r in self.cars.values() if r["is_player"]), None)

    def order(self):
        return sorted(self.cars.values(), key=lambda r: (r["position"] or 999))

    def class_position(self, r):
        same = [c for c in self.order() if c["vehicle_class"] == r["vehicle_class"]]
        return next((i + 1 for i, c in enumerate(same) if c["vehicle_id"] == r["vehicle_id"]), None)

    def multiclass(self):
        return len({r["vehicle_class"] for r in self.cars.values() if r["vehicle_class"]}) > 1

    def label(self, vid, short=False):
        """'Fischer (Hypercar)' style name for a car, or 'car N' without scoring."""
        r = self.cars.get(vid)
        if not r or not r["driver"]:
            return f"car {vid}"
        name = r["driver"].split()[-1] if short else r["driver"]
        return f"{name} ({r['vehicle_class']}, car {vid})" if r["vehicle_class"] and not short else f"{name} (car {vid})"

    def time_left(self, now):
        """Seconds left in the session, from the session end time (live) or the last snapshot."""
        s = self.session
        if not s:
            return None
        if s["session_end_time"] is not None and now is not None:
            return max(0.0, s["session_end_time"] - now)
        if s["time_remaining"] is not None and s["session_time"] is not None and now is not None:
            return max(0.0, s["time_remaining"] - (now - s["session_time"]))
        return None

    def session_line(self, now):
        s = self.session
        if not s:
            return None
        parts = [f"{s['session_type'] or 'session'}{(' ' + str(s['session_number'])) if s['session_number'] and s['session_type'] != 'race' else ''}",
                 s["phase"] or "-"]
        left = self.time_left(now)
        if s["max_laps"]:
            leader = self.order()[0] if self.cars else None
            done = leader["laps_completed"] if leader else None
            parts.append(f"{s['max_laps']} laps" + (f", leader has done {done}" if done is not None else ""))
        if left is not None:
            parts.append(f"{int(left // 60)}:{int(left % 60):02d} left")
        if s["full_course_yellow"] and s["full_course_yellow"] != "none":
            parts.append(f"FULL COURSE YELLOW ({s['full_course_yellow']})")
        yellows = [str(i) for i in (1, 2, 3) if s[f"local_yellow_{i}"]]
        if yellows:
            parts.append("local yellow flag in sector index " + ",".join(yellows))
        if s["track"]:
            parts.append(s["track"])
        return " | ".join(parts)

    def weather_line(self):
        s = self.session
        if not s:
            return None

        def col(name):  # columns added later are missing from older files
            return s[name] if name in s.keys() else None

        bits = []
        if s["ambient_temperature"] is not None:
            bits.append(f"air {s['ambient_temperature']:.0f} C")
        if s["track_temperature"] is not None:
            bits.append(f"track {s['track_temperature']:.0f} C")
        if s["rain"] is not None:
            bits.append(f"rain {s['rain'] * 100:.0f}%")
        if s["track_wetness_average"] is not None:
            bits.append(f"track wetness avg {s['track_wetness_average'] * 100:.0f}%"
                        f" (min {fmt((s['track_wetness_min'] or 0) * 100, 0)}%, max {fmt((s['track_wetness_max'] or 0) * 100, 0)}%)")
        if s["cloud_darkness"] is not None:
            bits.append(f"cloud darkness {s['cloud_darkness'] * 100:.0f}%")
        if col("cloudiness") is not None:
            bits.append(f"cloudiness {col('cloudiness') * 100:.0f}%")
        if s["cloud_coverage"] is not None:
            bits.append(f"cloud coverage {s['cloud_coverage']}")
        if s["wind_x"] is not None and s["wind_z"] is not None:
            wind = math.hypot(s["wind_x"], s["wind_z"])
            gust = col("wind_max_speed")
            bits.append(f"wind {wind * 3.6:.0f} km/h" + (f" (max {gust * 3.6:.0f})" if gust else ""))
        if s["track_grip_level"] is not None:
            bits.append(f"grip level {s['track_grip_level']}")
        return ", ".join(bits) if bits else None

    # The game's sky numbers; anything else is described with the game's own text (in the game's language).
    SKY = {0: "clear", 1: "light clouds", 2: "partly cloudy", 3: "mostly cloudy", 4: "overcast",
           5: "overcast with drizzle", 6: "overcast with light rain"}

    def forecast(self, session_type):
        """The newest forecast for a session type ('race', 'qualifying'...): its points, start to finish."""
        if "forecast" not in self.tables:
            return []
        return self.ctx.conn.execute(
            "SELECT * FROM forecast WHERE session_type = ? AND session_time = "
            "(SELECT session_time FROM forecast ORDER BY id DESC LIMIT 1) ORDER BY session_fraction",
            (session_type,)).fetchall()

    def forecast_line(self, now):
        """The forecast from now to the end of this session, plus the race's if this is not it (LMU only)."""
        s = self.session
        kind = s["session_type"] if s else None
        if not kind:
            return None

        def point(r):
            sky = self.SKY.get(r["sky"]) or r["sky_text"] or "-"
            return f"{fmt((r['rain_chance'] or 0) * 100, 0)}% rain, {sky}, {fmt(r['temperature'], 0)} C"

        def mark(f):
            return "start" if f == 0 else "finish" if f == 1 else f"{f * 100:.0f}% mark"

        rows = self.forecast(kind)
        parts = []
        if rows:
            end = s["session_end_time"]
            progress = now / end if end and end > 0 and now is not None else None
            if progress is None:
                parts = [f"{mark(r['session_fraction'])} {point(r)}" for r in rows]
            else:
                done = [r for r in rows if r["session_fraction"] <= progress]
                ahead = [r for r in rows if r["session_fraction"] > progress]
                if done:
                    parts.append(f"now ({mark(done[-1]['session_fraction'])}) {point(done[-1])}")
                for r in ahead:
                    minutes = (r["session_fraction"] * end - now) / 60
                    parts.append(f"{mark(r['session_fraction'])} in {minutes:.0f} min: {point(r)}")
            wet = max(rows, key=lambda r: r["rain_chance"] or 0)
            if (wet["rain_chance"] or 0) > 0 and (progress is None or wet["session_fraction"] > progress):
                parts.append(f"rain most likely at the {mark(wet['session_fraction'])} ({wet['rain_chance'] * 100:.0f}%)")
        line = (f"{kind}" + (f" ({now / s['session_end_time'] * 100:.0f}% through)"
                             if s["session_end_time"] and now is not None and s["session_end_time"] > 0 else "")
                + ": " + " | ".join(parts)) if parts else None
        if kind != "race":
            race = self.forecast("race")
            if race:
                wet = max(race, key=lambda r: r["rain_chance"] or 0)
                summary = (f"race: up to {wet['rain_chance'] * 100:.0f}% rain chance at the {mark(wet['session_fraction'])}"
                           if (wet["rain_chance"] or 0) > 0 else "race: dry")
                temps = [r["temperature"] for r in race if r["temperature"] is not None]
                if temps:
                    summary += f", {fmt(min(temps), 0)}-{fmt(max(temps), 0)} C"
                line = (line + " || " if line else "") + summary
        return line

    def rain_ahead(self, now):
        """(highest rain chance from now to the end of this session, minutes until that point, rain on track now)
        from the forecast, or None (no forecast, or a session without an end time)."""
        s = self.session
        if not s or not s["session_type"]:
            return None
        rows = self.forecast(s["session_type"])
        end = s["session_end_time"]
        if not rows or not end or now is None:
            return None
        ahead = [r for r in rows if r["session_fraction"] * end > now]
        if not ahead:
            return None
        wet = max(ahead, key=lambda r: r["rain_chance"] or 0)
        return (wet["rain_chance"] or 0), (wet["session_fraction"] * end - now) / 60, s["rain"]

    def track_limits(self, steps):
        """(steps now, steps per penalty, points left before a penalty) or None."""
        s = self.session
        if not s or steps is None or not s["track_limits_steps_per_penalty"]:
            return None
        per_pen = s["track_limits_steps_per_penalty"]
        per_point = s["track_limits_steps_per_point"] or 1
        left = max(0, per_pen - steps)
        return steps, per_pen, math.ceil(left / per_point)


def resolve_vehicle(ctx, text):
    """A vehicle id from an id or (part of) a driver name."""
    if text is None:
        return None
    if str(text).lstrip("-").isdigit():
        return int(text)
    sc = ctx.scoring()
    hits = [r for r in sc.cars.values() if r["driver"] and text.lower() in r["driver"].lower()]
    if len(hits) == 1:
        return hits[0]["vehicle_id"]
    die(f"no single driver matches '{text}': " + ", ".join(f"{r['driver']} = {r['vehicle_id']}" for r in sc.order()))


def cache_path(ctx):
    key = hashlib.sha1(f"{os.path.abspath(ctx.db_path)}|{ctx.player_filter}".encode()).hexdigest()[:16]
    return os.path.join(tempfile.gettempdir(), f"race-engineer-{key}.json")


def warm_in_background(ctx, args):
    """Start `warm` as a detached process if there is no history cache yet, so an urgent first
    question never waits for the one-off history scan."""
    if os.path.exists(cache_path(ctx)):
        return
    cmd = [sys.executable, os.path.abspath(__file__), "--db", ctx.db_path]
    if args.vehicle is not None:
        cmd += ["--vehicle", str(args.vehicle)]
    kw = dict(stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if os.name == "nt":
        kw["creationflags"] = getattr(subprocess, "DETACHED_PROCESS", 0) | getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
    else:
        kw["start_new_session"] = True
    try:
        subprocess.Popen(cmd + ["warm"], **kw)
    except OSError:
        pass


class History:
    """Incremental per-(vehicle, lap) aggregates for the current session.

    The first call scans the session once, in short chunks so the logger is never blocked for
    long; the result is cached in the temp directory and every later call only reads rows added
    since (a few thousand rows - milliseconds). Run `warm` once when the pilot gives the path."""

    VERSION = 3
    CHUNK = 200_000

    def __init__(self, ctx):
        self.ctx = ctx
        conn = ctx.conn
        self.path = cache_path(ctx)
        c = None
        try:
            with open(self.path, encoding="utf-8") as f:
                c = json.load(f)
        except (OSError, ValueError):
            pass
        if c and c.get("version") == self.VERSION and c.get("last_id"):
            # the cached boundary row must still be the same row (guards against a recreated file)
            r = conn.execute("SELECT session_time, vehicle_id FROM telemetry WHERE id = ?", (c["last_id"],)).fetchone()
            if r is None or [r[0], r[1]] != c.get("last_row"):
                c = None
        if not c or c.get("version") != self.VERSION:
            c = {"version": self.VERSION, "last_id": 0, "last_row": None, "session_start_id": 1,
                 "player_t": None, "laps": {}}
        self.c = c
        self.scanned = 0
        top = conn.execute("SELECT MAX(id) FROM telemetry").fetchone()[0] or 0
        if top > c["last_id"]:
            self._update(c["last_id"] + 1, top)
            r = conn.execute("SELECT session_time, vehicle_id FROM telemetry WHERE id = ?", (top,)).fetchone()
            c["last_id"], c["last_row"] = top, [r[0], r[1]]
            tmp = self.path + f".{os.getpid()}.tmp"
            try:
                with open(tmp, "w", encoding="utf-8") as f:
                    json.dump(c, f)
                os.replace(tmp, self.path)
            except OSError:
                pass
        self.session_start_id = c["session_start_id"]

    def _update(self, a, b):
        conn, c = self.ctx.conn, self.c
        for lo in range(a, b + 1, self.CHUNK):
            hi = min(b, lo + self.CHUNK - 1)
            reset = None
            for rid, t in conn.execute(
                    f"SELECT id, session_time FROM telemetry WHERE {self.ctx.player_filter} AND id BETWEEN ? AND ? ORDER BY id",
                    (lo, hi)):
                if t is not None and c["player_t"] is not None and t < c["player_t"] - 1.0:
                    reset = (rid, t)
                if t is not None:
                    c["player_t"] = t
            if reset:  # new session: start from the first row of the frame the player's row belongs to
                rid, t = reset
                start = conn.execute("SELECT MIN(id) FROM telemetry WHERE id BETWEEN ? AND ? AND session_time <= ?",
                                     (rid - FRAME_WINDOW, rid, t + 0.5)).fetchone()[0] or rid
                c["session_start_id"], c["laps"] = start, {}
                lo = max(lo, start)
            # Track-limit steps and the last impact only ever grow during a session, so their value at a
            # lap's last row is the lap's maximum; with car_state, that is looked up per lap.
            state = bool(state_columns(conn))
            for r in conn.execute(f"""
                    SELECT vehicle_id, lap, MIN(id), MAX(id), MAX(lap_start_time), MAX(lap_invalidated),
                           MAX(in_pit_lane), MAX(speed),
                           {"NULL, NULL" if state else "MAX(track_limits_steps), MAX(last_impact_time)"}
                    FROM telemetry WHERE id BETWEEN ? AND ? GROUP BY vehicle_id, lap""", (lo, hi)):
                v = c["laps"].setdefault(str(r[0]), {})
                old = v.get(str(r[1]))
                new = list(r[2:])
                if state:
                    s = car_state(conn, r[0], new[1])
                    new[6], new[7] = (s["track_limits_steps"], s["last_impact_time"]) if s is not None else (None, None)
                if old:
                    new[0] = min(old[0], new[0])
                    new[1] = max(old[1], new[1])
                    for i in range(2, len(new)):
                        vals = [x for x in (old[i], new[i]) if x is not None]
                        new[i] = max(vals) if vals else None
                v[str(r[1])] = new
            self.scanned += hi - lo + 1

    def laps(self, vehicle):
        keys = ("first_id", "last_id", "start", "invalid", "pit", "vmax", "tl_steps", "impact")
        v = self.c["laps"].get(str(vehicle), {})
        return [dict(zip(keys, v[k]), lap=int(k)) for k in sorted(v, key=int)]

    def vehicles(self):
        return [int(k) for k in self.c["laps"]]


def staleness(path):
    try:
        age = time.time() - os.path.getmtime(path)
    except OSError:
        return None
    return age


# --------------------------------------------------------------------------- laps
def lap_table(ctx, vehicle):
    """Per-lap summary for one vehicle in the current session."""
    laps = ctx.history().laps(vehicle)
    # first/last row values for fuel, energy, wear, gaps
    for L in laps:
        f = ctx.conn.execute("SELECT fuel, virtual_energy, session_time FROM telemetry WHERE id = ?", (L["first_id"],)).fetchone()
        e = ctx.conn.execute("""SELECT fuel, virtual_energy, gap_to_place_ahead, gap_to_place_behind, session_time
                                FROM telemetry WHERE id = ?""", (L["last_id"],)).fetchone()
        L["fuel_start"], L["ve_start"] = f["fuel"], f["virtual_energy"]
        L["fuel_end"], L["ve_end"] = e["fuel"], e["virtual_energy"]
        L["gap_ahead_end"], L["gap_behind_end"] = e["gap_to_place_ahead"], e["gap_to_place_behind"]
        L["t_last"] = e["session_time"]
        L["wear_end"] = {w: r["tire_wear"] for w, r in ctx.wheels(L["last_id"]).items()}
        L["wear_start"] = {w: r["tire_wear"] for w, r in ctx.wheels(L["first_id"]).items()}
    prev_impact = None
    for i, L in enumerate(laps):
        nxt = laps[i + 1] if i + 1 < len(laps) else None
        L["time"] = (nxt["start"] - L["start"]) if nxt and nxt["start"] and L["start"] is not None else None
        L["complete"] = nxt is not None
        # consumption: start of this lap to start of next (falls back to last row)
        f1 = nxt["fuel_start"] if nxt else L["fuel_end"]
        v1 = nxt["ve_start"] if nxt else L["ve_end"]
        L["fuel_used"] = (L["fuel_start"] - f1) if L["fuel_start"] is not None and f1 is not None else None
        L["ve_used"] = (L["ve_start"] - v1) if L["ve_start"] is not None and v1 is not None else None
        L["refuelled"] = (L["fuel_used"] is not None and L["fuel_used"] < -0.5) or (L["ve_used"] is not None and L["ve_used"] < -0.01)
        L["contact"] = L["impact"] is not None and L["impact"] > 0 and L["impact"] != prev_impact
        prev_impact = L["impact"] if L["impact"] else prev_impact
        # a pit lap is in/out lap: the pit lane flag was set at some point during the lap
        L["green"] = bool(L["complete"] and not L["pit"] and not L["refuelled"])
    return laps


def wear_mode(laps):
    """LMU reports tire_wear as fraction REMAINING (1.0 = new) even though the schema says worn.
    Decide from the data: if the value falls during a stint it is 'remaining'."""
    for L in laps:
        if L["wear_start"] and L["wear_end"]:
            d = [L["wear_end"][w] - L["wear_start"][w] for w in L["wear_start"] if L["wear_end"].get(w) is not None and L["wear_start"][w] is not None]
            if d and abs(sum(d)) > 1e-4:
                return "remaining" if sum(d) < 0 else "worn"
    return "remaining"


def cmd_laps(ctx, args):
    vehicle = resolve_vehicle(ctx, args.of_vehicle) if args.of_vehicle is not None else ctx.vehicle
    laps = lap_table(ctx, vehicle)
    mode = wear_mode(laps)
    green = [L for L in laps if L["green"] and not L["invalid"]]
    best = min(green, key=lambda L: L["time"]) if green else None
    print(f"LAPS {ctx.scoring().label(vehicle)}{' (player)' if vehicle == ctx.vehicle else ''} - current session; "
          f"best valid green lap: {('L%d %s' % (best['lap'], fmt_lap(best['time']))) if best else '-'}")
    print("lap  time       flags              fuelL  VE%   gapAhead gapBehind  wear-remaining FL/FR/RL/RR")
    shown = laps[-args.last:] if args.last else laps
    for L in shown:
        flags = []
        if not L["complete"]:
            flags.append("IN-PROGRESS")
        if L["invalid"]:
            flags.append("INVALID")
        if L["pit"]:
            flags.append("PIT")
        if L["refuelled"]:
            flags.append("REFUEL")
        if L["contact"]:
            flags.append("CONTACT")
        if best and L is best:
            flags.append("BEST")
        def rem(w):
            v = L["wear_end"].get(w)
            if v is None:
                return "-"
            return f"{(v if mode == 'remaining' else 1 - v) * 100:.0f}"
        ve = "-" if L["ve_used"] is None or L["refuelled"] else (f"{L['ve_used'] * 100:.1f}" if (L["ve_start"] or 0) <= 1.0 else f"{L['ve_used']:.2f}")
        fu = "-" if L["fuel_used"] is None or L["refuelled"] else f"{L['fuel_used']:.2f}"
        print(f"{L['lap']:>3}  {fmt_lap(L['time']):>9}  {','.join(flags):<18} {fu:>5}  {ve:>5}  "
              f"{fmt(L['gap_ahead_end'], 2):>8} {fmt(L['gap_behind_end'], 2):>9}  "
              f"{'/'.join(rem(w) for w in range(4))}")
    print("(fuel/VE = used during that lap; gaps = standings gaps in seconds at the end of the lap)")


# --------------------------------------------------------------------------- stint / strategy
def race_need(ctx, args, frac, avg_time):
    """Laps the player still has to drive (including the rest of this one), and where that came from."""
    if args.race_laps_left is not None:
        return max(0.0, args.race_laps_left - frac), f"pilot says {args.race_laps_left:g} laps left incl. this one"
    sc = ctx.scoring()
    s = sc.session
    now = ctx.player["session_time"]
    if s is None:
        if args.race_time_left is None:
            return None, "unknown (no session data): ask the pilot or pass --race-laps-left / --race-time-left"
    if s is not None and s["session_type"] and s["session_type"] != "race" and args.race_time_left is None:
        note = f"this is {s['session_type']}, not a race"
    else:
        note = None
    needs = []
    me = sc.player
    if s is not None and s["max_laps"] and me is not None and me["laps_completed"] is not None:
        # a lapped car finishes when it crosses the line after the leader has done max_laps
        n = s["max_laps"] - (me["laps_behind_leader"] or 0) - me["laps_completed"] - frac
        needs.append((max(0.0, n), f"{s['max_laps']} lap race"))
    time_left = args.race_time_left if args.race_time_left is not None else sc.time_left(now)
    if time_left is not None and avg_time:
        # the flag falls when the leader crosses the line after the clock runs out; you then finish your lap
        t_flag = now + time_left
        leader = sc.order()[0] if sc.cars else None
        if leader is not None and leader["vehicle_id"] != ctx.vehicle:
            lrow = ctx.frame().get(leader["vehicle_id"])
            llap = leader["last_lap_time"] or leader["estimated_lap_time"] or avg_time
            if lrow is not None and lrow["lap_start_time"] is not None and llap:
                cross = lrow["lap_start_time"] + llap
                while cross < now + time_left:
                    cross += llap
                t_flag = cross
        else:
            cross = now + (1 - frac) * avg_time
            while cross < now + time_left:
                cross += avg_time
            t_flag = cross
        mine = now + (1 - frac) * avg_time
        n = 1 - frac
        while mine < t_flag - 0.5:
            mine += avg_time
            n += 1
        src = f"{int(time_left // 60)}:{int(time_left % 60):02d} on the clock" + (" (from the session)" if args.race_time_left is None else " (pilot)")
        needs.append((n, src + ", plus the lap to the flag"))
    if not needs:
        return None, note or "unknown: ask the pilot or pass --race-laps-left / --race-time-left"
    need, src = min(needs)
    return need, src + (f"; NOTE {note}" if note else "")


def cmd_stint(ctx, args):
    laps = lap_table(ctx, ctx.vehicle)
    # current stint = laps after the last refuel
    start = 0
    for i, L in enumerate(laps):
        if L["refuelled"] or L["pit"]:
            start = i + 1
    stint = laps[start:]
    green = [L for L in stint if L["green"]][-5:]
    if len(green) < 2:  # not enough in this stint: borrow from the previous one
        green = [L for L in laps if L["green"]][-5:]
    p = ctx.player
    cur = laps[-1] if laps else None
    lap_elapsed = p["session_time"] - p["lap_start_time"] if p["lap_start_time"] is not None else None
    avg_time = sum(L["time"] for L in green) / len(green) if green else None
    frac = min(0.99, lap_elapsed / avg_time) if avg_time and lap_elapsed is not None else 0.0
    print(f"STINT  laps since last stop: {len([L for L in stint if L['complete']])} complete + current lap {p['lap']} "
          f"(~{frac * 100:.0f}% done by time)")
    print(f"pace (last {len(green)} green laps): avg {fmt_lap(avg_time)}"
          + (f", trend {(green[-1]['time'] - green[0]['time']) / max(1, len(green) - 1):+.2f}s/lap" if len(green) > 1 else ""))

    def project(name, now, per_lap, as_pct):
        if now is None or not per_lap:
            print(f"{name}: no data")
            return None
        laps_left = now / per_lap
        show = (lambda v: f"{v * 100:.1f}%") if as_pct else (lambda v: f"{v:.2f} L")
        print(f"{name}: now {show(now)}, avg use {show(per_lap)}/lap -> {laps_left:.1f} laps from here "
              f"(runs out ~lap {p['lap'] + laps_left - frac:.1f})")
        return laps_left

    ve_use = [L["ve_used"] for L in green if L["ve_used"] is not None and L["ve_used"] > 0]
    fu_use = [L["fuel_used"] for L in green if L["fuel_used"] is not None and L["fuel_used"] > 0]
    ve_laps = project("virtual energy", p["virtual_energy"], sum(ve_use) / len(ve_use) if ve_use else None,
                      (p["virtual_energy"] or 0) <= 1.0) if p["virtual_energy"] not in (None, 0) else None
    fu_laps = project("fuel", p["fuel"], sum(fu_use) / len(fu_use) if fu_use else None, False)
    estimate = []
    if p.get("fuel_per_lap"):
        estimate.append(f"fuel {p['fuel_per_lap']:.2f} L/lap")
    if p.get("virtual_energy_per_lap"):
        estimate.append(f"virtual energy {p['virtual_energy_per_lap'] * 100:.1f}%/lap")
    if p.get("fuel_capacity"):
        estimate.append(f"tank {p['fuel_capacity']:.0f} L")
    if estimate:
        print("the game's own figures: " + ", ".join(estimate))
    limits = [x for x in (ve_laps, fu_laps) if x is not None]
    if limits:
        lim = min(limits)
        which = "virtual energy" if ve_laps is not None and lim == ve_laps else "fuel"
        print(f"LIMIT: {which} - {lim:.1f} laps of range from the current position; "
              f"last full lap you can complete is lap {int(p['lap'] + lim - frac) - 0}")
        need, source = race_need(ctx, args, frac, avg_time)
        if need is None:
            print(f"race length: {source}")
        else:
            margin = lim - need
            print(f"race: {source} -> you need {need:.1f} laps incl. the rest of this one, margin {margin:+.2f} laps"
                  + ("" if margin >= 0 else f" -> SHORT: save {(-margin / max(need, 0.1)) * 100:.1f}% per lap or stop"))
    # tyre wear rate over the current stint
    mode = wear_mode(laps)
    done = [L for L in stint if L["complete"] and L["wear_end"]]
    if done and laps[-1]["wear_end"]:
        first = stint[0]["wear_start"] or {}
        now = laps[-1]["wear_end"]
        n = len(done) + frac
        parts = []
        for w in range(4):
            if first.get(w) is None or now.get(w) is None or n <= 0:
                continue
            left = now[w] if mode == "remaining" else 1 - now[w]
            rate = abs(first[w] - now[w]) / n
            parts.append(f"{WHEELS[w]} {left * 100:.0f}% left, -{rate * 100:.2f}%/lap")
        print("tyres this stint (%.1f laps on them): " % n + "; ".join(parts))
    ahead = ctx.scoring().rain_ahead(p["session_time"])
    if ahead:
        chance, minutes, rain_now = ahead
        print(f"weather ahead (game forecast): up to {chance * 100:.0f}% rain chance before the end of the session, peaking in"
              f" about {minutes:.0f} min; raining now: {fmt((rain_now or 0) * 100, 0)}%  (a chance, not a promise: weigh it"
              " when the pit stop or the tyre choice is close)")
    window, _ = pit_window(ctx)
    print(f"pit window from data: {window or 'none reported'}, scheduled_pit_stops={p['scheduled_pit_stops']}, "
          f"in pit lane now: {bool(p['in_pit_lane'])}")


# --------------------------------------------------------------------------- spotter
def rel(player, other):
    """Other car relative to the player in metres: (+ahead, +right). Uses LMU heading convention:
    forward = (-sin h, -cos h) and right = (-cos h, sin h) in world x/z."""
    h = player["heading"]
    dx = other["position_x"] - player["position_x"]
    dz = other["position_z"] - player["position_z"]
    fwd = dx * -math.sin(h) + dz * -math.cos(h)
    right = dx * -math.cos(h) + dz * math.sin(h)
    return fwd, right


def standings(frame):
    """Approximate running order: more laps first, then whoever started the current lap earlier.
    Exact at the line; can be wrong mid-lap for cars that swapped since."""
    return sorted(frame.values(), key=lambda r: (-(r["lap"] or 0), r["lap_start_time"] or 0))


def positions(ctx, frame):
    """(vehicle ids in running order, {vehicle: place}, exact?). Official positions from the
    standings table when the database has them, otherwise approximated from telemetry."""
    sc = ctx.scoring()
    if sc.cars:
        ids = [r["vehicle_id"] for r in sc.order()]
        return ids, {v: i + 1 for i, v in enumerate(ids)}, True
    ids = [r["vehicle_id"] for r in standings(frame)]
    return ids, {v: i + 1 for i, v in enumerate(ids)}, False


def place_text(ctx, vid, place, exact):
    sc = ctx.scoring()
    p = place.get(vid)
    if p is None:
        return "P?"
    txt = f"P{p}" if exact else f"~P{p}"
    r = sc.cars.get(vid)
    if exact and r is not None and sc.multiclass():
        txt += f" (P{sc.class_position(r)} in {r['vehicle_class']})"
    return txt


def cmd_nearby(ctx, args):
    p = ctx.player
    sc = ctx.scoring()
    frame = ctx.frame()
    ids, place, exact = positions(ctx, frame)
    print(f"SPOTTER  player vehicle {ctx.vehicle}, lap {p['lap']}, speed {p['speed'] * 3.6:.0f} km/h, {place_text(ctx, ctx.vehicle, place, exact)}/{len(ids)}")
    print(f"on-track gaps: car ahead {fmt(p['gap_to_car_ahead'], 2)}s, car behind {fmt(p['gap_to_car_behind'], 2)}s | "
          f"standings gaps: place ahead {fmt(p['gap_to_place_ahead'], 2)}s, place behind {fmt(p['gap_to_place_behind'], 2)}s")
    near = []
    for vid, r in frame.items():
        if vid == ctx.vehicle or r["position_x"] is None:
            continue
        f, rt = rel(p, r)
        d = math.hypot(f, rt)
        if d <= args.radius:
            near.append((d, vid, f, rt, r))
    if not near:
        print(f"no cars within {args.radius:.0f} m")
    for d, vid, f, rt, r in sorted(near):
        side = "RIGHT" if rt > 0 else "LEFT"
        if abs(f) < 5.5 and 1.0 < abs(rt) < 6:
            where = f"ALONGSIDE {side} ({abs(rt):.1f} m across, {f:+.1f} m)"
        elif f > 0:
            where = f"ahead {f:.0f} m ({abs(rt):.1f} m {side.lower()})"
        else:
            where = f"behind {-f:.0f} m ({abs(rt):.1f} m {side.lower()})"
        laps_diff = (r["lap"] or 0) - (p["lap"] or 0)
        lapinfo = "same lap" if laps_diff == 0 else (f"{-laps_diff} lap(s) DOWN on you" if laps_diff < 0 else f"{laps_diff} lap(s) UP on you")
        closing = (p["speed"] or 0) - (r["speed"] or 0)
        st = sc.cars.get(vid)
        extra = ""
        if st is not None:
            if st["flag"] == "blue":
                extra += "; has BLUE flag"
            if st["pit_state"] in ("requested", "entering"):
                extra += f"; pit {st['pit_state']}"
        print(f"  {sc.label(vid)}: {where}; {lapinfo}; {place_text(ctx, vid, place, exact)}; speed {r['speed'] * 3.6:.0f} km/h "
              f"({'you are faster' if closing > 0 else 'they are faster'} by {abs(closing) * 3.6:.0f} km/h)"
              + ("; IN PIT LANE" if r["in_pit_lane"] else "") + extra)
    if not args.trend:
        return
    # standings neighbours trend over recent laps (uses the history cache)
    laps = lap_table(ctx, ctx.vehicle)
    recent = [L for L in laps if L["complete"]][-4:]
    if len(recent) >= 2:
        a = [L["gap_ahead_end"] for L in recent]
        b = [L["gap_behind_end"] for L in recent]
        print("standings gap trend at the line, last laps (ahead / behind): "
              + ", ".join(f"L{L['lap']} {fmt(x, 2)}/{fmt(y, 2)}" for L, x, y in zip(recent, a, b)))
        live_a, live_b = p["gap_to_place_ahead"], p["gap_to_place_behind"]
        print(f"now: {fmt(live_a, 2)} / {fmt(live_b, 2)}  (ahead shrinking = you are catching; behind shrinking = being caught)")
        if exact:
            me = place.get(ctx.vehicle)
            for label, pos in (("ahead", me - 1), ("behind", me + 1)):
                if pos and 1 <= pos <= len(ids):
                    st = sc.cars[ids[pos - 1]]
                    print(f"car {label}: {sc.label(st['vehicle_id'])}, last lap {fmt_lap(st['last_lap_time'])}, best {fmt_lap(st['best_lap_time'])}")


def cmd_field(ctx, args):
    sc = ctx.scoring()
    frame = ctx.frame()
    if sc.cars:
        print("FIELD (official standings; gaps are from each car's last scoring row, up to a sector old)")
        print(" P  cls  car  driver               class        laps  last lap   best lap   gap->next  pits pen  status")
        for r in sc.order():
            vid = r["vehicle_id"]
            tel = frame.get(vid)
            status = []
            if r["finish_status"] and r["finish_status"] != "none":
                status.append(r["finish_status"].upper())
            if r["in_garage"]:
                status.append("garage")
            elif r["pit_state"] and r["pit_state"] != "none":
                status.append(f"pit {r['pit_state']}")
            elif (tel is not None and tel["in_pit_lane"]) or r["in_pits"]:
                status.append("pit lane")
            if r["flag"] == "blue":
                status.append("blue flag")
            if r["laps_behind_leader"]:
                status.append(f"{r['laps_behind_leader']} lap(s) down")
            gap = r["time_behind_next"]
            gap_s = "-" if r["position"] == 1 else (f"{r['laps_behind_next']} lap" if r["laps_behind_next"] else fmt(gap, 1, "s"))
            tag = " <- YOU" if vid == ctx.vehicle else ""
            print(f"{r['position'] or '?':>2}  {sc.class_position(r) or '-':>3}  {vid:>3}  {(r['driver'] or '-')[:20]:<20} {(r['vehicle_class'] or '-')[:12]:<12} "
                  f"{r['laps_completed'] if r['laps_completed'] is not None else '-':>4}  {fmt_lap(r['last_lap_time']):>9}  {fmt_lap(r['best_lap_time']):>9}  "
                  f"{gap_s:>9}  {r['pit_stops'] if r['pit_stops'] is not None else '-':>4} {r['penalties'] or 0:>3}  {', '.join(status) or 'on track'}{tag}")
        return
    order = standings(frame)
    hist = ctx.history()
    per = {v: [dict(L, inv=L["invalid"]) for L in hist.laps(v)] for v in hist.vehicles()}
    print("FIELD (approx order by laps + time of last line crossing; this database has no standings table)")
    print(" P  car  lap  last lap   best lap   pits  status")
    for i, r in enumerate(order):
        laps = per.get(r["vehicle_id"], [])
        times = []
        for a, b in zip(laps, laps[1:]):
            if a["start"] is not None and b["start"] is not None:
                times.append((a["lap"], b["start"] - a["start"], a["inv"], a["pit"]))
        last = times[-1][1] if times else None
        clean = [t for _, t, inv, pit in times if not inv and not pit]
        best = min(clean) if clean else None
        pits = sum(1 for (a, b) in zip(laps, laps[1:]) if a["pit"] and not b["pit"]) + (1 if laps and laps[-1]["pit"] else 0)
        tag = " <- YOU" if r["vehicle_id"] == ctx.vehicle else ""
        status = "PIT LANE" if r["in_pit_lane"] else "on track"
        print(f"{i + 1:>2}  {r['vehicle_id']:>3}  {r['lap']:>3}  {fmt_lap(last):>9}  {fmt_lap(best):>9}  {pits:>4}  {status}{tag}")


def scoring_alerts(ctx):
    """(alerts, info) from the scoring tables (both empty without them). Alerts change what the
    pilot must do right now; info is context to use only when it bears on the question."""
    sc = ctx.scoring()
    alerts, info = [], []
    s, me = sc.session, sc.player
    if s is not None:
        if s["full_course_yellow"] and s["full_course_yellow"] != "none":
            alerts.append(f"FULL COURSE YELLOW ({s['full_course_yellow']})")
        if s["phase"] == "stopped":
            alerts.append("RED FLAG, session stopped")
        elif s["phase"] and s["phase"] not in ("green", "full_course_yellow"):
            info.append(f"session phase {s['phase']}")
        yellows = [str(i) for i in (1, 2, 3) if s[f"local_yellow_{i}"]]
        if yellows:
            info.append("local yellow somewhere on track (sector index " + ",".join(yellows) + " as the sim reports it)")
    if me is not None:
        if me["flag"] == "blue":
            alerts.append("BLUE FLAG shown to you")
        if me["under_yellow"]:
            alerts.append("YOU ARE UNDER YELLOW")
        if me["penalties"]:
            alerts.append(f"{me['penalties']} PENALTY outstanding")
        if me["pit_state"] == "requested":
            info.append("pit stop requested")
        if me["finish_status"] and me["finish_status"] != "none":
            alerts.append(me["finish_status"].upper())
    tl = sc.track_limits(ctx.player["track_limits_steps"])
    if tl and tl[0]:
        steps, per_pen, points_left = tl
        info.append(f"track limits: {steps} of {per_pen} steps, {points_left} more point(s) to a penalty")
    elif ctx.player["track_limits_steps"]:
        info.append(f"track-limit steps {ctx.player['track_limits_steps']} (penalty threshold unknown)")
    return alerts, info


# --------------------------------------------------------------------------- status
def cmd_status(ctx, args, path):
    p = ctx.player
    age = staleness(path)
    wh = ctx.wheels(p["id"])
    frame = ctx.frame()
    sc = ctx.scoring()
    ids, place, exact = positions(ctx, frame)
    print(f"STATUS player vehicle {ctx.vehicle}{' (' + sc.player['driver'] + ')' if sc.player is not None and sc.player['driver'] else ''}"
          f" | session_time {p['session_time']:.1f}s"
          + (f" | db last written {age:.0f}s ago" + (" (STALE: game paused or logger stopped?)" if age > 10 else "") if age is not None else ""))
    sl = sc.session_line(p["session_time"])
    if sl:
        print("session: " + sl)
    wl = sc.weather_line()
    if wl:
        print("weather: " + wl)
    fl = sc.forecast_line(p["session_time"])
    if fl:
        print("forecast: " + fl)
    lap_elapsed = p["session_time"] - p["lap_start_time"] if p["lap_start_time"] is not None else None
    print(f"lap {p['lap']} sector {(p['sector'] or 0) + 1} | lap time so far {fmt_lap(lap_elapsed)} | delta to best {fmt(p['delta_best'], 2, 's')}"
          f" | lap invalidated: {'YES' if p['lap_invalidated'] else 'no'} | track-limit steps {p['track_limits_steps']}")
    me = place.get(ctx.vehicle)
    nb = lambda q: sc.label(ids[q - 1]) if q and 1 <= q <= len(ids) else "-"
    print(f"position {place_text(ctx, ctx.vehicle, place, exact)}/{len(ids)} (ahead {nb(me - 1 if me else None)}, behind {nb(me + 1 if me else None)}) | standings gap ahead {fmt(p['gap_to_place_ahead'], 2)}s, behind {fmt(p['gap_to_place_behind'], 2)}s"
          f" | on-track car ahead {fmt(p['gap_to_car_ahead'], 2)}s, behind {fmt(p['gap_to_car_behind'], 2)}s")
    print(f"speed {p['speed'] * 3.6:.0f} km/h gear {p['gear']} | pit lane {'YES' if p['in_pit_lane'] else 'no'} | limiter {'ON' if p['speed_limiter_on'] else 'off'}")
    ve = p["virtual_energy"]
    ve_s = "-" if ve is None else (pct(ve) if ve <= 1.0 else f"{ve:.2f}")
    print(f"fuel {fmt(p['fuel'], 2, ' L')} | virtual energy {ve_s} | battery {pct(p['battery_charge'])}")
    laps = lap_table(ctx, ctx.vehicle)
    mode = wear_mode(laps)
    print("tyres        wear-left  surface L/C/R C   core C  carcass C  press kPa  brake C  flat")
    opt = None
    for w in range(4):
        r = wh.get(w)
        if r is None:
            continue
        wear = r["tire_wear"]
        left = None if wear is None else (wear if mode == "remaining" else 1 - wear)
        opt = r["tire_optimal_temperature"]
        print(f"  {WHEELS[w]}         {pct(left):>7}  {fmt(r['tire_surface_temperature_left'], 0)}/{fmt(r['tire_surface_temperature_center'], 0)}/{fmt(r['tire_surface_temperature_right'], 0)}"
              f"      {fmt(r['tire_inner_temperature_center'], 0):>4}  {fmt(r['tire_carcass_temperature'], 0):>9}  {fmt(r['tire_pressure'], 1):>9}  {fmt(r['brake_temperature'], 0):>7}  {'FLAT' if r['tire_flat'] else 'no'}")
    if opt:
        opt_c = opt - 273.15 if opt > 200 else opt
        print(f"  optimal tyre temp reported: {opt_c:.0f} C  (L/C/R = left/centre/right tread edge seen from behind the car; inside edge = R on left tyres, L on right tyres; core = inner rubber layer, not the inside edge)")
    print(f"engine water {fmt(p['engine_water_temperature'], 0, ' C')}, oil {fmt(p['engine_oil_temperature'], 0, ' C')}, overheating {'YES' if p['engine_overheating'] else 'no'}")
    dents = [p[f"dent_severity_{i}"] for i in range(8)]
    impact = p["last_impact_time"]
    dmg = "none" if not any(dents) and not p["parts_detached"] else f"dents {dents} (0 none,1 some,2 more), parts detached: {bool(p['parts_detached'])}"
    parts = damage_parts(p)
    if parts:
        dmg = parts if dmg == "none" else f"{parts}; {dmg}"
    print(f"damage: {dmg}" + (f" | last impact {p['session_time'] - impact:.0f}s ago, magnitude {p['last_impact_magnitude']:.0f}" if impact else ""))
    bias = p["rear_brake_bias"]
    print(f"settings: TC {p['traction_control']} (slip {p['traction_control_slip']}, cut {p['traction_control_cut']}), ABS {p['abs']}, engine map {p['engine_map']}, "
          f"brake bias {fmt((1 - bias) * 100 if bias is not None else None, 1, '% front')}, migration {p['brake_migration']}, ARB F{p['front_anti_roll_bar']}/R{p['rear_anti_roll_bar']}")
    done = [L for L in laps if L["complete"]]
    if done:
        last = done[-1]
        print(f"last lap L{last['lap']}: {fmt_lap(last['time'])}" + (" INVALID" if last["invalid"] else "") + (" PIT" if last["pit"] else ""))
    me = sc.player
    if me is not None:
        print(f"scoring: best lap {fmt_lap(me['best_lap_time'])}, last sectors {fmt(me['last_sector_1'], 2)}/{fmt(me['last_sector_2'], 2)}/{fmt(me['last_sector_3'], 2)}, "
              f"pit stops {me['pit_stops']}, penalties {me['penalties'] or 0}, flag {me['flag'] or '-'}")
    alerts, info = scoring_alerts(ctx)
    if alerts:
        print("ALERTS (act now) " + "; ".join(alerts))
    if info:
        print("INFO " + "; ".join(info))


# --------------------------------------------------------------------------- quick (urgent)
# --------------------------------------------------------------------------- track description
# The my_engineer app saves the track's description next to the database as track.json at the start of each
# session, from Lovely Sim Racing's lovely-track-data (CC BY-NC-SA 4.0). Positions in it are fractions of the lap
# from the start/finish line.
TRACK_SOURCE = "Lovely Sim Racing, lovely-track-data (github.com/Lovely-Sim-Racing/lovely-track-data), CC BY-NC-SA 4.0"
# Turn "scale": 1 (hairpin) to 6 (wide).
TIGHTNESS = {1: "hairpin", 2: "very tight", 3: "tight", 4: "medium", 5: "open", 6: "wide"}
PIT_LOSSES = {"pit": "pit stop", "dt": "drive-through", "sng10": "10 s stop-and-go"}


def load_track(ctx, sc):
    """The track description saved next to the database, if there is one for this session's track."""
    path = os.path.join(os.path.dirname(os.path.abspath(ctx.db_path)), "track.json")
    try:
        with open(path, encoding="utf-8") as f:
            track = json.load(f)
    except (OSError, ValueError):
        return None
    name = sc.session["track"] if sc.session else None
    if not isinstance(track, dict) or (track.get("source_track") and name and track["source_track"] != name):
        return None  # left from another track
    return track


def track_length(track, sc):
    """The lap length in metres: the game's, else the track data's."""
    game = sc.session["track_length"] if sc.session and sc.session["track_length"] else None
    return game or track.get("length")


def turn_text(turn):
    label = " ".join(x for x in (f"T{turn['number']}" if turn.get("number") is not None else None, turn.get("name")) if x)
    how = ["left" if turn.get("direction") == 0 else "right" if turn.get("direction") == 1 else None,
           TIGHTNESS.get(turn.get("scale"))]
    how = ", ".join(x for x in how if x)
    return f"{label} ({how})" if how else label


def on_part(fraction, part):
    """Whether a lap fraction is within a turn or straight (which may run across the line)."""
    start, end = part.get("start"), part.get("end")
    if start is None or end is None:
        return False
    return start <= fraction <= end if start <= end else fraction >= start or fraction <= end


def track_position(track, fraction, length):
    """Where a lap fraction is: (the turn or straight it is on, or None; [(text, metres ahead)] of the next turn
    and of the pit entry when it is within a kilometre)."""
    turns = [t for t in track.get("turn", []) if isinstance(t, dict) and t.get("start") is not None]
    where = next((turn_text(t) for t in turns if on_part(fraction, t)), None)
    if where is None:
        where = next((s.get("name") for s in track.get("straight", []) if isinstance(s, dict) and on_part(fraction, s)), None)
    ahead = []
    upcoming = [((t["start"] - fraction) % 1.0, t) for t in turns if not on_part(fraction, t)]
    if upcoming:
        gap, turn = min(upcoming, key=lambda x: x[0])
        ahead.append((turn_text(turn), gap * length))
    if track.get("pitentry") is not None:
        gap = (track["pitentry"] - fraction) % 1.0
        if gap * length <= 1000:
            ahead.append(("pit entry", gap * length))
    return where, ahead


def pit_class(vehicle_class):
    """The track data's class for a car class name (e.g. 'Hypercar' -> 'hyper'), or None."""
    c = (vehicle_class or "").lower()
    for key, names in (("hyper", ("hyper",)), ("lmp2", ("lmp2", "p2")), ("gte", ("gte",)), ("gt3", ("gt3",))):
        if any(n in c for n in names):
            return key
    return None


def track_line(ctx, sc, p):
    """Where the player is on the lap, from the track description: for quick."""
    track = load_track(ctx, sc)
    distance = p.get("lap_distance")
    if not track or distance is None:
        return None
    length = track_length(track, sc)
    if not length:
        return None
    where, ahead = track_position(track, (distance % length) / length, length)
    parts = [f"{distance / length * 100:.0f}% of the lap" + (f", {'in' if '(' in where else 'on'} {where}" if where else "")]
    parts += [f"{text} in {metres:.0f} m" for text, metres in ahead]
    return " | ".join(parts)


def cmd_track(ctx, args):
    """The circuit: turns, straights, sectors, pit lane and time lost in the pits, in metres from the line."""
    sc = ctx.scoring()
    track = load_track(ctx, sc)
    if not track:
        name = sc.session["track"] if sc.session else "this track"
        print(f"TRACK no description for {name} (the app downloads one at the start of each session when "
              "lovely-track-data has the track; RaceRoom has none)")
        return
    length = track_length(track, sc)
    data_length = track.get("length")
    head = f"TRACK {track.get('name', '?')}" + (f" ({track['country']})" if track.get("country") else "")
    if length:
        head += f", {length:.0f} m"
        if data_length and abs(data_length - length) / length > 0.03:
            head += f" (the track data says {data_length:.0f} m: its positions may be a little off)"
    print(head)

    def at(fraction):
        return f"{fraction * length:.0f} m" if length else f"{fraction * 100:.0f}%"

    turns = [t for t in track.get("turn", []) if isinstance(t, dict)]
    if turns:
        print("TURNS " + " | ".join(
            f"{turn_text(t)} {at(t['start'])}-{at(t['end'])}" + (f", apex {at(t['marker'])}" if t.get("marker") is not None else "")
            for t in turns if t.get("start") is not None and t.get("end") is not None))
    straights = [s for s in track.get("straight", []) if isinstance(s, dict) and s.get("start") is not None]
    if straights:
        print("STRAIGHTS " + " | ".join(f"{s.get('name', '?')} {at(s['start'])}-{at(s['end'])}" for s in straights))
    sectors = [s for s in track.get("sector", []) if isinstance(s, dict) and s.get("marker") is not None]
    if sectors:
        print("SECTORS end at " + " | ".join(f"{s.get('name', '?')} {at(s['marker'])}" for s in sectors))
    if track.get("pitentry") is not None or track.get("pitexit") is not None:
        print("PIT LANE" + (f" entry {at(track['pitentry'])}" if track.get("pitentry") is not None else "")
              + (f", exit {at(track['pitexit'])}" if track.get("pitexit") is not None else ""))
    times = track.get("time")
    if isinstance(times, list) and times and isinstance(times[0], dict):
        mine = pit_class(sc.player["vehicle_class"]) if sc.player else None
        classes = []
        for cls, entries in times[0].items():
            losses = ", ".join(f"{PIT_LOSSES.get(e.get('name'), e.get('name'))} {e.get('time')} s"
                               for e in entries if isinstance(e, dict))
            classes.append(f"{cls}{' (your class)' if cls == mine else ''}: {losses}")
        print("TIME LOST (estimates, s) " + " | ".join(classes))
    print("SOURCE " + TRACK_SOURCE)


# --------------------------------------------------------------------------- corners
# Per-corner analysis on telemetry lap_distance. Each turn is split at the reference lap's braking point and
# the apex: "entry" runs from where the reference lap starts braking to the apex (braking, turn-in), "exit"
# from the apex to the next turn's braking point (traction, and the straight it leads onto). Entry + exit of
# every turn covers the whole lap, so the per-turn times add up to the lap time.
BRAKE_ON = 0.10          # brake pedal fraction that counts as braking
DECEL_ON = -8.0          # m/s^2 that counts as braking when a car's pedal is not recorded (other cars)
BRAKE_SEARCH = 700.0     # metres before an apex searched for the braking point
THROTTLE_ON = 0.90


class Trace:
    """One lap of one car by distance: monotonic distance (m) -> session time, speed, pedals, gear."""

    def __init__(self, ctx, vehicle, lap, length):
        self.lap, self.length = lap["lap"], length
        rows = ctx.conn.execute(
            "SELECT session_time, lap_distance, speed, brake, throttle, gear FROM telemetry "
            "WHERE vehicle_id = ? AND id BETWEEN ? AND ? ORDER BY id", (vehicle, lap["first_id"], lap["last_id"])).fetchall()
        rows = [r for r in rows if r["lap_distance"] is not None and r["session_time"] is not None]
        n = len(rows)
        # The first rows of a lap can still carry the old lap's distance, and the last ones the new lap's.
        rows = [r for i, r in enumerate(rows)
                if not (i < n * 0.1 and r["lap_distance"] > length * 0.5) and not (i > n * 0.9 and r["lap_distance"] < length * 0.5)]
        self.d, self.t, self.v, self.brake, self.throttle, self.gear = [0.0], [lap["start"]], [None], [None], [None], [None]
        top = 0.0
        for r in rows:
            top = max(top, min(r["lap_distance"], length))  # LMU's estimate can step back a little: keep it monotonic
            if top <= self.d[-1]:
                continue
            self.d.append(top); self.t.append(r["session_time"]); self.v.append(r["speed"])
            self.brake.append(r["brake"]); self.throttle.append(r["throttle"]); self.gear.append(r["gear"])
        end = lap["start"] + lap["time"] if lap.get("time") else None
        if end is not None and self.d[-1] < length:
            self.d.append(length); self.t.append(end); self.v.append(None); self.brake.append(None)
            self.throttle.append(None); self.gear.append(None)
        self.v[0] = self.v[1] if len(self.v) > 1 else None
        self.v[-1] = self.v[-1] if self.v[-1] is not None else (self.v[-2] if len(self.v) > 1 else None)
        self.has_pedals = any(b is not None for b in self.brake[1:-1]) and any((b or 0) > 0 for b in self.brake[1:-1])
        self.ok = len(self.d) > 50 and self.d[-1] >= length * 0.98

    def _index(self, x):
        lo, hi = 0, len(self.d) - 1
        while lo < hi:
            mid = (lo + hi) // 2
            if self.d[mid] < x:
                lo = mid + 1
            else:
                hi = mid
        return lo

    def time_at(self, x):
        x = min(max(x, 0.0), self.length)
        i = self._index(x)
        if i == 0:
            return self.t[0]
        d0, d1 = self.d[i - 1], self.d[i]
        return self.t[i - 1] + (self.t[i] - self.t[i - 1]) * ((x - d0) / (d1 - d0) if d1 > d0 else 0)

    def span(self, a, b):
        """Seconds from distance a to b, going round the line if b < a."""
        if b >= a:
            return self.time_at(b) - self.time_at(a)
        return (self.time_at(self.length) - self.time_at(a)) + (self.time_at(b) - self.time_at(0))

    def speed_at(self, x):
        i = self._index(min(max(x, 0.0), self.length))
        return self.v[i] if self.v[i] is not None else self.v[max(0, i - 1)]

    def indices(self, a, b):
        """Sample indices from distance a to b (wrapping)."""
        if b >= a:
            return [i for i in range(self._index(a), self._index(b) + 1) if i < len(self.d)]
        return self.indices(a, self.length) + self.indices(0, b)

    def braking_point(self, apex, back=BRAKE_SEARCH):
        """Where braking for the corner at `apex` starts: the start of the last continuous braking run before it."""
        idx = self.indices((apex - back) % self.length, apex)
        on = None
        for k, i in enumerate(idx):
            if self.has_pedals:
                braking = (self.brake[i] or 0) >= BRAKE_ON
            else:
                j = idx[k - 1] if k else None
                braking = (j is not None and self.v[i] is not None and self.v[j] is not None and self.t[i] > self.t[j]
                           and (self.v[i] - self.v[j]) / (self.t[i] - self.t[j]) <= DECEL_ON)
            if braking and on is None:
                on = self.d[i]
            elif not braking and on is not None and (self.d[i] - on) > 0 and i != idx[-1]:
                # a short lift or dab well before the apex is not the braking point: keep looking
                if apex - self.d[i] > 60:
                    on = None
        return on

    def corner(self, apex, start, end):
        """Metrics of one corner: braking point, minimum speed and where, exit speed, throttle back."""
        bp = self.braking_point(apex)
        lo = bp if bp is not None else start
        idx = self.indices(lo, end)
        vs = [(self.v[i], self.d[i]) for i in idx if self.v[i] is not None]
        vmin, dmin = min(vs) if vs else (None, None)
        throttle_back = None
        if dmin is not None:
            for i in self.indices(dmin, (end + 300) % self.length):
                if (self.throttle[i] or 0) >= THROTTLE_ON:
                    throttle_back = self.d[i]
                    break
        gear = None
        if dmin is not None:
            gear = self.gear[self._index(dmin)]
        return dict(brake=bp, vmin=vmin, at_min=dmin, vexit=self.speed_at(end), throttle=throttle_back, gear=gear)


def signed(x, apex, length):
    """Metres from the apex to x, negative before it, taking the shorter way round the lap."""
    d = (x - apex) % length
    return d - length if d > length / 2 else d


def before(apex, x, length):
    """Metres from x to the apex ahead of it."""
    return None if x is None else (apex - x) % length


def lap_turns(ctx, sc, ref):
    """The turns to analyse: from track.json when the circuit is known, otherwise the braking zones of the
    reference lap. [(label, start, apex, end)] in metres, in lap order."""
    track = load_track(ctx, sc)
    length = ref.length
    if track:
        out = []
        for t in track.get("turn", []):
            if not isinstance(t, dict) or t.get("start") is None or t.get("end") is None:
                continue
            apex = t.get("marker") if t.get("marker") is not None else (t["start"] + t["end"]) / 2
            out.append((turn_text(t), t["start"] * length, apex * length, t["end"] * length))
        if out:
            return sorted(out, key=lambda x: x[2]), "track data"
    # no track data: every braking zone of the reference lap is a corner, its apex the slowest point after it
    zones, on = [], None
    for i in range(1, len(ref.d) - 1):
        braking = (ref.brake[i] or 0) >= BRAKE_ON if ref.has_pedals else (
            ref.v[i] is not None and ref.v[i - 1] is not None and ref.t[i] > ref.t[i - 1]
            and (ref.v[i] - ref.v[i - 1]) / (ref.t[i] - ref.t[i - 1]) <= DECEL_ON)
        if braking and on is None:
            on = ref.d[i]
        elif not braking and on is not None:
            if ref.d[i] - on > 15:
                zones.append(on)
            on = None
    out = []
    for k, b in enumerate(zones):
        nxt = zones[k + 1] if k + 1 < len(zones) else b + length
        idx = [i for i in ref.indices(b, min(nxt, length)) if ref.v[i] is not None]
        if not idx:
            continue
        i = min(idx, key=lambda i: ref.v[i])
        out.append((f"braking zone {len(out) + 1}", b, ref.d[i], min(ref.d[i] + 60, length)))
    return out, "braking zones of the reference lap (no track data)"


def pick_lap(laps, which, default):
    if which in (None, ""):
        return default
    if str(which).lower() == "best":
        ok = [L for L in laps if L["green"] and not L["invalid"] and L.get("time")]
        return min(ok, key=lambda L: L["time"]) if ok else None
    if str(which).lower() == "last":
        done = [L for L in laps if L["complete"]]
        return done[-1] if done else None
    return next((L for L in laps if L["lap"] == int(which)), None)


def kmh(v):
    return "-" if v is None else f"{v * 3.6:.0f}"


def cmd_corners(ctx, args):
    sc = ctx.scoring()
    me = ctx.vehicle
    laps = lap_table(ctx, me)
    length = (sc.session["track_length"] if sc.session and sc.session["track_length"] else None)
    if not length:
        dists = ctx.conn.execute("SELECT MAX(lap_distance) FROM telemetry WHERE vehicle_id = ? AND id >= ?",
                                 (me, ctx.session_start_id)).fetchone()[0]
        length = dists
    if not length or ctx.player.get("lap_distance") is None and "lap_distance" not in columns(ctx.conn, "telemetry"):
        die("this database has no lap_distance: corner analysis needs a newer my_engineer plugin")
    # reference: another driver's best lap, a given lap, or the player's best valid lap
    ref_vehicle = me
    if args.ref_of:
        ref_vehicle = resolve_vehicle(ctx, args.ref_of)
        ref_laps = lap_table(ctx, ref_vehicle)
        ref_lap = pick_lap(ref_laps, args.ref or "best", None)
    else:
        ref_lap = pick_lap(laps, args.ref or "best", None)
    if ref_lap is None or not ref_lap.get("time"):
        die("no complete reference lap yet (needs a clean, complete lap)")
    ref = Trace(ctx, ref_vehicle, ref_lap, length)
    if not ref.ok:
        die(f"the reference lap {ref_lap['lap']} has no usable lap_distance trace")
    turns, source = lap_turns(ctx, sc, ref)
    if not turns:
        die("no corners found")
    ref_name = (sc.label(ref_vehicle) + " " if ref_vehicle != me else "") + f"lap {ref_lap['lap']} ({fmt_lap(ref_lap['time'])})"
    # boundaries: the reference lap's braking point before each apex (or the turn's start when it does not brake)
    bounds = []
    for label, start, apex, end in turns:
        bp = ref.braking_point(apex)
        bounds.append(bp if bp is not None else start)

    def measure(tr):
        out = []
        for k, (label, start, apex, end) in enumerate(turns):
            nxt = bounds[(k + 1) % len(turns)]
            entry = tr.span(bounds[k], apex)
            exit_ = tr.span(apex, nxt)
            out.append(dict(label=label, entry=entry, exit=exit_, **tr.corner(apex, start, end)))
        return out

    ref_m = measure(ref)
    if args.turn is not None:
        return corner_detail(ctx, sc, args, laps, turns, bounds, ref_m, ref_name, length, source)

    lap = pick_lap(laps, args.lap or "last", None)
    if lap is None or not lap.get("time"):
        die("no complete lap to analyse yet")
    tr = Trace(ctx, me, lap, length)
    if not tr.ok:
        die(f"lap {lap['lap']} has no usable lap_distance trace")
    m = measure(tr)
    flags = "".join(f" {f}" for f, on in (("INVALID", lap["invalid"]), ("PIT", lap["pit"])) if on)
    print(f"CORNERS lap {lap['lap']} ({fmt_lap(lap['time'])}{flags}) vs {ref_name}: {lap['time'] - ref_lap['time']:+.3f}s | corners from {source}")
    print("turn                           entry   exit   total | brake (m before apex) lap/ref | min km/h lap/ref | exit km/h lap/ref | full throttle (m after apex) lap/ref")
    rows = []
    for a, b, (label, start, apex, end) in zip(m, ref_m, turns):
        de, dx = a["entry"] - b["entry"], a["exit"] - b["exit"]
        rows.append((de + dx, label, de, dx))
        bp = lambda c: fmt(before(apex, c["brake"], length), 0) if c["brake"] is not None else "none"
        thr = lambda c: fmt(signed(c["throttle"], apex, length), 0) if c["throttle"] is not None else "-"
        print(f"{label[:30]:<30} {de:+6.2f} {dx:+6.2f} {de + dx:+7.2f} | {bp(a):>5}/{bp(b):<5} | {kmh(a['vmin']):>4}/{kmh(b['vmin']):<4} | "
              f"{kmh(a['vexit']):>4}/{kmh(b['vexit']):<4} | {thr(a):>4}/{thr(b):<4}")
    worst = sorted(rows, reverse=True)[:3]
    losses = [f"{label}: {tot:+.2f}s ({'entry' if abs(de) >= abs(dx) else 'exit'})" for tot, label, de, dx in worst if tot > 0.02]
    print("BIGGEST LOSSES " + (" | ".join(losses) if losses else "none"))
    gains = sorted(rows)[:2]
    g = [f"{label}: {tot:+.2f}s" for tot, label, de, dx in gains if tot < -0.02]
    if g:
        print("GAINS " + " | ".join(g))
    print("(entry = from the reference's braking point to the apex: braking and turn-in; exit = apex to the next braking point:"
          " traction and the straight after. Positive = slower than the reference. Distances are the game's lap_distance"
          + (", turn positions from the track data are approximate (about 1% of the lap)" if source == "track data" else "") + ")")


def corner_detail(ctx, sc, args, laps, turns, bounds, ref_m, ref_name, length, source):
    """One turn on every complete lap of the session: braking point, speeds, time, and the best way through."""
    key = str(args.turn).strip().lower()
    k = None
    for i, (label, *_ ) in enumerate(turns):
        number = label.split()[0].lstrip("tT")
        if key.lstrip("t") == number or key in label.lower():
            k = i
            break
    if k is None:
        die("no such turn; turns are: " + "; ".join(t[0] for t in turns))
    label, start, apex, end = turns[k]
    nxt = bounds[(k + 1) % len(turns)]
    print(f"CORNER {label}: apex at {apex:.0f} m, reference {ref_name} | corners from {source}")
    print("lap   time     entry   exit  | brake (m before apex) | min km/h (m before apex) gear | exit km/h | full throttle (m after apex) | flags")
    results = []
    for L in laps:
        if not L["complete"] or not L.get("time"):
            continue
        tr = Trace(ctx, ctx.vehicle, L, length)
        if not tr.ok:
            continue
        c = tr.corner(apex, start, end)
        entry, exit_ = tr.span(bounds[k], apex), tr.span(apex, nxt)
        flags = ",".join(f for f, on in (("INVALID", L["invalid"]), ("PIT", L["pit"])) if on)
        results.append((L, c, entry, exit_, flags))
        bp = before(apex, c["brake"], length)
        mn = before(apex, c["at_min"], length) if c["at_min"] is not None else None
        mn_txt = "-" if mn is None else (f"{mn:.0f}" if mn < length / 2 else f"{(length - mn):.0f} after")
        print(f"{L['lap']:>3}  {fmt_lap(L['time']):>9}  {entry:6.2f} {exit_:6.2f} | {fmt(bp, 0):>6} | {kmh(c['vmin']):>4} ({mn_txt:>9}) {c['gear'] if c['gear'] is not None else '-':>2} |"
              f" {kmh(c['vexit']):>4} | {fmt(signed(c['throttle'], apex, length), 0) if c['throttle'] is not None else '-':>5} | {flags}")
    clean = [r for r in results if not r[4]]
    if clean:
        best = min(clean, key=lambda r: r[2] + r[3])
        last = results[-1]
        L, c, e, x, _ = best
        print(f"FASTEST THROUGH IT lap {L['lap']}: {e + x:.2f}s (entry {e:.2f}, exit {x:.2f}), braking {fmt(before(apex, c['brake'], length), 0)} m before the apex,"
              f" min {kmh(c['vmin'])} km/h, exit {kmh(c['vexit'])} km/h")
        Ll, cl, el, xl, _ = last
        if Ll is not L:
            bl, bb = before(apex, cl["brake"], length), before(apex, c["brake"], length)
            diff = None if bl is None or bb is None else bl - bb
            print(f"LAST LAP {Ll['lap']} vs that: {el + xl - (e + x):+.2f}s"
                  + (f", braked {abs(diff):.0f} m {'earlier' if diff > 0 else 'later'}" if diff is not None and abs(diff) >= 3 else ", same braking point")
                  + f", min speed {((cl['vmin'] or 0) - (c['vmin'] or 0)) * 3.6:+.0f} km/h, exit {((cl['vexit'] or 0) - (c['vexit'] or 0)) * 3.6:+.0f} km/h")
    print("(braking point = where the brake first goes past 10% and stays on; distances from the game's lap_distance."
          " Advice from this is relative to the pilot's own laps, not a theoretical optimum.)")


def cmd_quick(ctx, args, path):
    """Everything urgent from the latest logger frame only: a handful of indexed lookups, no history."""
    p = ctx.player
    frame = ctx.frame()
    sc = ctx.scoring()
    ids, place, exact = positions(ctx, frame)
    pos = place.get(ctx.vehicle)
    ahead = ids[pos - 2] if pos and pos > 1 else None
    behind = ids[pos] if pos and pos < len(ids) else None
    age = staleness(path)
    print(f"QUICK lap {p['lap']} S{(p['sector'] or 0) + 1} | delta {fmt(p['delta_best'], 2, 's')}"
          f"{' | LAP INVALID' if p['lap_invalidated'] else ''} | {place_text(ctx, ctx.vehicle, place, exact)}/{len(ids)}"
          f" | gap ahead {fmt(p['gap_to_place_ahead'], 1)}s ({sc.label(ahead) if ahead is not None else 'nobody'})"
          f" behind {fmt(p['gap_to_place_behind'], 1)}s ({sc.label(behind) if behind is not None else 'nobody'})"
          + (f" | data {age:.0f}s old{' STALE' if age > 10 else ''}" if age is not None else ""))
    sl = sc.session_line(p["session_time"])
    if sl:
        print("SESSION " + sl)
    wh = ctx.wheels(p["id"])
    wl = sc.weather_line()
    wet = [WHEELS[w] for w in range(4) if w in wh and wh[w]["surface_type"] == 1]
    if wl or wet:
        print("WEATHER " + (wl or "-") + (f" | on a wet patch now: {' '.join(wet)}" if wet else ""))
    fl = sc.forecast_line(p["session_time"])
    if fl:
        print("FORECAST " + fl)
    tl = track_line(ctx, sc, p)
    if tl:
        print("TRACK " + tl)
    left, right, others = [], [], []
    for vid, r in frame.items():
        if vid == ctx.vehicle or r["position_x"] is None:
            continue
        f, rt = rel(p, r)
        if math.hypot(f, rt) > 60:
            continue
        laps_diff = (r["lap"] or 0) - (p["lap"] or 0)
        tag = "" if laps_diff == 0 else (f", {-laps_diff} lap down" if laps_diff < 0 else f", {laps_diff} lap up")
        st = sc.cars.get(vid)
        if st is not None and st["flag"] == "blue":
            tag += ", has blue flag"
        if abs(f) < 5.5 and 1.0 < abs(rt) < 6:
            (right if rt > 0 else left).append(f"{sc.label(vid)} ALONGSIDE{tag}")
        else:
            others.append((abs(f), f"{sc.label(vid)} {'ahead' if f > 0 else 'behind'} {abs(f):.0f} m{tag}"))
    print(f"TRAFFIC left: {', '.join(left) or 'clear'} | right: {', '.join(right) or 'clear'}"
          + (" | " + "; ".join(t for _, t in sorted(others)[:3]) if others else " | nobody within 60 m"))
    ve = p["virtual_energy"]
    print(f"ENERGY VE {'-' if ve is None else (pct(ve) if ve <= 1.0 else f'{ve:.2f}')} | fuel {fmt(p['fuel'], 1, ' L')} | battery {pct(p['battery_charge'])}")
    opt = next((r["tire_optimal_temperature"] for r in wh.values() if r["tire_optimal_temperature"]), None)
    opt_c = None if opt is None else (opt - 273.15 if opt > 200 else opt)
    temps = " ".join(f"{WHEELS[w]} {fmt(wh[w]['tire_surface_temperature_center'], 0)}" for w in range(4) if w in wh)
    press = "/".join(fmt(wh[w]["tire_pressure"], 0) for w in range(4) if w in wh)
    print(f"TYRES centre temp C: {temps}" + (f" (optimal {opt_c:.0f})" if opt_c else "") + f" | pressure kPa FL/FR/RL/RR {press}")
    alerts = []
    if p["in_pit_lane"]:
        # Some sims only report the limiter while the pilot drives: unknown is not "off".
        limiter = {None: "", 0: ", LIMITER OFF"}.get(p["speed_limiter_on"], ", limiter ON")
        alerts.append("in pit lane" + limiter)
    if any(r["tire_flat"] for r in wh.values()):
        alerts.append("FLAT " + ",".join(WHEELS[w] for w, r in wh.items() if r["tire_flat"]))
    if any(r["wheel_detached"] for r in wh.values()) or p["parts_detached"]:
        alerts.append("PARTS DETACHED")
    if p["engine_overheating"]:
        alerts.append(f"ENGINE OVERHEATING (water {fmt(p['engine_water_temperature'], 0)} C)")
    if p["last_impact_time"] and p["session_time"] - p["last_impact_time"] < 30:
        alerts.append(f"IMPACT {p['session_time'] - p['last_impact_time']:.0f}s ago, magnitude {p['last_impact_magnitude']:.0f}")
    more_alerts, info = scoring_alerts(ctx)
    alerts += more_alerts
    dents = [p[f"dent_severity_{i}"] or 0 for i in range(8)]
    if any(dents):
        info.insert(0, f"dents {dents}")
    parts = damage_parts(p)
    if parts:
        info.insert(0, "damage " + parts)
    window, closing = pit_window(ctx)
    if closing:
        alerts.append("PIT WINDOW CLOSING, mandatory stop not done (" + window + ")")
    elif window:
        info.append(window)
    print("ALERTS (act now) " + ("; ".join(alerts) if alerts else "none"))
    print("INFO (only if it bears on the question) " + ("; ".join(info) if info else "none"))
    sys.stdout.flush()
    warm_in_background(ctx, args)


def cmd_warm(ctx, args):
    t = time.time()
    h = ctx.history()
    laps = h.laps(ctx.vehicle)
    print(f"ready: player vehicle {ctx.vehicle}, {len(h.vehicles())} cars, current session from id {h.session_start_id}, "
          f"player on lap {ctx.player['lap']} ({len(laps)} laps cached); scanned {h.scanned} new rows in {time.time() - t:.2f}s")


# --------------------------------------------------------------------------- sql
def cmd_sql(ctx, args):
    q = args.query.strip().rstrip(";")
    if not q.lower().startswith(("select", "with", "pragma")):
        die("only SELECT / WITH / PRAGMA queries are allowed")
    print(f"-- player vehicle_id={ctx.vehicle}, current session telemetry rows have id >= {ctx.session_start_id}, latest player row id={ctx.last_id}")
    if state_columns(ctx.conn):
        print("-- aids, settings, damage, pit stops and track-limit steps are in car_state, written only on change: a car's "
              "value at telemetry row X is its latest car_state row with telemetry_id <= X (see references/schema.md)")
    sc = ctx.scoring()
    if sc.available and sc.cars:
        first = min(r["id"] for r in sc.cars.values())
        print(f"-- scoring: session/standings tables present; newest standings row per car found back to id {first} "
              "(rows are written on change only: take MAX(id) per vehicle_id)")
    cur = ctx.conn.execute(q)
    names = [d[0] for d in cur.description] if cur.description else []
    print(" | ".join(names))
    n = 0
    for r in cur:
        print(" | ".join("NULL" if v is None else (f"{v:.4g}" if isinstance(v, float) else str(v)) for v in r))
        n += 1
        if n >= args.limit:
            print(f"... (stopped at {args.limit} rows)")
            break


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", help="path to the telemetry .db file (default: the one the LMU plugin is writing)")
    ap.add_argument("--vehicle", type=int, help="vehicle_id to treat as the player (default: is_player = 1)")
    # --db and --vehicle are accepted after the subcommand too ("quick --db X" as well as "--db X quick")
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--db", default=argparse.SUPPRESS, help=argparse.SUPPRESS)
    common.add_argument("--vehicle", type=int, default=argparse.SUPPRESS, help=argparse.SUPPRESS)
    sub = ap.add_subparsers(dest="cmd", required=True)
    _add = sub.add_parser
    sub.add_parser = lambda *a, **k: _add(*a, parents=[common], **k)
    sub.add_parser("quick")
    sub.add_parser("warm")
    sub.add_parser("status")
    s = sub.add_parser("laps"); s.add_argument("--last", type=int, default=0); s.add_argument("--of", dest="of_vehicle", help="vehicle_id or part of a driver's name")
    s = sub.add_parser("stint"); s.add_argument("--race-laps-left", type=float); s.add_argument("--race-time-left", type=float)
    s = sub.add_parser("nearby"); s.add_argument("--radius", type=float, default=120.0); s.add_argument("--trend", action="store_true")
    sub.add_parser("field")
    sub.add_parser("track")
    s = sub.add_parser("corners"); s.add_argument("--lap", help="lap to analyse (default: the last complete lap)")
    s.add_argument("--ref", help="reference lap number or 'best' (default: the best valid lap)")
    s.add_argument("--ref-of", dest="ref_of", help="compare with another driver's best lap (name or vehicle id)")
    s.add_argument("--turn", help="one turn on every lap of the session: number (5, T5) or part of its name")
    s = sub.add_parser("sql"); s.add_argument("query"); s.add_argument("--limit", type=int, default=60)
    args = ap.parse_args()
    if not args.db:
        args.db = find_database()
    conn = connect(args.db)
    try:
        ctx = Ctx(conn, args.db, args.vehicle)
        if args.cmd == "quick":
            cmd_quick(ctx, args, args.db)
        elif args.cmd == "warm":
            cmd_warm(ctx, args)
        elif args.cmd == "laps":
            cmd_laps(ctx, args)
        elif args.cmd == "status":
            cmd_status(ctx, args, args.db)
        elif args.cmd == "stint":
            cmd_stint(ctx, args)
        elif args.cmd == "nearby":
            cmd_nearby(ctx, args)
        elif args.cmd == "field":
            cmd_field(ctx, args)
        elif args.cmd == "track":
            cmd_track(ctx, args)
        elif args.cmd == "corners":
            cmd_corners(ctx, args)
        elif args.cmd == "sql":
            cmd_sql(ctx, args)
    except sqlite3.OperationalError as e:
        die(f"sqlite: {e} (if 'database is locked', the logger is mid-write - just retry)")


if __name__ == "__main__":
    main()
