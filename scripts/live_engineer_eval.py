"""Replay user question sequences against live Rust telemetry tools, without historical answers.

The ignored Rust live_tool_bridge test supplies the embedded skill and exact tool schema.
Each original session is assigned a random opponent, held fixed for its follow-ups.
Measures real SDK + tool wall time, without estimated overhead; does not synthesize audio
or exercise Bono Cloud's remote router. The database is opened read-only by Rust.
"""
import argparse
import concurrent.futures
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import random
import secrets
import sqlite3
import subprocess
import threading
import time

import anthropic


class Bridge:
    def __init__(self, executable, database, out):
        self.lock = threading.Lock()
        self.log = (out / "bridge-stderr.txt").open("w", encoding="utf8")
        env = dict(os.environ, EVAL_DB=str(database))
        self.proc = subprocess.Popen(
            [str(executable), "engineer::tool::evals::live_tool_bridge", "--exact",
             "--ignored", "--nocapture", "--test-threads=1"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log,
            text=True, encoding="utf8", env=env,
        )

    def call(self, request):
        with self.lock:
            self.proc.stdin.write(json.dumps(request, ensure_ascii=False) + "\n")
            self.proc.stdin.flush()
            for line in self.proc.stdout:
                if "EVAL_BRIDGE " in line:
                    return json.loads(line.split("EVAL_BRIDGE ", 1)[1])
            raise RuntimeError("Rust bridge exited before replying")

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.terminate()
            self.proc.wait(timeout=10)
        self.proc.stdout.close()
        self.log.close()


def main():
    ap = argparse.ArgumentParser()
    for name in ["bridge", "db", "turns", "out", "key"]:
        ap.add_argument("--" + name, type=Path, required=True)
    ap.add_argument("--jobs", type=int, default=3)
    ap.add_argument("--only", default="")
    ap.add_argument("--assignments", type=Path)
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    turns = json.loads(args.turns.read_text(encoding="utf8"))
    if args.only:
        ids = set(args.only.split(","))
        turns = [t for t in turns if t["id"] in ids]
    groups = {}
    for turn in turns:
        groups.setdefault(turn["session"], []).append(turn)
    # Public SQLite read-only URI mode; no simulator flags or data are rewritten.
    with sqlite3.connect(args.db.resolve().as_uri() + "?mode=ro", uri=True, timeout=5) as db:
        opponents = db.execute("SELECT vehicle_id FROM telemetry GROUP BY vehicle_id HAVING max(is_player)=0 ORDER BY vehicle_id").fetchall()
        if not opponents:
            raise RuntimeError("No opponent telemetry exists")
        names = dict(db.execute("SELECT vehicle_id,driver FROM standings WHERE id IN (SELECT max(id) FROM standings GROUP BY vehicle_id)"))
    seed = secrets.randbits(64)
    rng = random.Random(seed)
    assignments = {session: rng.choice(opponents)[0] for session in groups}
    if args.assignments:
        previous = json.loads(args.assignments.read_text(encoding="utf8"))
        assignments = {session: previous["assignments"][session] for session in groups}
        available = {vehicle for vehicle, in opponents}
        if not set(assignments.values()).issubset(available):
            raise RuntimeError("A previously assigned opponent is no longer in the live session")
        seed = previous["seed"]
    manifest = dict(seed=seed, assignments=assignments, opponents=names,
                    question_count=len(turns), database=str(args.db),
                    profile="direct Haiku 5.5 high adaptive + actual Rust tools; fresh per-question quick snapshot; no remote router or physical audio")
    (args.out / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf8")
    bridge = Bridge(args.bridge, args.db, args.out)
    metadata = bridge.call({"op": "metadata"})["response"]
    (args.out / "metadata.json").write_text(json.dumps(metadata, ensure_ascii=False, indent=2), encoding="utf8")
    client = anthropic.Anthropic(api_key=args.key.read_text().strip(), max_retries=2, timeout=60)
    done_lock = threading.Lock()
    done = [0]

    def evaluate_session(item):
        session, questions = item
        vehicle = assignments[session]
        identity = names.get(vehicle, str(vehicle))
        system = metadata["system"] + (
            f"\n\nEvaluation driver context: the asking pilot is vehicle_id {vehicle}. "
            "Treat this car as your own for every query. In custom SQL filter that vehicle_id, "
            "not the simulator's is_player flag, which belongs to the spectator. "
            "Historical named rivals in questions may not exist in this live session; "
            "do not silently substitute another driver. Questions and reports are verbatim "
            "pilot messages; get current facts from the attached live snapshot and tools."
        )
        history = []
        results = []
        for turn in questions:
            started_utc = datetime.now(timezone.utc).isoformat()
            began = time.perf_counter()
            events, calls, segments = [], [], []
            first_text = None
            quick_input = {"command": "quick", "vehicle": vehicle}
            events.append(dict(kind="snapshot_start", seconds=0, input=quick_input))
            snapshot = bridge.call({"op": "query", "input": quick_input})["response"]["result"]
            calls.append(dict(input=quick_input, result=snapshot, source="live-snapshot"))
            events.append(dict(kind="snapshot_end", seconds=time.perf_counter() - began))
            question = turn["question"] + "\n\n[BEGIN UNTRUSTED TELEMETRY SNAPSHOT]\n" + snapshot + "\n[END UNTRUSTED TELEMETRY SNAPSHOT]"
            note = f"[Reply in {turn['lang']}, the pilot's language.]" if turn["lang"] else None
            if note:
                question += "\n\n" + note
            # Drop whole old turns to bound memory; never retain original recorded answers.
            while len(history) > 1 and len(json.dumps(history, ensure_ascii=False).encode()) > 120000:
                history.pop(0)
            exchange = [{"role": "user", "content": question}]
            messages = [m for group in history for m in group] + exchange
            stop = None
            error = None
            try:
                for round_number in range(1, 7):
                    with client.messages.stream(
                        model="claude-haiku-5-5", max_tokens=4096,
                        system=[{"type": "text", "text": system, "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
                        tools=[metadata["tool"]], messages=messages,
                        output_config={"effort": "high"},
                        thinking={"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "drop_block"}},
                        metadata={"user_id": "live-engineer-evaluation"},
                        extra_headers={"anthropic-beta": "thinking-binding-controls-2026-08-01"},
                    ) as stream:
                        for event in stream:
                            elapsed = time.perf_counter() - began
                            if event.type == "text" and first_text is None:
                                first_text = elapsed
                            if event.type == "content_block_stop" and event.content_block.type == "text":
                                events.append(dict(kind="text_complete", seconds=elapsed, round=round_number, text=event.content_block.text))
                        message = stream.get_final_message()
                    stop = message.stop_reason
                    segments.extend(b.text.strip() for b in message.content if b.type == "text" and b.text.strip())
                    assistant = {"role": "assistant", "content": [b.model_dump(exclude_none=True) for b in message.content]}
                    messages.append(assistant)
                    exchange.append(assistant)
                    uses = [b for b in message.content if b.type == "tool_use"]
                    if not uses or stop != "tool_use":
                        break
                    blocks = []
                    for use in uses:
                        actual_input = dict(use.input)
                        actual_input.setdefault("vehicle", vehicle)
                        events.append(dict(kind="tool_start", seconds=time.perf_counter() - began, input=actual_input))
                        returned = bridge.call({"op": "query", "input": actual_input})
                        value = returned["response"]
                        events.append(dict(kind="tool_end", seconds=time.perf_counter() - began, rust_seconds=returned["elapsed"]))
                        calls.append(dict(input=actual_input, requested_input=use.input, result=value["result"], source="live-rust", error=value.get("error", False)))
                        blocks.append({"type": "tool_result", "tool_use_id": use.id, "content": value["result"], "is_error": value.get("error", False)})
                    if note:
                        blocks.append({"type": "text", "text": note})
                    tool_result = {"role": "user", "content": blocks}
                    messages.append(tool_result)
                    exchange.append(tool_result)
            except Exception as exc:
                error = str(exc)
                stop = "evaluation_error"
            result = dict(id=turn["id"], session=session, question=turn["question"], lang=turn["lang"],
                          vehicle=vehicle, driver=identity, segments=segments, answer="\n".join(segments),
                          calls=calls, elapsed=round(time.perf_counter() - began, 3), first_speech=first_text,
                          stream_events=events, rounds=round_number, stop=stop, error=error,
                          history=[dict(question=r["question"], answer=r["answer"]) for r in results[-6:]],
                          started_utc=started_utc, ended_utc=datetime.now(timezone.utc).isoformat(),
                          timing_basis=manifest["profile"], spoken_confirmed=None)
            history.append(exchange)
            results.append(result)
            # Persist each completed turn immediately, including any evaluation error.
            (args.out / (turn["id"] + ".json")).write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf8")
            with done_lock:
                done[0] += 1
                print(f"completed {done[0]}/{len(turns)}", flush=True)
        return results

    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as executor:
            results = [r for group in executor.map(evaluate_session, groups.items()) for r in group]
        order = {t["id"]: i for i, t in enumerate(turns)}
        results.sort(key=lambda r: order[r["id"]])
        (args.out / "responses.json").write_text(json.dumps(results, ensure_ascii=False, indent=2), encoding="utf8")
        print(json.dumps({"completed": len(results), "errors": sum(bool(r["error"]) for r in results)}), flush=True)
    finally:
        bridge.close()
        client.close()


if __name__ == "__main__":
    main()
