"""JUnit reports of a Next.js deploy run -> one jsonl line per test file, in the shape of the Railpack e2e results.

usage: results.py <logs dir> <out.jsonl> <test files...>
status is pass when every test of the file passed (skipped ones aside), fail otherwise; a file run-tests.js never
started (`skipDeployment`, filtered by the deploy manifest) is a skip; one that started but left no report (the
harness or the deploy crashed before jest reported) is a fail with its run-tests.log lines.
"""

import glob
import json
import os
import sys
import xml.etree.ElementTree as ET

logs, out, tests = sys.argv[1], sys.argv[2], sys.argv[3:]
by_file = {}
for path in glob.glob(os.path.join(logs, "*junit-report", "*.xml")):
    for suite in ET.parse(path).getroot().iter("testsuite"):
        for case in suite.iter("testcase"):
            f = case.get("file") or suite.get("file") or ""
            f = f[f.find("test/") :] if "test/" in f else f
            entry = by_file.setdefault(f, {"passed": 0, "failed": 0, "skipped": 0, "time": 0.0, "failures": []})
            entry["time"] += float(case.get("time") or 0)
            if case.find("skipped") is not None:
                entry["skipped"] += 1
            elif case.find("failure") is not None or case.find("error") is not None:
                entry["failed"] += 1
                node = case.find("failure") if case.find("failure") is not None else case.find("error")
                msg = (node.get("message") or node.text or "").strip().splitlines()
                entry["failures"].append(f"{case.get('name')}: {msg[0][:300] if msg else ''}")
            else:
                entry["passed"] += 1

run_log = ""
try:
    run_log = open(os.path.join(logs, "run-tests.log"), errors="replace").read()
except OSError:
    pass
with open(out, "w") as fh:
    for t in tests:
        e = by_file.get(t)
        if e is None:
            lines = [l for l in run_log.splitlines() if t in l][-5:]
            started = any(l.startswith(f"Starting {t}") for l in lines)
            row = {"example": t, "case": 0, "status": "fail" if started else "skip", "build_s": 0.0, "total_s": 0.0,
                   "detail": ("no test report: " if started else "not run in deploy mode: ") + " | ".join(lines)[:1500]}
        else:
            row = {"example": t, "case": 0, "status": "fail" if e["failed"] else ("pass" if e["passed"] else "skip"),
                   "build_s": 0.0, "total_s": round(e["time"], 1),
                   "detail": "; ".join(e["failures"])[:3000],
                   "tests": {k: e[k] for k in ("passed", "failed", "skipped")}}
        fh.write(json.dumps(row) + "\n")

rows = [json.loads(l) for l in open(out)]
tot = {k: sum(r.get("tests", {}).get(k, 0) for r in rows) for k in ("passed", "failed", "skipped")}
files = {s: sum(r["status"] == s for r in rows) for s in ("pass", "fail", "skip")}
builds = []
try:
    builds = [l.split("\t") for l in open(os.path.join(logs, "builds.tsv")) if l.strip()]
except OSError:
    pass
secs = sorted(float(b[1]) for b in builds)
mbs = sorted(float(b[2]) for b in builds)
med = lambda v: v[len(v) // 2] if v else 0
print(f"files: {files}; tests: {tot}; deployments: {len(builds)}, build median {med(secs)} s, image median {med(mbs)} MB")
