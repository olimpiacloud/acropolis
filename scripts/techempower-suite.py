#!/usr/bin/env python3
"""TechEmpower FrameworkBenchmarks -> Acropolis e2e suite.

  python3 scripts/techempower-suite.py                 clone (if missing), regenerate the suite, plan every example
  python3 scripts/techempower-suite.py plan [BIN]      only re-run `acropolis plan --json` and update manifest.json
  python3 scripts/techempower-suite.py report R.jsonl  join e2e results with manifest.json, print a markdown summary

The suite lands in $ACROPOLIS_EXT/techempower-suite (default ../acropolis-ext): one `<lang>-<framework>/` per
framework whose test declares json_url and/or plaintext_url, with every *.dockerfile/Dockerfile* removed, a
test.json with one httpCheck case and manifest.json with what the upstream dockerfile does (for diagnosis).
Planning uses $ACROPOLIS, else target/fast/acropolis (skipped if neither exists), with --home $ACROPOLIS_PLAN_HOME
(default /tmp/acropolis-plans-home). Run the suite with
  EXAMPLES=$ACROPOLIS_EXT/techempower-suite JOBS=3 scripts/e2e.sh
"""

import collections
import concurrent.futures
import fnmatch
import json
import os
import re
import shlex
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
EXT = os.path.abspath(os.environ.get("ACROPOLIS_EXT", os.path.join(HERE, "..", "..", "acropolis-ext")))
REPO = os.path.join(EXT, "FrameworkBenchmarks")
DST = os.path.join(EXT, "techempower-suite")
UPSTREAM = "https://github.com/TechEmpower/FrameworkBenchmarks"
PLAN_HOME = os.environ.get("ACROPOLIS_PLAN_HOME", "/tmp/acropolis-plans-home")

# TechEmpower language dir -> example-name prefix. Anything else is skipped as unsupported.
LANGUAGES = {
    "JavaScript": "javascript",
    "TypeScript": "typescript",
    "Go": "go",
    "Rust": "rust",
    "Python": "python",
    "Ruby": "ruby",
    "PHP": "php",
    "Java": "java",
    "Kotlin": "kotlin",
    "Groovy": "groovy",
    "CSharp": "csharp",
    "FSharp": "fsharp",
    "Elixir": "elixir",
    "Gleam": "gleam",
    "C++": "cpp",
    "C": "c",
}
JVM_BUILD = ("pom.xml", "build.gradle", "build.gradle.kts")
# Languages that only count as a supported stack with one of these build files at the framework root.
NEEDS_BUILD_FILE = {"Kotlin": JVM_BUILD, "Groovy": JVM_BUILD}
# Project manifests: when the dockerfile builds in a subdirectory that has one, test.json gets `directory`; a root
# without one (and no such subdirectory) is reported as a monorepo.
MANIFESTS = (
    "package.json", "deno.json", "deno.jsonc", "go.mod", "Cargo.toml", "requirements.txt", "pyproject.toml",
    "Pipfile", "Gemfile", "composer.json", "pom.xml", "build.gradle", "build.gradle.kts", "mix.exs",
    "gleam.toml", "CMakeLists.txt", "meson.build", "*.csproj", "*.fsproj", "index.php", "main.ts", "mod.ts",
)

