"""Create performance-only council cards from live engineer evaluation artifacts."""
import argparse
import json
from pathlib import Path


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("directory", type=Path)
    args = ap.parse_args()
    records = json.loads((args.directory / "responses.json").read_text(encoding="utf8"))
    manifest = json.loads((args.directory / "manifest.json").read_text(encoding="utf8"))
    cards = [
        "# Live-session engineer performance\n\n"
        + manifest["profile"]
        + "\n\nThese are verbatim historical questions in their original session order, "
        "answered anew against a live session. Each conversation has one random opponent "
        "as its asking driver. No original answers or old telemetry are supplied. "
        "Current live conditions may differ from the original questions' assumptions. "
        "A missing rival, incident or race length must be handled honestly, not invented. "
        "The identity override is fixture context; SQL must target that vehicle rather "
        "than the simulator's spectator is_player flag. All model/tool durations are "
        "measured, not estimated. Physical speech was not played or observed. "
        "This profile exercises the skill/model plus native Rust tools, not Bono Cloud "
        "routing, app language detection or the local trivial-message shortcut.\n"
    ]
    for record in records:
        parts = [f"## {record['id']}",
                 f"Asking driver: {record['driver']!r}, vehicle_id {record['vehicle']}. "
                 f"Original conversation: {record['session']}. Live request: {record.get('started_utc', 'not captured')}."]
        if record["history"]:
            parts.append("Earlier in this newly generated conversation:\n" + "\n".join(
                f"Pilot: {h['question']}\nEngineer: {h['answer']}" for h in record["history"]))
        parts.append(f"Pilot asks (verbatim): {record['question']!r}")
        if record["lang"]:
            parts.append(f"Explicit reply-language note: {record['lang']}")
        for call in record["calls"]:
            result = call["result"].split("\nENGINEERING REFERENCE", 1)[0]
            if call["input"].get("command") == "schema":
                result = "[Current schema/engineering reference returned; full text is retained in the raw response JSON.]"
            parts.append(f"{call['source']} {json.dumps(call['input'], ensure_ascii=False)}:\n```\n{result}\n```")
        parts.append("Generated response segments (in order):\n" + "\n".join(repr(s) for s in record["segments"]))
        parts.append(f"First generated words: {record['first_speech']} s. "
                     f"Complete model/tool wall time: {record['elapsed']} s. "
                     f"Stop: {record['stop']}. Error: {record['error']}.")
        parts.append("Measured event order:\n" + "\n".join(
            json.dumps(e, ensure_ascii=False) for e in record["stream_events"]))
        parts.append("Physical playback: unobserved (spoken_confirmed=null).")
        cards.append("\n\n".join(parts))
    dest = args.directory / "cards.md"
    dest.write_text("\n\n---\n\n".join(cards), encoding="utf8")
    print(f"Wrote {len(records)} cards to {dest}")


if __name__ == "__main__":
    main()
