"""Smoke suite from the Heroku getting-started apps and the Paketo samples.

Clones what is missing (depth 1) under $ACROPOLIS_EXT, copies each chosen app to
smoke-suite/<source>-<lang>-<name>/ with a test.json, writes smoke-suite/manifest.json and
records what `acropolis plan --json` detects (binary: $ACROPOLIS_BIN, default the newest
acropolis-ext/bin/acropolis-e2e-*; ACROPOLIS_BIN= skips planning).

Every expected string carries its evidence: a repo-relative file that must contain a needle
(the string itself unless given), so the generator fails if upstream changes under it.
"""

import glob
import json
import os
import shutil
import subprocess
import sys

ext = os.environ.get("ACROPOLIS_EXT", os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "acropolis-ext"))
ext = os.path.abspath(ext)
dst = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ext, "smoke-suite")
REPOS = {
    "heroku/python-getting-started": "heroku/python-getting-started",
    "heroku/ruby-getting-started": "heroku/ruby-getting-started",
    "heroku/java-getting-started": "heroku/java-getting-started",
    "heroku/php-getting-started": "heroku/php-getting-started",
    "heroku/go-getting-started": "heroku/go-getting-started",
    "heroku/node-js-getting-started": "heroku/node-js-getting-started",
    "paketo-buildpacks/samples": "paketo-samples",
}
PORT = {"PORT": "8080"}
PAKETO = "Powered By Paketo Buildpacks"


def out(text, evidence, needle=None):
    return (text, evidence, needle or text)


def http(path, *outs, expected=200):
    return {"path": path, "expected": expected, "outs": outs}


def ex(name, repo, path, language, checks=None, output=None, envs=None, notes="", by_design=None):
    return dict(name=name, repo=repo, path=path, language=language, checks=checks or [], output=output or [],
                envs={**PORT, **(envs or {})} if checks else (envs or {}), notes=notes, by_design=by_design)


