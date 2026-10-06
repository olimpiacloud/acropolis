import json
import sys


def steps(path):
    out = {}
    for line in open(path):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            e = json.loads(line)
        except ValueError:
            continue
        if e.get("type") == "step_started":
            out.setdefault(e["id"], {})["start"] = e["t"]
        elif e.get("type") == "step_finished":
            s = out.setdefault(e["id"], {})
            s["end"] = e["t"]
            s["ms"] = e["ms"]
        elif e.get("type") == "build_finished":
            out["_total"] = {"start": 0, "end": e["t"], "ms": e["t"]}
    return out


runs = [steps(p) for p in sys.argv[1:]]
ids = sorted({k for r in runs for k in r}, key=lambda k: -max(r.get(k, {}).get("end", 0) for r in runs))
names = [p.split("/")[-2].removesuffix(".logs")[-24:] for p in sys.argv[1:]]
print(f"{'step':<16}" + "".join(f"{n:>34}" for n in names))
for k in ids:
    cells = []
    for r in runs:
        s = r.get(k)
        cells.append(f"{s.get('start', 0)/1000:7.1f}s → {s.get('end', 0)/1000:6.1f}s ({s.get('ms', 0)/1000:5.1f}s)" if s else "-")
    print(f"{k:<16}" + "".join(f"{c:>34}" for c in cells))
