# Engineer response evaluations

`recent-incident.md` covers loss-of-control questions on an unfinished lap, incidents
across start/finish, missing channels and explicitly requested older incidents. These
are controlled manual/model scenarios; adding them does not establish a pass.

`wet-transition.md` adds controlled manual/model scenarios for current wetness, compound
choice at a required stop, and reconsidering slicks late in a wet race. These require
their stated evidence; do not run them against the existing dry database unchanged.

These cases test the race engineer's answers, not the automatic spotter. Direct side/clear
calls, immediate alerts and imminent-rain announcements are excluded. Rival pace, longer
weather outlooks, strategy and driving advice remain in scope.

Judge both brevity and completeness. A short response that omits a requested action,
timing, reason or tyre measure fails. A detailed response that adds unrequested advice or
reads out displayed channels also fails. Word and sentence limits exclude a brief
acknowledgement before slow tool calls; they are evaluation bounds, not output truncation.

`session` in each case identifies the required recording. Run race and practice separately
with matching `EVAL_IDS`; running every case against one database gives invalid results.
The ignored Rust harness records replies and transcripts but does not itself grade these
behavioral assertions. Check API errors and grade the recorded answers explicitly.

Also inspect streaming text and tool-call timestamps for responsiveness. For slow
corner/history/SQL requests, the acknowledgement must be emitted as a separate text
block ending in a newline before the first tool call, followed by the answer after
the results. An acknowledgement prepended to the final answer fails even if its words
are correct. Check that the acknowledgement reaches speech before tool work finishes.
For a standalone radio check or facts present in the attached snapshot, expect a direct
answer with zero tool calls and no acknowledgement preamble. Compare time to first text
and first audible speech separately from total completion time, using the same model,
effort and fixture for baseline and candidate. Record measured times; a transcript alone
cannot establish improved latency.

Use the same database, model and effort for the old and new skill, with multiple runs.
Build both versions: the skill, schema and topic references are all compiled into the tool. Save distinct `EVAL_CONFIG`
and `EVAL_RUN` values. The current model comparison uses Haiku 5.5 high and Sonnet 5.5 low.

`scripts/generate_mock_telemetry.py` at the repository root creates controlled race and
practice fixtures for these cases. They are synthetic and cannot establish real-recording
parity. The fixtures provide the named T2/T3 losses, a hot/worn front left, slower Ruiz,
an energy deficit, and separate practice/race weather outlooks used by the assertions.