H = "heroku/"
P = "paketo-buildpacks/samples"
EXAMPLES = [
    ex("heroku-python", H + "python-getting-started", ".", "python",
       [http("/", out("Python Getting Started on Heroku", "hello/templates/base.html"),
             out("Getting Started with Python on Heroku", "hello/templates/index.html"))],
       notes="Django + gunicorn (gunicorn.conf.py binds [::]:$PORT). `/` only renders a template (no DB); with DEBUG off "
       "the {% static %} tags need collectstatic's manifest (whitenoise CompressedManifestStaticFilesStorage). "
       "`/db/` needs a database plus `manage.py migrate` (commented release step): not checked."),
    ex("heroku-ruby", H + "ruby-getting-started", ".", "ruby",
       [http("/", out("Ruby Getting Started on Heroku", "app/views/layouts/application.html.erb"),
             out("Getting Started with Ruby on Heroku", "app/views/welcome/index.erb"))],
       envs={"RAILS_ENV": "production", "SECRET_KEY_BASE": "smoke-suite-secret-key-base"},
       notes="Rails 8.1 + puma; config/puma.rb binds '::' only with RAILS_ENV=production (otherwise ::1). "
       "assume_ssl=true so force_ssl does not redirect plain HTTP. `/` (welcome#index) does not touch the DB and "
       "ActiveRecord connects lazily, so no compose; `/widgets` needs Postgres (app.json addon) plus db:migrate: not checked."),
    ex("heroku-java", H + "java-getting-started", ".", "java",
       [http("/", out("Java Getting Started on Heroku", "src/main/resources/templates/fragments/layout.html"),
             out("Getting Started with Java on Heroku", "src/main/resources/templates/index.html"))],
       notes="Spring Boot 4 (Maven, Java 17 from system.properties); server.port=${PORT:5000}. The datasource defaults to "
       "a dummy JDBC URL and Hikari connects lazily, so `/` works without DB; `/database` (needs JDBC_DATABASE_URL) is not checked."),
    ex("heroku-php", H + "php-getting-started", ".", "php",
       [http("/", out("PHP Getting Started on Heroku", "views/header.html"),
             out("Getting Started with PHP on Heroku", "views/index.twig"))],
       envs={"ACROPOLIS_PHP_ROOT_DIR": "/app/web"},
       notes="Slim 4 + Twig front controller in web/index.php. Procfile `heroku-php-apache2 web/` sets the document "
       "root; ACROPOLIS_PHP_ROOT_DIR=/app/web is the Acropolis equivalent."),
    ex("heroku-go", H + "go-getting-started", ".", "go",
       [http("/", out("Go Getting Started on Heroku", "templates/header.tmpl.html"),
             out("Getting Started with Go on Heroku", "templates/index.tmpl.html"))],
       notes="gin; exits if $PORT is unset; loads templates/ and static/ relative to the working directory at runtime."),
    ex("heroku-node", H + "node-js-getting-started", ".", "node",
       [http("/", out("Node.js Getting Started on Heroku", "views/partials/header.ejs"),
             out("Getting Started on Heroku with Node.js", "views/pages/index.ejs"))],
       notes="Express 5 + EJS, npm; package.json engines: node 22.x||24.x||26.x, npm 12.x."),
    # Paketo: expectations mirror the upstream smoke tests (Serve(ContainSubstring(...)).OnPort(8080)).
    ex("paketo-go-mod", P, "go/mod", "go", [http("/", out(PAKETO, "go/mod/main.go"))],
       notes="gorilla/mux, go.mod with `go 1.15`; listens on :$PORT."),
    ex("paketo-go-no-imports", P, "go/no-imports", "go", [http("/", out(PAKETO, "go/no-imports/main.go"))],
       notes="Single main.go without go.mod; listens on :$PORT."),
    ex("paketo-node-no-package-manager", P, "nodejs/no-package-manager", "node",
       [http("/", out(PAKETO, "nodejs/no-package-manager/server.js"))],
       notes="server.js only, no package.json; PORT || 8080."),
    ex("paketo-node-npm", P, "nodejs/npm", "node", [http("/", out(PAKETO, "nodejs/npm/server.js"))],
       notes="Express 5, npm start = `echo start && node server.js`."),
    ex("paketo-node-yarn", P, "nodejs/yarn", "node", [http("/", out(PAKETO, "nodejs/yarn/server.js"))],
       notes="Express 5, yarn.lock (v1)."),
    ex("paketo-node-npm-native-modules", P, "nodejs/npm-with-native-modules", "node",
       [http("/", out("Hello, World!", "nodejs/npm-with-native-modules/hello_world/hello.cc"))],
       notes="Local `file:hello_world` dependency compiled with node-gyp (needs python, make, g++ at build)."),
    ex("paketo-node-vue-npm", P, "nodejs/vue-npm", "node", [http("/", out(PAKETO, "nodejs/vue-npm/public/index.html"))],
       notes="Vue 2 + @vue/cli-service; upstream runs the `build` script and then `vue-cli-service serve` (port 8080)."),
    ex("paketo-node-react-yarn", P, "nodejs/react-yarn", "node",
       [http("/", out(PAKETO, "nodejs/react-yarn/public/index.html"))],
       notes="Create React App (react-scripts 5) with yarn; start = react-scripts start (PORT)."),
    ex("paketo-node-angular-npm", P, "nodejs/angular-npm", "node",
       [http("/", out(PAKETO, "nodejs/angular-npm/src/index.html"))],
       notes="Angular 13 CLI; start = `ng serve --host 0.0.0.0 --port 8080` (hardcoded 8080)."),
    ex("paketo-python-pip", P, "python/pip", "python", [http("/", out(PAKETO, "python/pip/templates/index.html"))],
       notes="Flask + gunicorn from requirements.txt; Procfile `web: gunicorn server:app` (gunicorn binds $PORT)."),
    ex("paketo-python-pipenv", P, "python/pipenv", "python",
       [http("/", out(PAKETO, "python/pipenv/templates/index.html"))],
       notes="Pipfile + Pipfile.lock; Procfile `web: gunicorn server:app`."),
    ex("paketo-python-poetry", P, "python/poetry", "python",
       [http("/", out(PAKETO, "python/poetry/templates/index.html"))],
       notes="Poetry package-mode=false; Procfile `web: gunicorn server:app`."),
    ex("paketo-python-poetry-run", P, "python/poetry-run", "python",
       [http("/", out(PAKETO, "python/poetry-run/default_app/templates/index.html"))],
       notes="No Procfile: the start command is the only [tool.poetry.scripts] entry `my-script` "
       "(default_app.server:run, reads PORT); Paketo's Poetry Run buildpack picks it up."),
    ex("paketo-python-uv", P, "python/uv", "python", [http("/", out(PAKETO, "python/uv/templates/index.html"))],
       notes="uv.lock + pyproject; Procfile `web: gunicorn server:app`."),
    ex("paketo-python-no-package-manager", P, "python/no_package_manager", "python",
       [http("/", out(PAKETO, "python/no_package_manager/web/index.html"))],
       notes="Stdlib http.server on a hardcoded port 8080 (PORT is ignored); Procfile `web: python server.py`."),
    ex("paketo-ruby-puma", P, "ruby/puma", "ruby", [http("/", out(PAKETO, "ruby/puma/app.rb"))],
       notes="Sinatra 4 + puma via config.ru."),
    ex("paketo-ruby-rackup", P, "ruby/rackup", "ruby", [http("/", out(PAKETO, "ruby/rackup/config.ru"))],
       notes="Plain Rack app with rackup + webrick."),
    ex("paketo-ruby-thin", P, "ruby/thin", "ruby", [http("/", out(PAKETO, "ruby/thin/app.rb"))],
       notes="Sinatra 4 + thin (config.ru `#\\ -s thin`)."),
    ex("paketo-ruby-unicorn", P, "ruby/unicorn", "ruby", [http("/", out(PAKETO, "ruby/unicorn/app.rb"))],
       notes="Sinatra 4 + unicorn gem; no unicorn config file."),
    ex("paketo-ruby-passenger", P, "ruby/passenger", "ruby", [http("/", out(PAKETO, "ruby/passenger/views/index.erb"))],
       notes="Sinatra 4 + passenger gem (Passengerfile.json engine builtin); no puma/rackup gem."),
    ex("paketo-ruby-rails-assets", P, "ruby/rails_assets", "ruby",
       [http("/", out(PAKETO, "ruby/rails_assets/app/views/welcome/index.html.erb"))],
       envs={"RAILS_ENV": "production", "SECRET_KEY_BASE": "some-key-base"},
       notes="Rails 8.1 without ActiveRecord (sprockets + importmap); SECRET_KEY_BASE as in the upstream smoke test."),
    ex("paketo-ruby-rake", P, "ruby/rake", "ruby",
       output=[out("I am the main rake task", "ruby/rake/Rakefile")],
       notes="Gemfile + Rakefile only; Paketo's Rake buildpack runs `bundle exec rake` (default task) and exits."),
    ex("paketo-php-builtin-server", P, "php/builtin-server", "php",
       [http("/", out(PAKETO, "php/builtin-server/htdocs/index.php"))], envs={"ACROPOLIS_PHP_ROOT_DIR": "/app/htdocs"},
       notes="index.php in htdocs/ (Paketo's default BP_PHP_WEB_DIR, also set in project.toml); "
       "ACROPOLIS_PHP_ROOT_DIR=/app/htdocs is the Acropolis equivalent."),
    ex("paketo-php-composer", P, "php/composer", "php", [http("/", out(PAKETO, "php/composer/htdocs/index.php"),
       out("Powered By Paketo Buildpacks</p>", "php/composer/htdocs/.env", 'PROJECT_NAME="Paketo"'))],
       envs={"ACROPOLIS_PHP_ROOT_DIR": "/app/htdocs"},
       notes="composer.json (vlucas/phpdotenv); htdocs/index.php requires ../vendor/autoload.php and prints PROJECT_NAME "
       "from htdocs/.env, so the .env file must be served to PHP (not hidden from the image)."),
    ex("paketo-php-composer-extensions", P, "php/composer_with_extensions", "php",
       [http("/", out(PAKETO, "php/composer_with_extensions/index.php"),
             *[out(e, "php/composer_with_extensions/composer.json", f'"ext-{e}"') for e in ("fileinfo", "gd", "mysqli", "zip")])],
       notes="composer.json requires ext-zip, ext-gd, ext-fileinfo, ext-mysqli, ext-mbstring; index.php (root) lists "
       "get_loaded_extensions()."),
    ex("paketo-php-httpd", P, "php/httpd", "php", [http("/", out(PAKETO, "php/httpd/htdocs/index.php"))],
       envs={"ACROPOLIS_PHP_ROOT_DIR": "/app/htdocs"},
       notes="Same app as builtin-server; project.toml BP_PHP_SERVER=httpd is Paketo-only (Acropolis uses its own server)."),
    ex("paketo-php-nginx", P, "php/nginx", "php", [http("/", out(PAKETO, "php/nginx/htdocs/index.php"))],
       envs={"ACROPOLIS_PHP_ROOT_DIR": "/app/htdocs"},
       notes="Same app as builtin-server; project.toml BP_PHP_SERVER=nginx is Paketo-only (Acropolis uses its own server)."),
    ex("paketo-dotnet-aspnet", P, "dotnet-core/aspnet", "dotnet",
       [http("/", out(PAKETO, "dotnet-core/aspnet/Startup.cs"))],
       notes="ASP.NET Core net8.0 (aspnet.csproj); the app does not read PORT, so the image has to map it (ASPNETCORE_URLS)."),
    ex("paketo-dotnet-runtime", P, "dotnet-core/runtime", "dotnet",
       [http("/", out(PAKETO, "dotnet-core/runtime/Program.cs"))],
       notes="Console app (net8.0) with a raw TcpListener on int.Parse($PORT)."),
    ex("paketo-dotnet-fdd", P, "dotnet-core/fdd-app", "dotnet",
       [http("/", out(PAKETO, "dotnet-core/dotnet_test.go"))],
       notes="Prebuilt framework-dependent deployment (aspnet.dll + runtimeconfig.json, no project file); "
       "string from the upstream smoke test (UTF-16 inside the dll).",
       by_design="prebuilt binaries without a .csproj/.fsproj: Acropolis (like Railpack) builds .NET from sources"),
    ex("paketo-dotnet-fde", P, "dotnet-core/fde-app", "dotnet",
       [http("/", out(PAKETO, "dotnet-core/dotnet_test.go"))],
       notes="Prebuilt framework-dependent executable (apphost `aspnet` + aspnet.dll, no project file); "
       "string from the upstream smoke test.",
       by_design="prebuilt binaries without a .csproj/.fsproj: Acropolis (like Railpack) builds .NET from sources"),
    ex("paketo-java-maven", P, "java/maven", "java",
       [http("/", out(PAKETO, "java/maven/src/main/java/io/paketo/demo/HomeController.java")),
        http("/actuator/health", out("UP", "java/maven/pom.xml", "spring-boot-starter-actuator"))],
       notes="Spring Boot (webflux + actuator), mvnw; Netty on the default 8080."),
    ex("paketo-java-gradle", P, "java/gradle", "java",
       [http("/actuator/health", out("UP", "java/gradle/build.gradle", "spring-boot-starter-actuator"))],
       notes="Spring Boot 4.1 (webflux + actuator), gradlew; no controller, so only the actuator endpoint answers 200. "
       "Upstream sets BP_GRADLE_BUILT_ARTIFACT=build/libs/*-SNAPSHOT.jar (bootJar + plain jar both exist)."),
    ex("paketo-java-kotlin", P, "java/kotlin", "java",
       [http("/actuator/health", out("UP", "java/kotlin/build.gradle.kts", "spring-boot-starter-actuator"))],
       notes="Kotlin on Gradle (build.gradle.kts), Spring Boot 4.1 webflux + actuator; no controller."),
    ex("paketo-java-dist-zip", P, "java/dist-zip", "java",
       [http("/actuator/health", out("UP", "java/dist-zip/build.gradle", "spring-boot-starter-actuator"))],
       notes="Spring Boot on Gradle; upstream builds `bootDistZip` via BP_GRADLE_BUILD_ARGUMENTS, a plain bootJar works too."),
    ex("paketo-java-war-spring", P, "java/war-spring", "java",
       [http("/actuator/health", out("UP", "java/war-spring/pom.xml", "spring-boot-starter-actuator"))],
       notes="Spring Boot <packaging>war</packaging> with SpringBootServletInitializer; the war is executable with "
       "`java -jar` (upstream deploys it on Tomcat)."),
    ex("paketo-java-gradle-node", P, "java/java-node/gradle-node", "java",
       [http("/actuator/health", out("UP", "java/java-node/gradle-node/build.gradle", "spring-boot-starter-actuator")),
        http("/", out('<script type="module" crossorigin src="/assets/index-',
                      "java/java-node/gradle-node/smoke_test/gradle_node_test.go", "/assets/index-"))],
       notes="Gradle Exec tasks run `npm --prefix ./frontend install/run build` (vite) before bootJar: the build image "
       "needs Node+npm (upstream: BP_JAVA_INSTALL_NODE=true). Script tag shape from the upstream smoke test."),
    ex("paketo-java-maven-yarn", P, "java/java-node/maven-yarn", "java",
       [http("/actuator/health", out("UP", "java/java-node/maven-yarn/pom.xml", "spring-boot-starter-actuator")),
        http("/", out('<script type="module" crossorigin src="/assets/index-',
                      "java/java-node/maven-yarn/smoke_test/maven_node_test.go", "/assets/index-"))],
       notes="exec-maven-plugin runs `yarn install` and `yarn build` (vite) during package: the build image needs Node+yarn "
       "(upstream: BP_JAVA_INSTALL_NODE=true)."),
    ex("paketo-static-httpd", P, "web-servers/httpd-sample", "static",
       [http("/", out(PAKETO, "web-servers/httpd-sample/htdocs/index.html"))],
       envs={"ACROPOLIS_STATIC_FILE_ROOT": "htdocs"},
       notes="httpd.conf DocumentRoot ${APP_ROOT}/htdocs; ACROPOLIS_STATIC_FILE_ROOT=htdocs is the Acropolis equivalent."),
    ex("paketo-static-nginx", P, "web-servers/nginx-sample", "static",
       [http("/", out(PAKETO, "web-servers/nginx-sample/public/index.html"))],
       envs={"ACROPOLIS_STATIC_FILE_ROOT": "public"},
       notes="nginx.conf `root public;`; ACROPOLIS_STATIC_FILE_ROOT=public is the Acropolis equivalent."),
    ex("paketo-static-no-config", P, "web-servers/no-config-file-sample/app", "static",
       [http("/", out(PAKETO, "web-servers/no-config-file-sample/app/public/index.html"))],
       envs={"ACROPOLIS_STATIC_FILE_ROOT": "public"},
       notes="Only the app/ subdir (binding/ is for the basic-auth variant). Paketo serves public/ by default when "
       "BP_WEB_SERVER is set; ACROPOLIS_STATIC_FILE_ROOT=public is the equivalent."),
    ex("paketo-static-react-frontend", P, "web-servers/react-frontend-sample", "node",
       [http("/", out("<title>Paketo Buildpacks</title>", "web-servers/react-frontend-sample/public/index.html"))],
       notes="Create React App (npm) built and served as static files (upstream: BP_NODE_RUN_SCRIPTS=build, "
       "BP_WEB_SERVER_ROOT=build, push state)."),
    ex("paketo-procfile-go", P, "procfile/procfile-sample", "go",
       [http("/hello-world.txt", out("Hello World!", "procfile/procfile-sample/web/hello-world.txt"))],
       notes="Procfile `web: go run main.go`, no go.mod; plan.toml (Paketo-only) requires go at launch."),
]
SKIPPED = {
    "ca-certificates/ca-certificates-sample": "buildpack demo: CA certificates through service bindings",
    "git/git-sample": "buildpack demo: git credentials binding and $REVISION",
    "java/akka": "sbt/Scala: unsupported stack",
    "java/deps": "Clojure (deps.edn): unsupported stack",
    "java/leiningen": "Clojure (Leiningen): unsupported stack",
    "java/tools-build": "Clojure (tools.build): unsupported stack",
    "java/application-insights": "buildpack demo: Azure Application Insights agent via service binding",
    "java/opentelemetry": "buildpack demo: OpenTelemetry agent via BP_OPENTELEMETRY_ENABLED and bindings",
    "java/aspectj": "buildpack demo: AspectJ weaver agent wired by the buildpack from aop.xml",
    "java/jar": "prebuilt exploded Spring Boot jar (26 MB, no sources or build file)",
    "java/war": "plain Jakarta EE war: needs a servlet container (upstream adds Tomcat)",
    "java/native-image": "GraalVM native-image builds driven by BP_NATIVE_IMAGE and BP_* build arguments",
    "python/conda": "conda environment.yml: unsupported package manager",
    "python/pixi": "pixi: unsupported package manager",
    "php/app_with_extensions": "extensions only via the buildpack's .php.ini.d snippets",
    "php/memcached_session_handler": "needs a memcached service binding (SERVICE_BINDING_ROOT)",
    "php/redis_session_handler": "needs a redis service binding (SERVICE_BINDING_ROOT)",
    "dotnet-core/self-contained-deployment": "prebuilt self-contained deployment (90 MB of binaries, no project file)",
    "web-servers/angular-nginx-sample": "identical to nodejs/angular-npm (only README differs)",
}


