# Current-lap evidence windows

Controlled manual/model scenarios, not executed results. Supply the stated evidence as
snapshots/tool results and allow missing-evidence requests. Compare baseline/candidate
with identical model/settings and inspect queries as well as answers. All custom SQL
must follow `schema` and be bounded by current-session ids and vehicle. Slow work needs
the separate acknowledgement required by the main skill.

| Pilot question | Controlled evidence | Pass criteria |
|---|---|---|
| Was that braking better at T1? | Current lap T1 is complete and faster than the matched previous pass; last completed lap T1 was slower. | Compares the just-driven corner with the matched earlier pass, without waiting for lap completion or answering about the older pass. |
| Where am I losing time this lap? | Only T1 and T2 have been driven; T2 exit is incomplete. Matched data shows a T1 entry loss. Previous lap's largest loss was T5. | Reports the supported loss so far; doesn't name T5 or invent a T2 exit/full-lap delta. |
| Why did the fronts get hot just then? | Recent current-lap wheel and brake samples capture a lockup and temperature rise. Earlier lap was normal. | Inspects the recent event and gives only the supported explanation, not a diagnosis from old lap averages. |
| Did that hit affect the car? | Current-lap impact and new damage are recorded; no comparable post-impact driving yet. | Reports recorded damage, but does not assert a measured pace/handling effect or invent the contact partner. |
| Is the new brake bias helping? | Bias changed mid-lap. A matched later braking event improved, but grip and driver input also changed. | Uses before/after events including this lap; describes the local improvement without proving bias caused it or claiming sustained gain. |
| Has it become too wet for slicks? | Current wetness has risen sharply since the last completed dry lap; recent comparable grip evidence supports wets. | Uses current conditions and supported grip evidence for the decision; doesn't wait for a completed wet lap or use old dry pace as current evidence. |
| Am I saving enough now? | Saving began this lap. Matched recent distance spans show reduced use, but the sample is short and `stint` still predicts a deficit. | Recognises the local saving; retains the range baseline and uncertainty, without guaranteeing the finish or comparing partial use to a full lap. |
| Am I catching Ruiz now? | Last completed laps show Ruiz faster; current-lap gap history shows the gap shrinking. | Answers from observed recent gap movement, not last-lap pace. Does not invent why Ruiz slowed. |
| Was my braking better on lap 2? | Currently lap 5 with different braking behaviour. Lap 2 and its comparison are available. | Honours lap 2, rather than substituting the newest event. |
| Are my tyres wearing faster over the stint? | Current lap has a temperature spike; representative completed laps show stable wear rate. | Uses sustained wear evidence, not a transient temperature spike as proof of faster wear. |

Also repeat a recent-event case with missing relevant channels/history: pass requires a
specific brief uncertainty after inspecting available evidence, not an invented verdict
or a request to finish the lap. Fresh snapshot-only questions should still need no SQL.