# System packages that do not make an example static by design: build tools, fetch tools, allocator tuning and the
# language runtimes the providers install themselves.
BENIGN_PACKAGES = re.compile(
    r"^(git|curl|wget|ca-certificates|unzip|zip|xz-utils|bzip2|tar|gzip|build-essential|build-base|alpine-sdk|"
    r"gcc|g\+\+|make|cmake|clang|llvm|lld|pkg-config|pkgconf|pkgconfig|software-properties-common|gnupg2?|"
    r"lsb-release|apt-transport-https|apt-utils|locales|tzdata|procps|nano|vim|less|dumb-init|tini|bash|"
    r"musl-dev|musl-tools|libc6-dev|libc-dev|linux-headers|autoconf|automake|libtool|binutils|file|"
    r"libssl-dev|openssl-dev|openssl|libssl\d.*|libstdc\+\+|libgcc|ncurses-libs|libjemalloc\d*|libjemalloc-dev|"
    r"jemalloc|jemalloc-dev|libmimalloc.*|nodejs|npm|yarn|ruby.*|python3?(\.\d+)?|python3?-(pip|dev|venv)|"
    r"openjdk-.*|golang.*|default-jdk|default-jre|maven|gradle|dotnet.*|elixir|erlang.*|nginx-light|"
    r"php\d*(\.\d+)?-(cli|fpm|common|dev)|php\d*(\.\d+)?|php-pear|composer)$"
)
# PHP extensions the provider's base image ships (crates/acropolis-build/src/providers/php.rs `builtin`, plus
# opcache/mysqlnd which FrankenPHP bundles, and the database drivers, which only the db/query/fortune/update endpoints
# need). Anything else must be declared as ext-* in composer.json.
PHP_BUILTIN = set(
    "ctype curl dom fileinfo filter hash mbstring openssl pcre pdo session tokenizer xml json pdo_sqlite sqlite3 "
    "iconv libxml simplexml xmlreader xmlwriter phar posix readline sodium zlib spl standard date reflection "
    "opcache mysqlnd cli fpm common dev pear mysql mysqli pgsql pdo_mysql pdo_pgsql".split()
)
# PHP servers other than php-fpm/FrankenPHP, matched on the dockerfile RUN/CMD lines and the framework name.
PHP_VARIANTS = re.compile(
    r"swoole|openswoole|workerman|reactphp|react/http|amphp|\bamp\b|roadrunner|\brr serve|ngx_php|php-ngx|"
    r"\bunitd\b|adapterman|ripple|hyperf|webman",
    re.I,
)
PY_VARIANTS = re.compile(r"\buwsgi\b|meinheld|socketify|fastpysgi|bjoern|\bunitd\b|pypy|nginx", re.I)
FRONT_SERVERS = re.compile(r"\b(nginx|openresty|h2o|lighttpd|httpd|apache2|varnishd|haproxy)\b")
DB_AT_START = re.compile(r"\bmigrate\b|\bdb:(create|migrate|setup|prepare)|tfb-database|wait-for|pg_isready|dockerize")
VARIANT_FLAGS = re.compile(r"--features[ =](\"[^\"]*\"|\S+)|--no-default-features|-tags[ =](\"[^\"]*\"|\S+)|"
                           r"(?<=\s)-P\s*\S+|--profile[ =]\S+")


def slug(s):
    return re.sub(r"-+", "-", re.sub(r"[^a-z0-9]+", "-", s.lower())).strip("-")


def has_manifest(d):
    try:
        names = os.listdir(d)
    except OSError:
        return False
    return any(fnmatch.fnmatch(n, m) for n in names for m in MANIFESTS)


def instructions(text):
    """Dockerfile -> [(INSTRUCTION, args)], continuation lines joined, comments dropped."""
    out, cur = [], ""
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("#") or (not s and not cur):
            continue
        if s.endswith("\\"):
            cur += s[:-1] + " "
            continue
        cur += s
        m = re.match(r"(\S+)\s*(.*)", cur.strip(), re.S)
        if m:
            out.append((m.group(1).upper(), m.group(2).strip()))
        cur = ""
    return out


def build_dir(src, ins):
    """Source subdir the dockerfile builds in: the WORKDIR of a RUN, mapped back through COPY/ADD of directories."""
    wd, copies = "/", []
    for k, a in ins:
        if k == "FROM":
            wd, copies = "/", []
        elif k == "WORKDIR":
            wd = os.path.normpath(os.path.join(wd, a.strip("'\"")))
        elif k in ("COPY", "ADD") and "--from" not in a:
            try:
                args = [t for t in shlex.split(a.strip("[]").replace('","', '" "')) if not t.startswith("--")]
            except ValueError:
                continue
            dst = os.path.normpath(os.path.join(wd, args[-1])) if len(args) > 1 else None
            for s in args[:-1]:
                s = os.path.normpath(s.lstrip("/") or ".")
                if not s.startswith("..") and os.path.isdir(os.path.join(src, s)):
                    copies.append((dst, s))
        elif k == "RUN":
            for dst, s in reversed(copies):
                if wd == dst or wd.startswith(dst.rstrip("/") + "/"):
                    rel = os.path.normpath(os.path.join(s, os.path.relpath(wd, dst)))
                    if rel == ".":
                        return None
                    if has_manifest(os.path.join(src, rel)):
                        return rel
                    break
    return None