def git(*args, cwd=None):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


revs = {}
for repo, local in REPOS.items():
    path = os.path.join(ext, local)
    if not os.path.isdir(path):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        subprocess.run(["git", "clone", "-q", "--depth", "1", f"https://github.com/{repo}", path], check=True)
    revs[repo] = git("rev-parse", "HEAD", cwd=path)


def ignore(_dir, names):
    return [n for n in names if n in (".git", ".github") or n == "Dockerfile" or n.startswith("Dockerfile.") or n.endswith(".Dockerfile")]


def binary():
    if "ACROPOLIS_BIN" in os.environ:
        return os.environ["ACROPOLIS_BIN"] or None
    found = sorted(glob.glob(os.path.join(ext, "bin", "acropolis-e2e-*")), key=os.path.getmtime)
    return found[-1] if found else None


acropolis = binary()
plan_home = os.environ.get("ACROPOLIS_PLAN_HOME", "/tmp/acropolis-plans-home")
shutil.rmtree(dst, ignore_errors=True)
os.makedirs(dst)
manifest = {}
for e in EXAMPLES:
    root = os.path.join(ext, REPOS[e["repo"]])
    for text, evidence, needle in [o for c in e["checks"] for o in c["outs"]] + e["output"]:
        with open(os.path.join(root, evidence), errors="replace") as f:
            if needle not in f.read():
                sys.exit(f"{e['name']}: {needle!r} not found in {evidence} (expected {text!r})")
    target = os.path.join(dst, e["name"])
    shutil.copytree(os.path.join(root, e["path"]), target, symlinks=True, ignore=ignore)
    case = {}
    if e["checks"]:
        checks = [{"internalPort": 8080, "path": c["path"], "expected": c["expected"],
                   "expectedOutput": [o[0] for o in c["outs"]]} for c in e["checks"]]
        case["httpCheck"] = checks[0] if len(checks) == 1 else checks
    else:
        case["expectedOutput"] = [o[0] for o in e["output"]]
    if e["envs"]:
        case["envs"] = e["envs"]
    with open(os.path.join(target, "test.json"), "w") as f:
        json.dump([case], f, indent=2)
        f.write("\n")
    entry = {
        "source": e["repo"].split("/")[0].split("-")[0],
        "upstream": f"https://github.com/{e['repo']}",
        "upstream_rev": revs[e["repo"]],
        "upstream_path": e["path"],
        "language": e["language"],
        "notes": e["notes"],
        "evidence": sorted({o[1] for c in e["checks"] for o in c["outs"]} | {o[1] for o in e["output"]}),
        "by_design": e["by_design"],
    }
    if acropolis:
        cmd = [acropolis, "--home", plan_home, "plan", target, "--json"]
        for k, v in e["envs"].items():
            cmd += ["-e", f"{k}={v}"]
        p = subprocess.run(cmd, capture_output=True, text=True)
        if p.returncode == 0:
            plan = json.loads(p.stdout)
            entry["provider"] = plan.get("provider")
            entry["start"] = " ".join((plan.get("image") or {}).get("cmd") or []) or None
        else:
            lines = (p.stderr + p.stdout).splitlines()
            entry["provider"] = None
            entry["plan_error"] = next((l for l in lines if l.startswith("error:")), lines[0] if lines else f"exit {p.returncode}")
    manifest[e["name"]] = entry
manifest = {"examples": manifest, "skipped": {f"{P}/{k}": v for k, v in sorted(SKIPPED.items())}}
with open(os.path.join(dst, "manifest.json"), "w") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
failed = [n for n, m in manifest["examples"].items() if m.get("plan_error")]
print(f"{len(EXAMPLES)} examples in {dst}; {len(SKIPPED)} upstream samples skipped; plan errors: {failed or 'none'}")
for n, m in manifest["examples"].items():
    print(f"  {n}: {m.get('provider')}  {m.get('plan_error') or m.get('start')}")
