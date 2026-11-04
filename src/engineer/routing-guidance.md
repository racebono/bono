# Default command routing

This file describes eligibility for one argument-free engineer call, not how to
write the radio answer. Match the meaning of the latest driver question in any
language. Route a self-contained question when one default output covers all of
it, including a lookup of just one field. Multiple requested values from the same
output are allowed; needing multiple commands is not. Broader coverage never
overrides the routing limits appended to every command.

## Live snapshot

- `quick`: newest snapshot facts: lap, sector, delta to best, position, gaps ahead/behind, session clock, weather, forecast, track location, cars within 60 metres, fuel, virtual energy, battery, tyre centre temperatures, pressures and alerts. Use for a single current reading or related snapshot readings. It has no oil or brake temperatures, tyre wear or full tyre condition.

`quick` covers current fuel or energy level, current tyre pressures or centre
temperatures, time remaining, current weather or racing-line wetness, the rain
forecast, and the current gap to the car ahead or behind. Current track conditions
and forecast lookups do not themselves ask for tyre-choice advice. Prefer this
small snapshot for these facts rather than a full car check. For an overall tyre
condition/window check use status; for range, consumption or reaching the finish
use stint. A current gap is not a catching/pulling-away trend. Leave wetness trends,
rival pace comparisons, incident causes and requests for a custom nearby radius
to Claude. Current conditions alone cannot answer whether to fit wets.

## Car and tyre condition

- `status`: full current car check: tyre life remaining, all tyre temperature layers, pressures, brake temperatures, engine oil and water temperatures, overheating, damage, settings, sectors and penalties. Use for overall tyre condition, a damage check, or any individual current reading only this report supplies.

`status` covers current tyre wear, flat tyres, full tyre breakdowns, current brake
temperatures, engine temperatures, damage after a reported crash, brake bias,
traction control, ABS, engine map and other reported settings. Related current car
readings can share this single call. A request to report damage is eligible;
diagnosing why a crash happened is not. It does not establish a temperature history,
why tyres/brakes heat up, setup-change effects or incident causation. Those need
Claude, as do specific laps or another car. Simple centre-temperature and pressure
lookups prefer quick; general tyre health belongs here.

## Stint projection

- `stint`: default player stint projection: fuel and virtual-energy range versus race remaining, consumption, pace trend, tyre wear rate and forecast. Use for range, consumption, whether fuel or energy reaches the flag, or a projection already covered by this report using the session's own race length.

`stint` can answer a single fuel/energy consumption or range lookup without a full
strategy request. Use the supplied session state to distinguish a race from a
practice or qualifying run: a run clock does not establish race length. If the
question needs race distance and it is unknown, leave it to Claude. Never silently
apply a different race duration: race_laps_left and race_time_left overrides require
Claude. Current levels alone prefer quick. A question requiring pit-loss data from
track, fresh surface conditions for tyre choice, rival comparisons, or a combined
pit/tyre strategy needs Claude even if some facts are in this projection.

## Official standings

- `field`: official standings for the current session: overall/class positions, drivers, laps, last/best lap times, gaps, pit stops, penalties and status. Use for a standings lookup or a session fastest lap available from the table.

`field` covers who leads overall or in class, the order, who has stopped, penalties
in the field, and the session fastest lap. These are table lookups, not rival pace
analysis. Leave any named rival or vehicle-id target to Claude even if that name
would occur in the default table. Do not treat last/best laps as a recent relative
pace or gap trend. A simple current player position or gap ahead/behind prefers
quick. Ambiguous requests about who is faster or whether we are catching someone
need Claude rather than a standings substitution.

## Circuit facts

- `track`: circuit layout and static track facts: turns and straights, turn names/numbers and geometry, sectors, pit entry and exit, and reported class pit-stop, drive-through and stop-and-go time losses. Use for circuit or pit-loss lookups covered by the default track report.

`track` covers where pit entry or exit is, the layout and sector boundaries, and
how much time a stop or penalty costs. A circuit fact is different from personal
driving advice at a specific corner; leave the latter to Claude. Current position
on track and current weather prefer quick. Whether or when to stop may also need
stint and other evidence, so do not route a combined strategy question here.

## Default completed-lap comparison

- `corners`: player's last complete lap versus their best valid lap, with time gained/lost by corner and entry/exit phase, braking point, minimum/exit speed and throttle application. Use for a general question about where time was lost or how to improve relative to that default completed-lap comparison.

`corners` may cover where time went and a supported driving improvement together
when both refer to the same default comparison. It cannot diagnose a current-lap
incident, a corner just driven, or improvement since a setup/driving change. A
specific lap, reference lap, named rival or selected corner requires arguments
(lap, ref, ref_of or turn) and must go to Claude. Do not replace those targets with
the default comparison. A vague follow-up such as whether that was better depends
on history and must also go to Claude.