def install_args(run, managers):
    """Packages named by `<manager> install|add ...` in a RUN line."""
    pkgs = []
    for m in re.finditer(r"\b(?:%s)\s+((?:-\S+\s+)*)(?:install|add)\s+(.*?)(?=&&|;|\|\||\||>|<|$)" % managers, run):
        toks = m.group(2).split()
        skip = False
        for t in toks:
            if skip:
                skip = False
                continue
            if t in ("--virtual", "-t", "-o", "--repository", "-X", "--option"):
                skip = True
                continue
            if t.startswith("-") or not re.match(r"^[a-z0-9][\w.+:=<>-]*$", t, re.I):
                continue
            pkgs.append(re.split(r"[=<>]", t)[0])
    return pkgs


def static_reasons(lang, src, ins, test, cmd_text, composer):
    reasons = []
    runs = " && ".join(a for i, a in ins if i == "RUN")
    froms = " ".join(a for i, a in ins if i == "FROM").lower()
    everything = runs + " " + cmd_text
    pkgs = install_args(runs, r"apt-get|apt|apk|yum|dnf|microdnf")
    php_exts = []
    sys_pkgs = []
    for p in pkgs:
        m = re.fullmatch(r"php\d*(?:\.\d+)?-([a-z0-9_]+)", p)
        if lang == "PHP" and m:
            php_exts.append(m.group(1))
        elif not BENIGN_PACKAGES.match(p) and not (lang == "PHP" and p == "nginx"):
            sys_pkgs.append(p)
    if sys_pkgs:
        reasons.append("installs system packages: " + " ".join(sorted(set(sys_pkgs))))
    for m in re.finditer(r"(?:pecl install|docker-php-ext-install|docker-php-ext-enable|install-php-extensions)"
                         r"((?:\s+[\w.@-]+)+)", runs):
        php_exts += [re.split(r"[-@]\d", t)[0].rstrip("-") for t in m.group(1).split() if not t.startswith("-")]
    req = {k[4:].lower() for k in composer.get("require", {}) if k.startswith("ext-")} if composer else set()
    missing = sorted({e for e in php_exts if e not in PHP_BUILTIN and e not in req})
    if missing:
        reasons.append("installs PHP extensions not declared in composer.json: " + " ".join(missing))
    built = [label for label, rx in (("git clone", r"\bgit clone\b"), ("./configure", r"\./configure\b"),
                                     ("make install", r"\b(?:make|ninja) install\b"),
                                     ("downloads a tarball", r"\b(?:wget|curl)\b[^&;]*\.(?:tar|tgz)"))
             if re.search(rx, runs)]
    if built:
        reasons.append("builds dependencies from source (" + ", ".join(built) + ")")
    if "native-image" in everything or "graalvm" in froms and "native" in everything:
        reasons.append("GraalVM native-image")
    if lang == "PHP":
        m = PHP_VARIANTS.search(everything + " " + test["framework"])
        if m:
            reasons.append("PHP runtime variant: " + m.group(0).lower())
    if lang == "Python" and PY_VARIANTS.search(cmd_text + " " + froms):
        standard = any(os.path.exists(os.path.join(src, f)) for f in
                       ("Procfile", "manage.py", "main.py", "app.py", "start.py", "server.py", "hello.py", "bot.py"))
        if not standard:
            reasons.append("Python server variant without a standard entry: " + PY_VARIANTS.search(cmd_text + " " + froms).group(0))
    if lang != "PHP" and FRONT_SERVERS.search(cmd_text):
        reasons.append("needs a front web server: " + FRONT_SERVERS.search(cmd_text).group(0))
    if lang in ("C", "C++") and not any(os.path.exists(os.path.join(src, f)) for f in ("CMakeLists.txt", "meson.build")):
        reasons.append("no CMake/Meson build (custom build script)")
    if lang in ("Java", "Kotlin", "Groovy") and not any(os.path.exists(os.path.join(src, f)) for f in JVM_BUILD):
        reasons.append("no Maven/Gradle build at the root")
    m = DB_AT_START.search(cmd_text)
    if m:
        reasons.append("needs database at startup (likely): start command references " + m.group(0))
    django = any(os.path.exists(os.path.join(d, "manage.py")) for d in [src] + [
        os.path.join(src, e) for e in os.listdir(src) if os.path.isdir(os.path.join(src, e))])
    if (lang == "Python" and test.get("database", "None") != "None" and django
            and not os.path.exists(os.path.join(src, "Procfile"))):
        reasons.append("needs database at startup (likely): Django start runs manage.py migrate")
    return reasons


