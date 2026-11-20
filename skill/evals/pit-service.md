# Pit service

Controlled manual/model scenarios, not executed results. They cover changing the car's next stop
with the `pit` tool: what the pilot asks for directly, proposals of the engineer's own, saying
which stop a change counts for, standing orders and stop requests. They are evals 48 to 59 in
`evals.json`, run against the synthetic race fixture.

They need a harness that registers the `pit` tool with a fixture menu and records its calls in
the transcript. Each case's `pit` field gives the menu:

- `game`: the game whose behaviour the fixture follows (`lmu` or `iracing`), for which stop a
  change counts for at each `position`.
- `menu`: what the message's `PIT MENU NOW` and `PIT OPTIONS` lines carry, worded as the app words
  them. The `pit` tool's `read` returns the same lines.
- `position`: where the car is when the change is made, for what `change` returns as the stop it
  counts for (`this stop`, `the next stop`, `not kept`, `unknown`).
- `takes`: whether the fixture game takes a change; when it doesn't, `change` returns the fields
  as `Not taken`.

A case with `history` has earlier turns of the same conversation, sent before its question as
the app would have kept them.

Cases with `scheduled` are sent as the app's scheduled-turn message, as in `scheduled.md`. A
harness without the `pit` tool says nothing about these behaviours: leave these ids out of
`EVAL_IDS` there.

## The pilot asks

| Pilot question | Pass criteria |
|---|---|
| Put wets on all four | One `change` setting all four corners to the new wet tyre the menu offers; no proposal or question first. One sentence saying wets on all four and for this stop. |
| Fronts only, mediums | One `change` setting FL and FR only; the rears untouched. |
| Add fuel for ten more laps, in a car that sets the level it leaves with | A `change` of the level field (`energy_leave_pct` or `fuel_leave_l`), worked out from the level now and the use per lap, not `fuel_add_l` and not ten itself. |
| Change the tear-off, in a car without one | No `change` claimed as done: says plainly it can't be set for this car. |
| Wets on, made while the car is stopped in its box, in a game that keeps it for the next stop | Says the change counts for the next stop, not this one. |
| Wets on, made while the car leaves the pits, in a game that ticks every service again then | Says the change won't stick and that the pilot can ask again once back on track; never that it's set for a stop. |
| Box this lap, in a game with stop requests | One `request_stop`, confirmed in a few words. |
| A change the game didn't take | Says it didn't take; never that it's set. |

## The engineer's own idea

| Situation | Pass criteria |
|---|---|
| Rain coming, the pilot asks how the weather looks | Answers the weather and may propose wets at the stop, as a question. No `change` call. |
| The pilot says yes to the proposal from the turn before | One `change` fitting the wets, confirmed with which stop it counts for. |

## Standing orders

| Request that fired | Pass criteria |
|---|---|
| Keep the fuel set to finish, with the menu already right | No `change` call; exactly `[SILENCE]`. |
| Keep the fuel set to finish, with the menu short of what's needed | One `change` to the needed level; one sentence opening with its subject, saying what it set. No `[SILENCE]`. |

Grade the reply as spoken, and the transcript for the tool calls. Any reply that says a change was
made when the transcript has no `change`, or when the result lists it as not taken, fails.
