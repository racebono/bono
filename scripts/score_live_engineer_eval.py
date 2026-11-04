"""Validate two independent performance verdict sets and report their intersection."""
import argparse
import json
import math
from pathlib import Path
import statistics


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("directory", type=Path)
    ap.add_argument("--deadlines", type=Path, required=True)
    args = ap.parse_args()
    records = json.loads((args.directory / "responses.json").read_text(encoding="utf8"))
    run = {r["id"]: r for r in records}
    if len(run) != len(records):
        raise ValueError("Duplicate response IDs")
    jury = []
    for name in ["sol", "sol-2"]:
        rows = [json.loads(s) for s in (args.directory / "verdicts" / f"{name}.jsonl").read_text(encoding="utf8").splitlines()]
        verdicts = {v["id"]: v for v in rows}
        if len(rows) != len(run) or set(verdicts) != set(run):
            raise ValueError(f"{name}: incomplete or duplicate verdicts")
        deadlines = json.loads((args.deadlines / f"deadlines-{name}.json").read_text(encoding="utf8"))
        for identity, verdict in verdicts.items():
            if verdict["allowed_seconds"] != deadlines[identity]:
                raise ValueError(f"{name}/{identity}: changed deadline")
            criteria = [verdict[k] for k in ["good", "accurate", "timely", "ack_appropriate"]]
            if any(type(value) is not bool for value in criteria):
                raise ValueError(f"{name}/{identity}: non-boolean criterion")
            if verdict["pass"] != all(criteria):
                raise ValueError(f"{name}/{identity}: incorrect pass conjunction")
            if verdict["timely"] != (run[identity]["elapsed"] <= verdict["allowed_seconds"]):
                raise ValueError(f"{name}/{identity}: incorrect timing comparison")
            if verdict["spoken_confirmed"] is not None:
                raise ValueError(f"{name}/{identity}: unsupported physical-playback claim")
        jury.append(verdicts)
    both = [identity for identity in run if all(j[identity]["pass"] for j in jury)]
    times = sorted(r["elapsed"] for r in records)
    summary = dict(
        n=len(run), sol_1=sum(v["pass"] for v in jury[0].values()),
        sol_2=sum(v["pass"] for v in jury[1].values()), both=len(both),
        both_percent=round(100 * len(both) / len(run), 2),
        threshold_met=len(both) / len(run) >= .9,
        median_seconds=statistics.median(times), p90_seconds=times[math.ceil(.9 * len(times)) - 1],
        profile=json.loads((args.directory / "manifest.json").read_text(encoding="utf8"))["profile"],
        failures=[dict(id=identity, reasons=[j[identity]["reason"] for j in jury if not j[identity]["pass"]])
                  for identity in run if identity not in both],
    )
    (args.directory / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf8")
    print(json.dumps({k: v for k, v in summary.items() if k != "failures"}))


if __name__ == "__main__":
    main()