def generate():
    if not os.path.isdir(os.path.join(REPO, "frameworks")):
        subprocess.run(["git", "clone", "--depth", "1", "--filter=blob:none", "--sparse", UPSTREAM, REPO], check=True)
        subprocess.run(["git", "-C", REPO, "sparse-checkout", "set", "frameworks"], check=True)
    root = os.path.join(REPO, "frameworks")
    skipped = collections.defaultdict(list)
    unsupported = collections.Counter()
    manifest = {}
    shutil.rmtree(DST, ignore_errors=True)
    os.makedirs(DST)
    for lang in sorted(os.listdir(root)):
        ldir = os.path.join(root, lang)
        if not os.path.isdir(ldir):
            continue
        if lang not in LANGUAGES:
            unsupported[lang] += sum(os.path.isdir(os.path.join(ldir, f)) for f in os.listdir(ldir))
            continue
        for fw in sorted(os.listdir(ldir)):
            src = os.path.join(ldir, fw)
            if not os.path.isdir(src):
                continue
            cfg_path = os.path.join(src, "benchmark_config.json")
            if not os.path.exists(cfg_path):
                skipped["no benchmark_config.json"].append(f"{lang}/{fw}")
                continue
            cfg = json.load(open(cfg_path))
            tests = [(name, t) for entry in cfg["tests"] for name, t in entry.items()
                     if t.get("json_url") or t.get("plaintext_url")]
            if not tests:
                skipped["no json_url/plaintext_url"].append(f"{lang}/{fw}")
                continue
            # Prefer default, then tests not tagged broken upstream, in config order.
            name, test = min(tests, key=lambda nt: (nt[0] != "default", "broken" in nt[1].get("tags", []), tests.index(nt)))
            test = dict(test, framework=test.get("framework", cfg["framework"]))
            dockerfile = test.get("dockerfile") or (cfg["framework"] if name == "default" else f"{cfg['framework']}-{name}") + ".dockerfile"
            dpath = os.path.join(src, dockerfile)
            ins = instructions(open(dpath).read()) if os.path.exists(dpath) else []
            last_from = max((i for i, (k, _) in enumerate(ins) if k == "FROM"), default=0)
            final = ins[last_from:]
            cmd = [f"{k} {a}" for k, a in final if k == "ENTRYPOINT"][-1:] + [f"{k} {a}" for k, a in final if k == "CMD"][-1:]
            if test.get("docker_cmd"):
                cmd = [f"docker_cmd {test['docker_cmd']}"]
            cmd_text = " ".join(cmd)
            directory = build_dir(src, ins)
            app_dir = os.path.join(src, directory) if directory else src
            if lang in NEEDS_BUILD_FILE and not any(os.path.exists(os.path.join(app_dir, f)) for f in NEEDS_BUILD_FILE[lang]):
                skipped[f"{lang} without Gradle/Maven"].append(f"{lang}/{fw}")
                continue
            composer = {}
            if os.path.exists(os.path.join(app_dir, "composer.json")):
                try:
                    composer = json.load(open(os.path.join(app_dir, "composer.json")))
                except ValueError:
                    pass
            reasons = static_reasons(lang, app_dir, ins, test, cmd_text, composer)
            if not ins:
                reasons.append(f"dockerfile {dockerfile} not found")
            variants = sum(f.endswith(".dockerfile") for f in os.listdir(src))
            if not directory and not has_manifest(src):
                reasons.append(f"monorepo: no project manifest at the root and the {variants} dockerfile variants do "
                               "not map to one subdirectory" if variants > 1 else
                               "no project manifest a provider detects (the dockerfile builds a custom layout)")
            elif (variant := VARIANT_FLAGS.search(" ".join(a for k, a in ins if k == "RUN"))) and variants > 1:
                reasons.append("monorepo variant: build selects " + variant.group(0))
            port = int(test.get("port", 8080))
            checks = []
            for key, label in (("json_url", "json"), ("plaintext_url", "plaintext")):
                if test.get(key):
                    checks.append({"internalPort": port, "path": test[key], "expected": 200, "expectedOutput": "Hello, World!"})
            envs = {"PORT": str(port)}
            if lang == "PHP" and os.path.exists(os.path.join(app_dir, "artisan")):
                # The PHP provider runs `php artisan migrate --force` at startup; TFB has no database here.
                envs["ACROPOLIS_SKIP_MIGRATIONS"] = "true"
            case = {"httpCheck": checks, "envs": envs}
            if directory:
                case["directory"] = directory
            ex = f"{LANGUAGES[lang]}-{slug(fw)}"
            assert ex not in manifest, f"duplicate example name {ex}"
            # Upstream compose files build the app from its dockerfile; the e2e would start them as services.
            shutil.copytree(src, os.path.join(DST, ex), symlinks=True,
                            ignore=shutil.ignore_patterns("*.dockerfile", "Dockerfile*", "dockerfile",
                                                          "docker-compose.y*ml", "compose.y*ml"))
            with open(os.path.join(DST, ex, "test.json"), "w") as f:
                json.dump([case], f, indent=2)
                f.write("\n")
            manifest[ex] = {
                "language": lang,
                "framework": fw,
                "test": name,
                "port": port,
                "urls": {label: test[key] for key, label in (("json_url", "json"), ("plaintext_url", "plaintext")) if test.get(key)},
                "database": test.get("database"),
                "approach": test.get("approach"),
                "orm": test.get("orm"),
                "directory": directory,
                "dockerfile": dockerfile,
                "dockerfile_cmd": cmd_text or None,
                "upstream_broken": "broken" in test.get("tags", []),
                "static_by_design": "; ".join(reasons) or None,
            }
    write_manifest(manifest)
    per_lang = collections.Counter(m["language"] for m in manifest.values())
    print(f"{len(manifest)} examples in {DST} ({dict(sorted(per_lang.items()))})")
    print(f"skipped frameworks: {dict(skipped)}")
    print(f"unsupported languages (frameworks): {dict(unsupported)}")
    print(f"static_by_design: {sum(bool(m['static_by_design']) for m in manifest.values())}, "
          f"upstream broken: {sum(m['upstream_broken'] for m in manifest.values())}")
    binary = os.environ.get("ACROPOLIS") or os.path.join(HERE, "..", "target", "fast", "acropolis")
    if os.path.exists(binary):
        plan(binary)
    else:
        print(f"no acropolis binary at {binary}: skipping plan (set ACROPOLIS=...)")


