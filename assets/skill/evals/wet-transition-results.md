# Wet-transition forward-test results

Date: 2026-10-23. Three independent T3 child runs using Codex `gpt-6.1-sol`,
reasoning effort `low`, service tier `default`. Each read only the current `SKILL.md`
and `references/strategy.md`, then answered independent Spanish pilot questions from
supplied synthetic evidence. No expected answers or evaluation assertions were supplied.
Later rounds received earlier findings as required for delegated review rounds.
Parent reviewed the actual answers against `wet-transition.md`.

This tests instruction-following on supplied evidence, not the app's live telemetry
retrieval, speech latency, or Barcelona's real crossover. It is not a baseline comparison,
statistical reliability estimate, or a test of the Claude Haiku model in the screenshots.

## Round 1: ten scenarios

Correct decisions/evidence handling in ten of ten cases. Nine of ten met the simple
twenty-word budget: case 4 had 24 words. The evaluator also found strategy's sixty-word
exception inconsistent with the main skill's forty-five-word compound-question limit.

Actual replies, in scenario order (severity, required stop, six laps, missing evidence,
dry track, unprofitable stop, changed evidence, drying track, wheel-only, energy-only):

1. La pista está mojada en toda la trazada, con zonas más mojadas que otras.
2. Pon mojados en la parada de esta vuelta.
3. Sí, compensa parar por mojados.
4. Recomiendo volver por mojados por la dificultad de control, aunque faltan datos para confirmar si compensa la parada.
5. Sigue con secos ahora; reconsidera los mojados si llega la lluvia y se moja la trazada.
6. No, sigue con secos.
7. Pon mojados en esta parada.
8. La pista sigue mojada en toda la trazada, aunque se está secando.
9. Hay lluvia ligera, pero faltan datos de la trazada para valorar cuánto está mojada.
10. No, nos faltan dos vueltas de energía.

Changes: align compound budgets; explicitly recognize pilot-reported control difficulty
as evidence for a driveability recommendation, separate from proven time gain; illustrate
a brief, specific uncertainty caveat.

## Round 2: fourteen scenarios

All fourteen decisions were supported and answers met their word budgets. Cases 1 and
11 added an unrequested missing-history caveat to a question about current severity,
so they failed the main skill's scope rule despite correctly calling the track wet.
Cases 1-10 repeat round 1; cases 11-14 add intensity/probability semantics, a six-lap
unprofitable stop, a compound question, and poor control with two minutes remaining.

1. La pista está mojada, con zonas más cargadas de agua; sin historial no puedo confirmar la evolución.
2. Monta mojados en esta parada.
3. Sí, compensa parar por mojados.
4. Sí, volvería por mojados para controlar el coche; no puedo confirmar la ganancia de tiempo.
5. No, mantén secos mientras la trazada siga seca y estable.
6. No, sigue con secos.
7. No, monta mojados en esta parada.
8. La pista está mojada, con zonas más cargadas de agua, aunque se está secando.
9. No puedo determinar cuánto está mojada la trazada con la lluvia y una sola rueda.
10. No, nos faltan unas dos vueltas.
11. La pista está mojada, con zonas más cargadas de agua; sin historial no puedo confirmar la evolución.
12. No, sigue con secos.
13. La pista está mojada y empeorando. Monta mojados en esta parada obligatoria: son más rápidos y el cambio no añade tiempo.
14. Sí, monta mojados para recuperar el control; no puedo confirmar que compense en tiempo.

Change: clarify that missing history needs no caveat when current severity is known and
the pilot did not ask about a trend.

## Round 3: seven targeted checks of the final revision

All seven passed decision, evidence, scope and twenty-word budget checks. First two
repeat round 2's scope failures. Third asks about the trend with the same missing history.
Remaining cases check wheel-only uncertainty, driver-reported poor control, and profitable
versus unprofitable extra stops with six laps left.

1. La pista está bastante mojada, con zonas más húmedas que otras.
2. La pista está bastante mojada, con zonas más húmedas que otras.
3. La pista está bastante mojada; sin historial no puedo confirmar si se está mojando más.
4. No puedo determinar cómo está la pista en general solo con una rueda mojada.
5. Sí, volvería por mojados para recuperar control; no puedo confirmar que compense en tiempo.
6. Sí, compensa parar por mojados.
7. No, sigue con los neumáticos actuales.

The full fourteen-case suite was run before the final scope clarification; the final
revision received these seven targeted checks, not a fresh fourteen-case full run.

## Structural checks

Bundled `quick_validate.py assets/skill`: passed using temporary PyYAML 6.0.3.
`git diff --check`: passed. No Rust code changed or application integration tests run.
