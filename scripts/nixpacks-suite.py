import collections
import json
import os
import re
import shutil
import sys

repo = sys.argv[1] if len(sys.argv) > 1 else "/root/personal/acropolis-ext/nixpacks"
dst = sys.argv[2] if len(sys.argv) > 2 else "/root/personal/acropolis-ext/nixpacks-suite"
src = open(os.path.join(repo, "tests/docker_run_tests.rs")).read()
cases = collections.defaultdict(list)
skipped = collections.Counter()
for f in re.split(r"\n#\[tokio::test\]\n", src)[1:]:
    paths = re.findall(r'"\./examples/([^"]+)"', f)
    if not paths:
        skipped["no example"] += 1
        continue
    if re.search(r"postgres|mysql|network|add_hosts|nginx_host", f, re.I):
        skipped["needs a database or network"] += 1
        continue
    if "should_panic" in f or "is_err()" in f:
        skipped["expects a failure"] += 1
        continue
    expected = []
    for bang, body in re.findall(r"assert!\(\s*(!?)(.*?)\);", f, re.S):
        if not bang:
            expected += [s.encode().decode("unicode_escape") for s in re.findall(r'\.contains\(\s*"((?:[^"\\]|\\.)*)"\s*\)', body)]
    if not expected:
        skipped["no output assertion"] += 1
        continue
    envs = {}
    m = re.search(r'build_with_(?:build_time_)?env(?:_vars)?\(\s*"[^"]+",\s*vec!\[(.*?)\]', f, re.S)
    if m:
        for kv in re.findall(r'"([^"]+)"', m.group(1)):
            if "=" in kv:
                k, v = kv.split("=", 1)
                envs[k] = v
    for k, v in re.findall(r'\(\s*"([A-Z_][A-Z0-9_]*)"\.to_string\(\),\s*"([^"]*)"\.to_string\(\)', f):
        envs[k] = v
    case = {"expectedOutput": expected, "stderrAllowed": True}
    if envs:
        case["envs"] = envs
    cases[paths[0]].append(case)
shutil.rmtree(dst, ignore_errors=True)
os.makedirs(dst)
for ex, cs in cases.items():
    if os.path.isdir(os.path.join(repo, "examples", ex)):
        shutil.copytree(os.path.join(repo, "examples", ex), os.path.join(dst, ex), symlinks=True)
        json.dump(cs, open(os.path.join(dst, ex, "test.json"), "w"), indent=2)
print(f"{sum(len(c) for c in cases.values())} cases over {len(cases)} examples in {dst}; skipped {dict(skipped)}")