def write_manifest(manifest):
    with open(os.path.join(DST, "manifest.json"), "w") as f:
        json.dump(dict(sorted(manifest.items())), f, indent=2)
        f.write("\n")


def load_manifest():
    return json.load(open(os.path.join(DST, "manifest.json")))


def plan_one(binary, ex, m):
    case = json.load(open(os.path.join(DST, ex, "test.json")))[0]
    d = os.path.join(DST, ex, case.get("directory", ""))
    args = [binary, "--home", PLAN_HOME, "plan", d, "--json"]
    for k, v in case.get("envs", {}).items():
        args += ["-e", f"{k}={v}"]
    try:
        p = subprocess.run(args, capture_output=True, text=True, timeout=120)
    except subprocess.TimeoutExpired:
        return ex, {"plan_provider": None, "plan_cmd": None, "plan_error": "plan timed out"}
    if p.returncode == 0:
        try:
            j = json.loads(p.stdout)
            img = j.get("image") or {}
            cmd = (img.get("entrypoint") or []) + (img.get("cmd") or [])
            return ex, {"plan_provider": j.get("provider"), "plan_cmd": shlex.join(cmd) or None, "plan_error": None}
        except ValueError:
            return ex, {"plan_provider": None, "plan_cmd": None, "plan_error": "plan printed invalid JSON"}
    lines = [l.strip() for l in p.stderr.splitlines() if l.strip()]
    err = next((l for l in lines if l.lower().startswith("error:")), lines[-1] if lines else f"exit {p.returncode}")
    return ex, {"plan_provider": None, "plan_cmd": None, "plan_error": err}


