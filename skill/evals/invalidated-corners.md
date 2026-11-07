# Invalidated corner evidence

Controlled manual/model scenarios, not executed results. Supply the stated corner passes and
lap flags, keeping the same session, corner, relevant time window and comparable conditions.
Inspect both the SQL selection and the spoken reply. These cases concern the engineer, not
automatic track-limit warnings.

| Pilot request | Controlled evidence | Expected behavior |
|---|---|---|
| What happened at that corner on the last lap? | The requested lap is invalidated and has usable corner measurements. | Uses the requested pass and explains the supported event; does not replace it with an older clean lap or refuse because of invalidity. |
| Where did I lose time, and why did the tyre get hot there? | Only invalidated laps have comparable passes and wheel-temperature changes. | Uses that evidence for the requested information and explanation; no blanket valid-lap filter. |
| What should I change to go faster through this corner? | A usable clean reference exists; a faster invalidated reference also exists. | Bases the improvement suggestion on the clean reference, even though the invalidated one is faster. The observed pass may be invalidated. |
| What should I change to go faster through this corner? | All usable references for the same corner in the relevant comparison window are invalidated. | Uses a supported invalidated reference and briefly tells the pilot the suggestion is based on an invalidated lap/pass; does not silently present it as clean. |
| What should I change at this corner? | Clean laps exist but their relevant corner measurements are missing; an invalidated pass has those measurements. | Uses the usable invalidated fallback with the same explicit caveat rather than treating clean lap metadata as sufficient evidence. |
| Why was this corner slower? | The lap was invalidated later; no corner-specific violation is recorded. | Uses the observed corner and does not claim this particular corner caused the invalidation. |
| Where can I gain time at this corner? | The only invalidated passes are incomplete or affected by a shortcut/contact that prevents a comparable recommendation. | States the evidence limit; invalidity fallback does not justify inventing a gain or recommending a shortcut. |
