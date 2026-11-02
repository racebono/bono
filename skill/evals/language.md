# Reply language

Controlled manual/model scenarios, not executed results. They come from real Bono Cloud
turns (AI observability, 28 Oct – 2 Nov 2026). On 2 Nov, an Italian question repeated five
times got one Italian answer and then four English ones. In each English turn, the router
had made the `quick` call, and the last thing before the answer was its English telemetry.
On 31 Oct, "Positions." in a French conversation got an English answer.

The app ends each question in a known language with "[Reply in <language>, the pilot's
language.]", and repeats that note after every tool result. With the ignored harness, set
`EVAL_LANG` (e.g. `it`) so `EVAL_QUESTIONS` are asked as if heard in that language: its
config has no voices, so typed questions get no note. Questions separated by `|` share one
conversation. Check the transcript as well as the answers: the note must be the last block
of each tool-result message. Test Bono Cloud separately from the Claude API, with routing on,
because only the router makes a call without Claude's thinking.

| Pilot questions (`EVAL_QUESTIONS`) | `EVAL_LANG` | Pass criteria |
|---|---|---|
| Situazione meteo\|Situazione meteo\|Situazione meteo\|SITUAZIONE METEO | `it` | Every answer, acknowledgements included, is Italian. None switches to English after a routed `quick` call or after an earlier answer. No English terms ("pista dry"). |
| Quelle est ma position ?\|Positions.\|Thank you. | `fr` | Every answer is French, including the ambiguous "Positions." and "Thank you." the voice speaks in French. |
| ¿Cuánto queda de sesión?\|¿Cómo están las ruedas? | `es` | Spanish answers, with the same brevity and content as the English cases. |
| Where am I losing time? | `it` | The early acknowledgement ("Ricevuto, controllo.") and the final answer are both Italian. |

Grade the content as for the matching English case: the language note must not lengthen
the answer, or introduce the language or the note itself into it.
