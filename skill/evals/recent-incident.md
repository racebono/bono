# Recent incident behavioral cases

Controlled manual/model scenarios, not executed results. Supply the stated evidence as
tool results and allow requests for missing evidence. Compare baseline and candidate
with identical model/settings; inspect tool calls as well as the final answer.

## T1 on an unfinished lap

Pilot: "Why did I lose the car?"
Evidence: fresh snapshot just after T1 on lap 4. Lap 3 was clean. Recent telemetry on
lap 4 captures a throttle increase followed by rear wheel slip and rapid rotation;
the supplied channels support power oversteer. `corners` defaults to lap 3.
Pass: acknowledges before tool work, reads `schema` before custom SQL, and inspects
session- and vehicle-scoped recent telemetry including lap 4. Gives the supported cause.
Fail: diagnoses from lap 3, requires a completed lap, or invents an unrelated cause.

## Incident across start/finish

Pilot: "Why did I spin just then?"
Evidence: snapshot at lap 5 T1; loss began near the end of lap 4 and recovery ended
on lap 5. Only samples spanning the boundary capture the triggering inputs.
Pass: queries a recent time window across both laps and bases the diagnosis on the
event, rather than restricting evidence to the snapshot's lap number.

## No complete reference lap

Pilot: "Why did I lose it at T1?"
Evidence: first recorded lap, no completed reference; recent samples capture the event
but wheel channels are unavailable and the remaining channels cannot establish a cause.
Pass: inspects available current-lap data and states the specific uncertainty briefly.
Fail: waits for lap completion, treats missing reference as missing live telemetry,
or confidently attributes the loss to throttle, brakes, tyres or contact without evidence.

## Explicit older incident

Pilot: "Why did I lose the car at T1 on lap 2?"
Evidence: currently lap 5; lap 2 and recent telemetry describe different events.
Pass: targets lap 2 and the named corner, not the most recent incident.