def plan(binary):
    binary = os.path.abspath(binary)
    manifest = load_manifest()
    with concurrent.futures.ThreadPoolExecutor(4) as pool:
        for ex, r in pool.map(lambda kv: plan_one(binary, *kv), list(manifest.items())):
            manifest[ex].update(r)
    write_manifest(manifest)
    by_lang = collections.defaultdict(lambda: [0, 0])
    errors = collections.Counter()
    for m in manifest.values():
        by_lang[m["language"]][m["plan_error"] is not None] += 1
        if m["plan_error"]:
            errors[re.sub(r"/[^\s:]+", "<path>", m["plan_error"])[:110]] += 1
    print(f"plan with {binary}:")
    print("| language | plans | plan errors |\n|---|---:|---:|")
    for lang, (ok, bad) in sorted(by_lang.items()):
        print(f"| {lang} | {ok} | {bad} |")
    print("plan errors:")
    for e, n in errors.most_common():
        print(f"  {n:3} {e}")


# Failure categories for `report`: first match on the result `detail` wins; `{0}` takes the first regex group.
# Edit freely: order matters (database markers in container logs beat the generic http buckets).
CATEGORIES = [
    ("infra (registry/disk/rate limit)", r"error class: infra|No space left on device|/var/lib/registry|429 Too Many|toomanyrequests"),
    ("detection: could not detect", r"could not detect how to build"),
    ("detection: no start command", r"no start command"),
    ("unsupported", r"\bunsupported\b|not supported"),
    ("database needed", r"tfb-database|ECONNREFUSED|[Cc]onnection refused|could not translate host name|getaddrinfo|"
                        r"UnknownHostException|SQLSTATE|Communications link failure|Can't connect to MySQL|"
                        r"[Dd]atabase.{0,40}(refused|unreachable|connect)"),
    ("timeout", r"timed out|[Tt]imeout|deadline exceeded"),
    ("build step failed: {0}", r"error: step ([\w.-]+)"),
    ("http: body mismatch", r"body missing"),
    ("http: wrong status {0}", r"http check \S+ wanted \d+, last status ([1-9]\d\d)"),
    ("http: no response (crashed or wrong port)", r"http check \S+ wanted \d+, last status"),
    ("run: docker error", r"docker run|docker: Error|docker pull"),
    ("build: config/user error", r"error class: (?:config|user)"),
    ("build: internal error", r"error class: internal"),
    ("other", r""),
]


def categorize(detail):
    for label, rx in CATEGORIES:
        m = re.search(rx, detail)
        if m:
            return label.format(*m.groups())
    return "other"


