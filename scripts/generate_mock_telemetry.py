"""Generate synthetic issue-37 race/practice recordings from Bono's C schema.

Usage: python scripts/generate_mock_telemetry.py OUTPUT_DIRECTORY
Only creates new databases. These are controlled fixtures, not captured simulator data.
"""
import argparse
import json
from pathlib import Path
import re
import sqlite3

ROOT = Path(__file__).resolve().parents[1]
LENGTH = 5000.0
APEXES = [600.0, 1500.0, 2500.0, 3400.0, 4400.0]
NAMES = ['Prima', 'Curva del Lago', 'Esse', 'Bosco', 'Ultima']


def columns():
    result = {}
    for path in (ROOT / 'include/bono').glob('*.h'):
        text = re.sub(r'/\*.*?\*/', '', path.read_text(), flags=re.S)
        lines = iter(text.splitlines())
        for line in lines:
            match = re.match(r'#define SIM_(\w+)_COLUMNS\(X\)', line)
            if not match:
                continue
            body = line
            while line.rstrip().endswith('\\'):
                line = next(lines)
                body += '\n' + line
            result[match[1]] = re.findall(r'X\((f64|i64|boolean|string),\s*(\w+)(?:,\s*\w+)?\)', body)
    assert len(result) == 6, result.keys()
    return result


