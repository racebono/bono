# Scheduled requests

Controlled manual/model scenarios, not executed results. They cover adding requests with the
`schedule` tool, managing pending ones by talking, and the engineer's own scheduled turns,
silent and spoken. They are evals 30 to 45 in `evals.json`, run against the synthetic race
fixture (practice for the race-start reminder).

They need a harness that registers the `schedule` tool, records its calls in the transcript,
and understands two fields beside the usual ones:

- `pending`: requests already pending when the question is asked. Each is the `schedule`
  tool's input for an add (`instruction`, `trigger`, `repeat`, `until`, `stale_after`) without
  `action`, plus the `id` the pending list gives it: ids number from `#1` in the order listed.
  The harness adds them before the question, so the question carries them in its pending list
  as the app's would.
- `scheduled`: the requests that fired, each with its `instruction` and `why` (why it fired
  now, worded as the app words it). A case with it is sent as the engineer's scheduled-turn
  message, not as a pilot question; its `prompt` is empty because nobody asked.

A harness without these sends every case as a plain question with no schedule tool, which
says nothing about these behaviours: leave these ids out of `EVAL_IDS` there.

## Scheduling

| Pilot question | Pass criteria |
|---|---|
| Fuel at the end of every lap for the next five laps | One add: one lap, repeating, until five laps; `stale_after` keeps a lap's figure from being said after the next lap ends. Self-contained instruction. No fuel figure now. One sentence confirming what and when, with no "checking" acknowledgement first. |
| A reminder in ten minutes | A `time` trigger of the requested length, not a clock time or lap worked out by the model. |
| In practice: at the race start, remind me of something | A `session` trigger for the race. The instruction says it was asked in practice. |
| Tell me if the car behind gets within a second | A condition that returns a row only while it holds, finding the player with `is_player = 1` or `race_state.player_vehicle_id`. If the tool says it's already true, the confirmation says so and that the call comes when it next happens. |
| Tell me when a tyre drops below a level | A condition over a table without `is_player`, so the player comes from `race_state.player_vehicle_id` or a join; no literal vehicle id. |
| A rival's gap every lap until I pit | A lap trigger with an `until` condition on the player's pit lane; the rival named in the instruction, not by id. |

Any literal `vehicle_id` number in a condition fails: ids change between sessions.

## Managing

Each case has the same three pending requests: fuel every lap for five laps, a call if the car
behind gets within a second, and a brake bias reminder.

| Pilot question | Pass criteria |
|---|---|
| What have you got scheduled? | No tool call. All three, by what and when, without ids or symbols, within the breakdown budget. |
| Forget one of them | A cancel of that id only, confirmed in one sentence. |
| Make one of them less frequent | A replace of that id, not a cancel and an add, keeping its `until`; confirmed in one sentence. |
| Start one of them again from now | A cancel and a new add, not a replace, which would keep what was already counted. The others are untouched. |

## Scheduled turns

| Requests that fired | Pass criteria |
|---|---|
| A watch for something that hasn't happened | Exactly `[SILENCE]`, nothing before or after it, no acknowledgement and no schedule call. |
| A watch for rain on a dry track with rain forecast later | Exactly `[SILENCE]`: the forecast wasn't asked about. |
| Two watches, neither with news | One `[SILENCE]` for the turn, not a "nothing to report" per request. |
| A check the data answers | Opens with the subject ("Energy check: …"), then a brief verdict. No acknowledgement, even before tool calls. |
| A check with an answer and a watch without news | Only the check, opening with its subject; the watch is left out and there's no `[SILENCE]`. |
| A figure each lap | Opens with the subject, then the figure; one sentence, no advice, no new requests. |

Grade the reply as spoken, and the transcript for the tool calls. A scheduled turn that
acknowledges, explains why it's speaking, or says there's nothing to report instead of
`[SILENCE]` fails. Run them on the same models as the other cases.
