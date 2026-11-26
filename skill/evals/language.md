# Reply language

Controlled manual/model scenarios, not executed results. They cover answering in the
selected reply language across tool calls (whose telemetry is in English), repeated questions, earlier answers in another language and
ambiguous short questions.

The app ends each question with a known reply language with "[Reply in <language>.]",
and repeats that note after every tool result. With the ignored harness, set
`EVAL_LANG` (e.g. `it`) so `EVAL_QUESTIONS` are asked as if heard in that language: its
config has no voices, so typed questions get no note. Questions separated by `|` share one
conversation. Check the transcript as well as the answers: the note must be the last block
of each tool-result message. Test Bono Cloud separately from the Claude API.

| Pilot questions (`EVAL_QUESTIONS`) | `EVAL_LANG` | Pass criteria |
|---|---|---|
| Situazione meteo\|Situazione meteo\|Situazione meteo\|SITUAZIONE METEO | `it` | Every answer, acknowledgements included, is Italian. None switches to English after a tool call or after an earlier answer. No English terms mixed into the Italian. |
| Quelle est ma position ?\|Positions.\|Thank you. | `fr` | Every answer is French, including the ambiguous "Positions." and "Thank you." the voice speaks in French. |
| ¿Cuánto queda de sesión?\|¿Cómo están las ruedas? | `es` | Spanish answers, with the same brevity and content as the English cases. |
| Where am I losing time? | `it` | The early acknowledgement ("Ricevuto, controllo.") and the final answer are both Italian. |

Grade the content as for the matching English case: the language note must not lengthen
the answer, or introduce the language or the note itself into it.

Also exercise independent input and output settings in the app: select a Spanish heard
language and an English spoken language, then ask Spanish questions requiring direct
answers, early acknowledgements and multiple tool calls. Every spoken part must remain
English, including after tool results and earlier Spanish conversation. Repeat after
changing the spoken language between questions; the latest reply note must take priority.

## Misheard words

Spoken questions come from speech recognition. Ask with a racing word replaced by others
that sound like it: split in two ("over steer" as two unrelated words), a term swapped for
an everyday word, a rival's surname spelled as a common noun, or a corner's name written
by its sound. Include one where the intended word is clear from the telemetry or the
earlier question, and one where two readings would be answered differently.

| Case | Pass criteria |
|---|---|
| The meaning is clear from its sound, the telemetry or the earlier question | Answers what was meant, with the same content and length as the correctly heard question. Doesn't repeat the wrong word or mention it was misheard. |
| Two readings would be answered differently | Asks one short question naming them, instead of guessing. |
| The odd word doesn't change the answer | Answers directly, without asking. |