def generate(output):
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    path = output / 'telemetry.db'
    if path.exists():
        raise FileExistsError(path)
    conn = sqlite3.connect(path)
    mapping = {'telemetry': 'TELEMETRY', 'telemetry_wheel': 'WHEEL_TELEMETRY',
               'car_state': 'CAR_STATE', 'session': 'SESSION', 'standings': 'STANDING', 'forecast': 'FORECAST'}
    schema = columns()
    types = {'f64': 'REAL', 'i64': 'INTEGER', 'boolean': 'INTEGER', 'string': 'TEXT'}
    fields = {}
    for table, macro in mapping.items():
        leading = 'telemetry_id INTEGER NOT NULL REFERENCES telemetry(id), wheel INTEGER NOT NULL' if table == 'telemetry_wheel' else 'id INTEGER PRIMARY KEY'
        if table == 'car_state':
            leading += ', telemetry_id INTEGER NOT NULL REFERENCES telemetry(id), vehicle_id INTEGER, session_time REAL'
        definitions = ', '.join(name + ' ' + types[ty] for ty, name in schema[macro])
        ending = ', PRIMARY KEY(telemetry_id,wheel)' if table == 'telemetry_wheel' else ''
        conn.execute(f'CREATE TABLE {table} ({leading}, {definitions}{ending})')
        fields[table] = {name for _, name in schema[macro]} | {'id', 'telemetry_id', 'wheel'}
        if table == 'car_state':
            fields[table] |= {'vehicle_id', 'session_time'}
    conn.execute('CREATE INDEX car_state_by_vehicle ON car_state(vehicle_id,telemetry_id)')

    def insert(table, values):
        assert set(values) <= fields[table], (table, set(values) - fields[table])
        names = ','.join(values)
        return conn.execute(f'INSERT INTO {table} ({names}) VALUES ({",".join("?" for _ in values)})', tuple(values.values())).lastrowid

    traces = []
    lap_durations = {}
    for vehicle in [1, 2, 3, 4]:
        t = 0.0
        durations = []
        for lap in range(11):
            start = t
            for distance in range(0, int(LENGTH), 10):
                # Last complete lap brakes 35 m early for T2; lap 7 loses exit speed at T3.
                early = vehicle == 1 and lap in (8, 9) and 1300 <= distance < 1500
                speed = 80.0
                braking = False
                for index, apex in enumerate(APEXES):
                    before = apex - distance
                    zone = 155.0 if early and index == 1 else 120.0
                    if 0 <= before <= zone:
                        speed = min(speed, 35 + 45 * before / zone)
                        braking = True
                    elif -100 <= before < 0:
                        speed = min(speed, 35 - 45 * before / 100)
                    if vehicle == 1 and lap == 7 and index == 2 and -160 <= before <= 0:
                        speed = min(speed, 24 + abs(before) * 0.27)
                if vehicle == 3:
                    speed *= 0.99
                traces.append(dict(vehicle_id=vehicle, is_player=int(vehicle == 1), session_time=t,
                    lap=lap, lap_start_time=start, lap_distance=float(distance), sector=min(2, distance // 1667),
                    speed=speed, heading=0.0, position_x=0.0, position_y=0.0, position_z=-float(distance),
                    brake=float(braking), throttle=0.0 if braking else 1.0, steering=0.1 if speed < 60 else 0.0,
                    fuel=90 - (lap + distance / LENGTH) * 3.0,
                    virtual_energy=0.84 - (lap + distance / LENGTH) * 0.04,
                    in_pit_lane=0, lap_invalidated=0, engine_overheating=0, gear=4, engine_rpm=6500.0))
                t += 10 / speed
            durations.append(t - start)
        lap_durations[vehicle] = durations
    # Ten completed laps, then ten seconds of the next one; one shared latest frame.
    now = sum(lap_durations[1][:10]) + 10.0
    traces = [row for row in traces if row['session_time'] < now]
    for vehicle in [2, 3, 4, 1]:
        latest = max((r for r in traces if r['vehicle_id'] == vehicle), key=lambda r: r['session_time']).copy()
        latest.update(session_time=now, lap=9 if vehicle == 4 else 10,
            lap_distance=800.0 if vehicle == 2 else 760.0 if vehicle == 3 else 797.0 if vehicle == 4 else 800.0,
            position_x=-3.0 if vehicle == 4 else 0.0,
            position_z=-803.0 if vehicle == 2 else -760.0 if vehicle == 3 else -797.0 if vehicle == 4 else -800.0,
            heading=0.0, speed=70.0, lap_start_time=now - 10.0)
        traces.append(latest)
    traces.sort(key=lambda r: (r['session_time'], 1 if r['vehicle_id'] == 1 else 0, r['vehicle_id']))
    for row in traces:
        row_id = insert('telemetry', row)
        progress = row['lap'] + row['lap_distance'] / LENGTH
        for wheel in range(4):
            worn = progress * (0.035 if wheel == 0 else 0.009)
            temp = 110.0 if wheel == 0 else [0, 91, 89, 90][wheel]
            insert('telemetry_wheel', dict(telemetry_id=row_id, wheel=wheel,
                tire_wear=max(0.0, 1.0-worn), tire_pressure=178.0 if wheel == 0 else 170.0,
                tire_surface_temperature_center=temp, tire_surface_temperature_left=temp+1,
                tire_surface_temperature_right=temp-1, tire_optimal_temperature=90.0,
                tire_flat=0, wheel_detached=0))
        if row['vehicle_id'] == 1 and (row['lap_distance'] == 0 or row['session_time'] == now):
            insert('car_state', dict(telemetry_id=row_id, vehicle_id=1, session_time=row['session_time'],
                track_limits_steps=3, last_impact_time=sum(lap_durations[1][:8]) if row['lap'] >= 8 else 0.0,
                last_impact_magnitude=100.0 if row['lap'] >= 8 else 0.0,
                virtual_energy_per_lap=0.04, fuel_per_lap=3.0, fuel_capacity=90.0,
                speed_limiter_on=0))
    for t in [0.0, now]:
        insert('session', dict(session_time=t, time_remaining=now+900-t, track='Synthetic Lago Ring',
            track_length=LENGTH, session_type='race', phase='green', session_end_time=now+900,
            vehicle_count=4, ambient_temperature=21.0, track_temperature=30.0,
            rain=0.0, track_wetness_min=0.0, track_wetness_max=0.0, track_wetness_average=0.0,
            track_limits_steps_per_penalty=4, track_limits_steps_per_point=1))
        for vehicle, driver, position in [(1, 'Ana Lopes', 2), (2, 'Lena Fischer', 1), (3, 'Sofia Ruiz', 3), (4, 'Tom Hughes', 4)]:
            laps = (9 if vehicle == 4 else 10) if t else 0
            duration = lap_durations[vehicle][9] if t else None
            insert('standings', dict(session_time=t, vehicle_id=vehicle, is_player=int(vehicle == 1),
                driver=driver, vehicle='Mock Hypercar' if vehicle != 4 else 'Mock GT3',
                vehicle_class='Hypercar' if vehicle != 4 else 'GT3', position=position, laps_completed=laps,
                lap_start_time=now-10 if t else 0.0, lap_distance=800.0,
                last_lap_time=duration, best_lap_time=min(lap_durations[vehicle][:10]) if t else None,
                laps_behind_leader=1 if vehicle == 4 else 0, pit_stops=0, penalties=0,
                in_pits=0, flag='blue' if vehicle == 4 else 'green', finish_status='none'))
    for session_type, chances in [('race', [0.0, 0.01, 0.03, 0.25, 0.70]), ('practice', [0.0, 0.01, 0.01, 0.02, 0.03])]:
        for fraction, chance in zip([0, .25, .5, .75, 1], chances):
            insert('forecast', dict(session_time=now, session_type=session_type, session_fraction=fraction,
                rain_chance=chance, temperature=21.0, sky=4 if chance > .1 else 0,
                sky_text='Synthetic forecast', humidity=.6, wind_speed=2.0))
    conn.commit()
    assert conn.execute('PRAGMA integrity_check').fetchone()[0] == 'ok'
    assert not conn.execute('PRAGMA foreign_key_check').fetchall()
    practice = output.parent / 'practice'
    practice.mkdir(exist_ok=True)
    practice_path = practice / 'telemetry.db'
    if practice_path.exists():
        raise FileExistsError(practice_path)
    with sqlite3.connect(practice_path) as practice_conn:
        conn.backup(practice_conn)
        practice_conn.execute("UPDATE session SET session_type='practice'")
    track = dict(name='Synthetic Lago Ring', source_track='Synthetic Lago Ring', length=LENGTH,
        pitentry=.95, turn=[dict(number=i+1, name=name, start=(apex-140)/LENGTH,
        marker=apex/LENGTH, end=(apex+130)/LENGTH) for i, (apex, name) in enumerate(zip(APEXES, NAMES))])
    for directory in [output, practice]:
        (directory / 'track.json').write_text(json.dumps(track, indent=2))
    summary = dict(synthetic=True, telemetry_rows=len(traces), session_time=now,
        race_time_remaining=900, database_bytes=path.stat().st_size,
        caveat='Controlled synthetic fixture; not the original eval recording. Review assertions against actual tool output.')
    (output.parent / 'fixture.json').write_text(json.dumps(summary, indent=2))
    conn.close()
    print(json.dumps(summary))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output_directory')
    generate(parser.parse_args().output_directory)
