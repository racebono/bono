# Race strategy behavioral cases

These are controlled scenarios for manual/model evaluation, separate from `evals.json`'s
database-backed cases. Supply the stated evidence and `Race strategy:` briefing line as current
turn context and allow tool calls (`engineer`, `strategy`, `schedule`, `pit`) where relevant.
They are synthetic scenarios; compare baseline and candidate with the same model and settings,
recording actual replies outside the skill.

## Multi-stop rain-window synthesis

Pilot: "How should we play our pit stops from here?"
Briefing: `Race strategy: none.`
Evidence: `stints` shows `laps_left` 18, `full_stint_laps` 28, `laps_needed` 52, `stops_needed` 2;
`forecast_now` shows 80 percent rain chance starting around `approx_lap` 42 (current lap 10).
Dry now, tyres healthy.
Pass: accounts for full-stint capacity and two required stops rather than suggesting one fill to
the flag; proposes aligning a stop with the late rain window (for example, short-fueling the
first stop so the second stop lands near lap 42 if rain arrives); frames rain as a chance and
window, not a certainty; invites pilot input without calling `strategy(set)` yet; spoken prose
within about four sentences and 75 words.

## Multi-stop fuel-saving check with rain

Pilot: "How much would I need to save to drop a stop?"
Briefing: `Race strategy: none.`
Evidence: `stints` shows `stops_needed` 2, `save_pct_needed` 64, `save_pct_one_fewer_stop` 9,
`laps_needed` 55, `full_stint_laps` 30; `forecast_now` shows 75 percent rain chance late in the
race around `approx_lap` 45.
Pass: reports the ~9 percent saving from `save_pct_one_fewer_stop` (not the 64 percent zero-stop
`save_pct_needed`) and notes that likely late rain could force a tyre stop anyway; does not save
a strategy unasked.

## Bare fuel-check scope matrix

Run each variant with the stated context:

1. **Dry bare check, no active strategy:**
   - Pilot: "Can I make it on energy?"
   - Briefing: `Race strategy: none.`, snapshot dry and forecast rain under 20 percent.
   - Pass: verdict only, at most 20 words, no weather mention, no extra `forecast_now` query.
2. **High-rain bare check, no active strategy:**
   - Pilot: "Can I make it on energy?"
   - Briefing: `Race strategy: none.`, snapshot shows stop needed and 70 percent rain late.
   - Pass: gives the energy verdict first; may append at most one brief opt-in question (~6 to 10
     words) offering to plan around the rain; fails if it gives an unsolicited pit lap, fuel load
     or tyre call, or runs an extra `forecast_now` query just to check weather.
3. **Verdict-only with rain:**
   - Pilot: "Can I make it on energy? Just the verdict."
   - Briefing: `Race strategy: none.`, snapshot shows 80 percent rain late.
   - Pass: verdict only, at most 15 words, no weather mention and no opt-in offer.
4. **Bare check with active strategy:**
   - Pilot: "Can I make it on energy?"
   - Briefing: `Race strategy: 2-stop, short-fuel around lap 22, cover rain around lap 42.`
   - Pass: answers concisely (1 to 2 sentences) relative to the agreed plan (on target vs shifted),
     without re-lecturing the whole strategy.

## Collaborative lifecycle and guardrails

1. **Pilot agrees to proposed plan:**
   - Prior turn proposed a 2-stop plan with rain contingency; pilot replies "Sounds good, let's go
     with that."
   - Pass: calls `strategy` with `action: "set"` and a concise, self-contained `plan`; no
     "Copy, checking." acknowledgement if `strategy` is the only tool call; may briefly offer one
     or two opt-in `schedule` watches without calling `schedule` or `pit` unasked.
2. **Mid-race revision:**
   - Briefing has an active `Race strategy:`; forecast shifts dry and pilot says "Let's switch to a
     standard even two-stop then."
   - Pass: calls `strategy` with `action: "set"` replacing the plan with the updated summary, and
     confirms concisely.
3. **Dropping the plan:**
   - Pilot says "Forget the strategy, we'll play it by ear."
   - Pass: calls `strategy` with `action: "clear"` and confirms briefly.
4. **Scheduled turn while strategy is active:**
   - Scheduled turn fires for a rain or pit-window trigger while `Race strategy:` is set.
   - Pass: reports the fired watch concisely (opening with subject, no acknowledgement) and never
     calls `strategy` unasked.
