# Proposal: keep long Bono Cloud conversations within the website's limits

Status: proposal, not implemented. Builds on #51 (Claude API for Bono Cloud).

## Problem

The engineer keeps one conversation per game session. It is reset only when
`SessionState::follow_game_session` (`src/engineer.rs`) sees another track,
session type or session number. Every question adds a user message carrying a
telemetry snapshot (`question_with_snapshot`, `src/engineer/tool.rs`). Each tool
round adds an assistant message and a `tool_result` message, and the full
history is resent with every request.

The website rejects requests over these limits (bono-web
`src/lib/cloud-protocol.ts`):

| Limit | Value | Response |
| --- | --- | --- |
| Request body | 160,000 UTF-8 bytes | `413 request_too_large` |
| Messages | 128 | `400 invalid_request` ("This request is not supported by Bono Cloud.") |

A long practice or endurance race reaches one of them. From then on, every
question in that game session fails:

- `Turn` (`src/provider/claude.rs`) correctly rolls the failed turn back.
- But the history before that turn stays as it was, so the next request is just
  as large.
- 413 isn't retried and 400 isn't recoverable, so the driver gets an error on
  every question until the next session starts.

Request size also matters before the hard limit. Outside a Cloud session lease,
the website reserves about 1 micro-USD per body byte against a daily ceiling.
Without a lease, requests above roughly 88 KB are rejected as
`daily_allowance_exhausted` even with no usage that day. Sessions are proposed
separately; keeping history small helps in both cases.

## Goals

- A conversation never gets stuck: the engineer keeps answering for a whole
  game session, however long.
- The cached prefix stays stable. Trimming happens rarely and in big steps,
  never on every turn, so prompt caching keeps working.
- History stays valid for the Claude API: tool results stay paired with their
  `tool_use`, the first message is a user message, and the latest turn's
  thinking blocks are untouched.
- Direct Claude, and other providers that don't need this, behave exactly as
  they do today.

## Proposed design

### 1. History budget on `Conversation`

Add an optional `HistoryLimits { max_bytes, max_messages, target }` to
`ConversationBuilder` (`history_limits(...)`). It is off by default, so direct
Claude keeps using its own context window and optional compaction. Bono Cloud
sets it in `bono_cloud.rs` next to `server_settings()`:

```rust
.history_limits(HistoryLimits {
    max_bytes: 120_000,   // headroom under the website's 160,000 for system, tools and the new turn
    max_messages: 100,    // headroom under 128 for a tool loop of `max_requests` rounds
    target: 0.5,          // trim down to half of each budget
})
```

### 2. Trim on whole turns, before a turn starts

In `run_turn`, before pushing the new user message, measure the history: the
serialized `messages` length, plus the system prompt and tool definitions, which
are constant per conversation and can be measured once. If adding the question
would exceed either budget, call `trim_history`:

- **Turn boundaries.** A turn starts at a user message whose `content` is a
  string, which is how questions are pushed. Tool results are arrays, so they
  are never treated as turn starts. Removing whole turns from the front keeps
  each `tool_use` with its `tool_result` and keeps the first message a user
  message.
- **Drop the oldest turns** until both measurements are at or below `target`
  times their budget. Trimming to half the budget, rather than just under it,
  means one cache rewrite buys many cached turns before the next trim.
- **Never drop the newest turn.** If one turn is over the budget by itself (a
  huge tool result, say), fall back to step 3 for that turn's tool results
  instead.
- **Thinking blocks:** removing earlier assistant turns, including their
  thinking, is allowed by the API. Only the current tool loop's thinking must be
  sent back unchanged, and that loop is never trimmed.

Trimming between turns, rather than mid-loop, matters. Within a tool loop, the
latest assistant message's signed thinking and `tool_use` must be resent exactly
as received.

### 3. Shrink old telemetry snapshots during the same trim

The snapshot is the bulk of each question's bytes and is only meaningful for its
own moment. While trimming, which already invalidates the cache, also replace
the snapshot section of every kept question except the last few (say 4). The
section is everything between `[BEGIN UNTRUSTED TELEMETRY SNAPSHOT]` and
`[END UNTRUSTED TELEMETRY SNAPSHOT]`, and it becomes `[telemetry snapshot omitted]`.
This removes most of the history's size while keeping what the driver asked and
what the engineer answered, so the next trim comes much later.

Old answers and questions are kept verbatim. Only bytes that were already stale
go.

### 4. Recover instead of failing on limit errors

As a safety net for estimates that are off, or for limits that change on the
website:

- On `413` (`request_too_large`), or on a `400` that the website marks as a
  history limit (see the website change below), roll the turn back as today.
  Then run `trim_history` with a smaller target (for example a quarter) and
  retry the turn once.
- If the retry still fails, return the error as today. Only then fall back to
  `reset()`, keeping the new question, so the engineer answers with no history
  rather than not at all. Log it at `warn` level.

### 5. Tell the driver, quietly

When history is trimmed, log it at `info` level ("Bono Cloud: forgot N older
messages to stay within limits"). Add an optional chat notice the first time it
happens in a session ("Older conversation forgotten to keep answering"), using a
new `engineer.history_trimmed` string in `i18n/`. Nothing is spoken.

## Website change (bono-web)

To make step 4 reliable rather than based on message matching:

- Return a distinct code, `conversation_too_long`, for the 128-message cap,
  still as HTTP 400 with type `invalid_request_error`. The app can then tell it
  apart from other 400s.
- Document both limits in `docs/cloud-ai.md` as part of the contract, and
  optionally expose them in the `/v1/models` entry
  (`max_input_bytes`, `max_messages`) so the app can read them rather than
  hard-coding them.

## Cost and caching

Bono Cloud caches the system prompt and conversation for one hour.

- Each trim rewrites the cache for the shorter history once, at the one-hour
  cache-write rate.
- With a 50% target and snapshot stripping, a trim should happen at most every
  few dozen questions, so the extra cost is small next to the cache reads it
  preserves.
- Trimming on every turn (a sliding window) would rewrite the cache on every
  request, so this proposal avoids it.

## Alternatives considered

- **Server-side compaction (`context_management`).** It keeps context instead
  of dropping it, but it costs an extra billed summarization step that the
  website would have to price, and the website currently strips
  `context_management`. It could be a later option on the website side.
- **Reset the whole conversation when a limit is hit.** It's the simplest
  option, but the engineer forgets everything from the session, including
  strategy agreed earlier.
- **Raising the website limits.** That only postpones the problem, and body size
  also drives the allowance reservation (see above).
- **Summarizing old turns on the client with another model call.** It adds
  latency and cost on the driver's account, and adds complexity for little gain
  over step 3.

## Tests

In `src/provider/claude.rs`, using the existing mock server:

- A conversation grown past `max_bytes` or `max_messages` sends a trimmed
  history. The first message is still a user string, there are no orphan
  `tool_result`s, and the latest turn is unchanged.
- Old snapshots are replaced and the last few are kept; questions and answers
  are verbatim.
- A trim happens once, then not again for the following turns while under the
  budget, so the cached prefix is stable.
- A `413` response leads to an aggressive trim and one retry. A second `413`
  returns the error, and the history is not left half-trimmed.
- Without `history_limits`, requests are byte-for-byte what they are today.

In `src/provider/bono_cloud.rs`: the Bono Cloud conversation enables the limits,
and a long scripted session against the mock keeps getting answers.
