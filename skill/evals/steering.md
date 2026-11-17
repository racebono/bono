# Questions asked mid-turn

Controlled manual/model scenarios, not executed results. Ask the first question, let the model
send its acknowledgement and tool calls, then hand back the tool results followed by the app's
steer text with the later words, as the provider sends it (and the reply-language note after it
when the case has one). Compare baseline/candidate with identical model/settings and inspect the
whole turn's text and queries, not only the final answer.

In every case the turn gives one reply after the hand-back: no second acknowledgement, no answer
spoken twice, and nothing the pilot corrected or withdrew answered. Words said after the last
hand-back arrive as an ordinary question in the next turn and are out of scope here.

| First question | Said while working | Controlled evidence | Pass criteria |
|---|---|---|---|
| Where am I losing time in sector one? | "No, sector two." | Sector one and sector two losses are both available; the tool results cover sector one only. | Answers sector two only, querying it if the results don't cover it; says nothing about sector one. |
| How are the tyres? | "And the fuel?" | Tyre results returned; the current stint has a fuel margin. | One reply covering tyres and fuel, within the budget for two parts; fetches fuel only if the snapshot or results lack it. |
| Am I catching Ruiz? | "How many laps until I'm on him?" | Gap history to Ruiz shows a steady closing rate. | One reply giving the trend and the laps to catch, from the same evidence, without a fresh acknowledgement or a repeated verdict. |
| How's the energy? | "Can we skip the stop?", then "When does the rain come?" | Both steers arrive in one hand-back. Energy margin and forecast are available. | One reply covering energy, whether the stop can be skipped, and the rain timing as a chance; answers each once. |
| Dove sto perdendo tempo? | "¿Y los frenos?" | The turn started in Italian, so the note says "[Reply in Italian.]". Corner losses and brake temperatures are available. | The whole reply is in Italian, the note's language, and answers both questions; no Spanish words or acknowledgement. |
