# Wet-transition behavioral cases

These are controlled scenarios for manual/model evaluation, separate from `evals.json`'s
database-backed cases. Supply the stated evidence as current tool results and allow the
engineer to request missing evidence. They are not reconstructions of the Barcelona
telemetry: the supplied transcript and screenshots show replies, not surface readings.
Compare baseline and candidate with the same model/settings; record actual replies and
grade the assertions. See `wet-transition-results.md` for actual controlled forward-test
results and their limitations; adding scenarios alone does not establish a pass.

## Present severity versus rain

Pilot: "¿Cómo está la pista?"
Evidence: fresh racing-line wetness min 0.45, average 0.55, max 0.70; rain 0.05.
No wetness history is supplied.
Pass: describes the track as wet in Spanish, without claiming it is only starting to
get wet, mostly dry or just one wet patch. Does not infer a trend or add a pit plan.

## Compound at an already-required stop

Pilot: "Tengo que parar por energía. ¿Pongo seco o mojado?"
Evidence: energy stop due this lap; fresh wetness average 0.55, min 0.45, max 0.70;
average rose from 0.30 over the last minute; rain continues. Comparable clean running
in these conditions supports wets being faster; fitting tyres adds no service time.
Pass: recommends wets for this stop. Does not use light rain, an earlier slick recommendation
or the cost of an entire extra pit stop to argue for dry tyres. Does not invent thresholds.

## Extra stop with six laps left

Pilot: "¿Paro por mojados?"
Evidence: six laps remain; slicks fitted at the previous stop; current surface remains
wet and is worsening. Comparable clean laps support a conservative ten-second-per-lap
advantage on wets including warmup; total extra stop cost is thirty seconds.
Pass: recommends stopping for wets; does not reject solely because six laps remain or
because slicks were just fitted. Uses the supplied comparison without inventing a cause.

## Insufficient evidence after a stop

Pilot: "Está muy mojada y me cuesta mantener el coche. ¿Vuelvo por mojados?"
Evidence: five to ten minutes remain, slicks fitted, current rain is light; surface
wetness, comparable wet pace and pit loss are unavailable even after relevant lookups.
Pass: takes the pilot's driveability report into account, qualifies any recommendation
and states the material uncertainty briefly. Does not assert the track is nearly dry,
that slicks are adequate, or that another stop cannot pay back based only on time left.

## Avoid an unnecessary wet stop

Pilot: "¿Pongo mojados ya?"
Evidence: racing line dry, wetness min/average/max all zero and stable; no grip issues;
forecast gives a high rain chance later but no precise onset or wet-tyre pace evidence.
Pass: stays on slicks now, with any later switch conditional on observed conditions;
does not treat forecast probability as surface water or promise an exact arrival minute.

## Extra stop that does not pay back

Pilot: "¿Paro por mojados?"
Evidence: two laps remain, damp and stable, driver comfortable; reliable wet-tyre
advantage two seconds per lap including warmup, additional stop thirty seconds.
Pass: recommends staying on slicks. Does not overcorrect into always recommending wets.
Repeat with six laps remaining and three seconds per lap: same expected decision.

## Fresh evidence overturns earlier advice

Pilot: "¿Pongo seco, no?"
Evidence: prior reply recommended slicks; fresh wetness min 0.50, average 0.60, max 0.75
and increasing; energy stop now, comparable clean pace favours wets; tyre service overlaps
the energy stop.
Pass: recommends wets despite the leading question and previous slick recommendation.

## Wet despite stopped rain

Pilot: "¿Cómo está la pista?"
Evidence: rain zero, wetness min 0.50, average 0.60, max 0.70; average was 0.70 a minute ago.
Pass: describes a wet track that is drying, not a dry track or increasing wetness.

## Wheel surface without racing-line readings

Pilot: "¿Cómo está la pista?"
Evidence: light rain, one wheel reports wet surface; racing-line readings/history unavailable.
Pass: does not infer an isolated patch or overall severity from the wheel alone; briefly
states the missing surface evidence.

## Rain intensity versus forecast probability

Pilot: "¿Cómo está la pista?"
Evidence: rain 0.05, forecast rain_chance 0.90, wetness min 0.40, average 0.55, max 0.65;
no history.
Pass: describes a wet track; neither intensity nor forecast probability overrides the
surface evidence. Does not invent a wetting trend.

## Compound answer covers condition, action and reason

Pilot: "¿Cómo está la pista y qué monto en la parada obligatoria, y por qué?"
Evidence: wet and worsening, energy stop this lap, comparable clean pace supports wets
faster, tyre service overlaps energy stop.
Pass: answers condition, wets at the required stop, and a supported reason in at most
three sentences and 45 words.

## Very little time but poor control

Pilot: "¿Pongo mojados?"
Evidence: two minutes remain; pilot reports repeated loss of control on slicks in
standing water; comparable pace and pit loss unavailable.
Pass: gives a qualified driveability recommendation rather than rejecting wets solely
because little time remains; does not claim a proven time gain.

## Energy-only scope control

Pilot: "¿Podemos llegar con la energía?"
Evidence: range short by two laps, very wet track.
Pass: answers the energy deficit without adding an unrequested wet-tyre or pit timing plan.

For all cases: Spanish spoken prose, at most 20 words for a simple question; no raw-data
recital or unsupported precision. Compound questions retain every requested part.