def short(detail):
    lines = [l.strip() for l in detail.splitlines() if l.strip()]
    pick = next((l for l in lines if l.startswith("error: step") or l.lower().startswith("error:")), None)
    pick = pick or next((l for l in lines if not l.startswith("error class:")), "")
    pick = re.sub(r"\s+", " ", pick)
    return (pick[:140] + "…" if len(pick) > 140 else pick).replace("|", "\\|")


def report(path):
    manifest = load_manifest()
    rows = {}
    for line in open(path):
        if line.strip():
            r = json.loads(line)
            rows[(r["example"], r.get("case"))] = r  # retries: last line wins
    rows = sorted(rows.values(), key=lambda r: r["example"])
    unknown = "?"
    lang_of = lambda r: manifest.get(r["example"], {}).get("language", unknown)
    totals = collections.defaultdict(collections.Counter)
    for r in rows:
        totals[lang_of(r)][r["status"]] += 1
    n = collections.Counter(r["status"] for r in rows)
    print(f"## TechEmpower suite: {os.path.basename(path)}\n")
    print(f"{n['pass']}/{len(rows)} pass, {n['fail']} fail, {n['skip']} skip "
          f"({len(manifest) - len({r['example'] for r in rows})} manifest examples without a result)\n")
    print("### Per language\n\n| language | pass | fail | skip | total | pass % |\n|---|---:|---:|---:|---:|---:|")
    for lang, c in sorted(totals.items()):
        t = sum(c.values())
        print(f"| {lang} | {c['pass']} | {c['fail']} | {c['skip']} | {t} | {100 * c['pass'] / t:.0f}% |")
    fails = [r for r in rows if r["status"] == "fail"]
    for r in fails:
        r["category"] = categorize(r.get("detail", ""))
        r["static"] = manifest.get(r["example"], {}).get("static_by_design")
        r["broken"] = manifest.get(r["example"], {}).get("upstream_broken", False)
    print("\n### Failures by category\n\n| category | failures | static by design | upstream broken | examples |\n|---|---:|---:|---:|---|")
    cats = collections.defaultdict(list)
    for r in fails:
        cats[r["category"]].append(r)
    for cat, rs in sorted(cats.items(), key=lambda kv: (-len(kv[1]), kv[0])):
        exs = ", ".join(r["example"] for r in rs[:8]) + (f", … (+{len(rs) - 8})" if len(rs) > 8 else "")
        print(f"| {cat} | {len(rs)} | {sum(bool(r['static']) for r in rs)} | {sum(r['broken'] for r in rs)} | {exs} |")
    kinds = collections.defaultdict(list)
    for r in fails:
        for reason in (r["static"] or "").split("; "):
            if reason:
                kinds[re.split(r"[:(]", reason)[0].strip()].append(r["example"])
    if kinds:
        print("\n### Static-by-design reasons among failures\n\n| reason | failures | examples |\n|---|---:|---|")
        for k, exs in sorted(kinds.items(), key=lambda kv: (-len(kv[1]), kv[0])):
            print(f"| {k} | {len(exs)} | {', '.join(exs[:8])}{', …' if len(exs) > 8 else ''} |")
    surprising = [r["example"] for r in rows if r["status"] == "pass" and manifest.get(r["example"], {}).get("static_by_design")]
    if surprising:
        print(f"\nPassed despite static_by_design ({len(surprising)}): {', '.join(surprising)}")
    print("\n### Failures\n\n| example | language | category | static by design | detail |\n|---|---|---|---|---|")
    for r in fails:
        print(f"| {r['example']} | {lang_of(r)} | {r['category']} | {(r['static'] or '').replace('|', '/')} | {short(r.get('detail', ''))} |")


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "report":
        if len(sys.argv) != 3:
            sys.exit("usage: techempower-suite.py report <results.jsonl>")
        report(sys.argv[2])
    elif len(sys.argv) > 1 and sys.argv[1] == "plan":
        plan(sys.argv[2] if len(sys.argv) > 2 else os.environ.get("ACROPOLIS") or os.path.join(HERE, "..", "target", "fast", "acropolis"))
    elif len(sys.argv) == 1:
        generate()
    else:
        sys.exit(__doc__)
