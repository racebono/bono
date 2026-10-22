# Engineer response evaluations

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

Use the same database, model and effort for the old and new skill, with multiple runs.
Build both versions: topic references are compiled into the tool, so changing only
`EVAL_SKILL` leaves the newer tool guidance in the baseline. Save distinct `EVAL_CONFIG`
and `EVAL_RUN` values. The current model comparison uses Haiku 5.5 high and Sonnet 5.5 low.

`scripts/generate_mock_telemetry.py` at the repository root creates controlled race and
practice fixtures for these cases. They are synthetic and cannot establish real-recording
parity. The fixtures provide the named T2/T3 losses, a hot/worn front left, slower Ruiz,
an energy deficit, and separate practice/race weather outlooks used by the assertions.
