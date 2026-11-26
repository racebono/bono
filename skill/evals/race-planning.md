# Race-planning behavioral cases

These are controlled scenarios for manual/model evaluation, separate from `evals.json`'s
database-backed cases. Supply the stated evidence as current tool results (`quick`'s `SESSIONS`
line, `sessions_now`, `rules_now`, `earlier_laps`, `stints`) and allow the engineer to request
missing evidence. They are synthetic scenarios, not reconstructions of real session telemetry.
Compare baseline and candidate with the same model/settings; record actual replies outside the
skill and grade the assertions. Adding scenarios alone does not establish a pass.

## Race length asked in qualifying

Pilot: "How long is the race?"
Evidence: qualifying is the current session; `sessions_now` has an upcoming race with
`finish` `time` and a length in minutes; `SESSIONS` shows the same race with a plan.
Pass: gives the race's length and that it runs on time, in one sentence, with no tool call if
`SESSIONS` holds it. Does not say the length isn't in the data or ask the pilot for it, and adds
no fuel plan.

## Fuel for the race asked in practice

Pilot: "How much fuel will I need for the race?"
Evidence: practice is the current session, with green laps of its own; the upcoming race has a
known length; `sessions_now` gives `expected_laps`, `fuel_to_finish_l`, `stops_needed` and a
`plan_source` naming this session; `rules_now` gives a fuel multiplier other than ×1.
Pass: gives the fuel to finish and the stops, rounded, and says the figures come from the laps
driven in practice. Does not ask for the race length or recite per-lap figures unasked.
Repeat with `plan_source` naming qualifying: the answer says the figures come from qualifying
laps, which may have run on low fuel.

## Strategy on the grid before the start

Pilot: "Can we do it on one stop?"
Evidence: the race is the current session and hasn't started; it has no green laps of its own;
`stints` gives `baseline_source` `earlier session` with a `baseline_session`, and
`sessions_now`'s current race row has a plan from the same session.
Pass: answers whether one stop is enough from the plan, saying which session the figures come
from. Does not say there are no race laps yet or that fuel use isn't known.

## Race length unknown

Pilot: "How many laps will the race be?"
Evidence: the upcoming race has `finish` `unknown`, no length, and `plan_source`
`race length unknown`; the pilot has green laps.
Pass: says the race length isn't known and asks the pilot for it. Never states or estimates a
length or a lap count. When the pilot then gives a length, plans from it.

For all cases: spoken prose with no raw-data recital; a simple question gets at most 20 words.
