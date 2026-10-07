use crate::detect::Env;
use crate::plan::{Action, LayerFrom, Plan, Step};
use anyhow::{Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MISE_ENV: &[(&str, &str)] = &[
    ("MISE_DATA_DIR", "/mise"),
    ("MISE_CONFIG_DIR", "/mise"),
    ("MISE_CACHE_DIR", "/mise/cache"),
    ("MISE_GLOBAL_CONFIG_FILE", "/mise/config.toml"),
    ("MISE_TRUSTED_CONFIG_PATHS", "/app"),
];

fn step(id: &str, name: String, action: Action, deps: Vec<String>) -> Step {
    Step { id: id.to_string(), name, action, deps, hash: String::new() }
}

fn push_index(plan: &Plan) -> usize {
    plan.steps.iter().position(|s| s.id == "push").unwrap_or(plan.steps.len())
}

fn insert_before_push(plan: &mut Plan, s: Step) {
    let idx = push_index(plan);
    plan.steps.insert(idx, s);
}

fn add_push_dep(plan: &mut Plan, id: &str) {
    if let Some(push) = plan.steps.iter_mut().find(|s| s.id == "push")
        && !push.deps.iter().any(|d| d == id)
    {
        push.deps.push(id.to_string());
    }
}

fn unique_id(plan: &Plan, base: &str) -> String {
    let clean: String = base.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' }).collect();
    let mut id = clean.clone();
    let mut n = 2;
    while plan.step(&id).is_some() {
        id = format!("{clean}-{n}");
        n += 1;
    }
    id
}

fn debian_like(image: &str) -> bool {
    !(image.contains("distroless") || image.contains("alpine") || image.starts_with("caddy") || image == "scratch")
}

fn is_build_run(s: &Step) -> bool {
    matches!(s.action, Action::ImageRun { mount_app: true, .. }) && s.id != "native-libs"
}

fn work_image(plan: &Plan) -> (String, Vec<String>) {
    if let Some(Action::ImageRun { image, .. }) = plan.steps.iter().rev().filter(|s| is_build_run(s)).map(|s| &s.action).next()
        && debian_like(image)
    {
        let deps = if image.starts_with('@') { vec!["base".to_string()] } else { vec![] };
        return (image.clone(), deps);
    }
    match plan.step("base").map(|s| &s.action) {
        Some(Action::ResolveBase { image }) if image.starts_with('@') => ("@base".into(), vec!["base".into()]),
        Some(Action::ResolveBase { image }) if debian_like(image) => (image.clone(), vec![]),
        Some(Action::ResolveNodeBase { variant, .. }) if debian_like(variant) => ("@base".into(), vec!["base".into()]),
        _ => ("debian:bookworm-slim".into(), vec![]),
    }
}

fn user_env(env: &Env, only: Option<&[String]>) -> BTreeMap<String, String> {
    env.vars
        .iter()
        .filter(|(k, _)| !k.starts_with("ACRO_") && !k.starts_with("RAILPACK_"))
        .filter(|(k, _)| only.map(|o| o.iter().any(|n| n == *k)).unwrap_or(true))
        .map(|(k, v)| (k.clone(), crate::env_ref(k, v)))
        .collect()
}

fn ensure_source(plan: &mut Plan) -> String {
    if let Some(s) = plan.steps.iter().find(|s| matches!(s.action, Action::CopySource { .. })) {
        return s.id.clone();
    }
    let id = unique_id(plan, "source");
    plan.steps.insert(0, step(&id, "copy source".into(), Action::CopySource { exclude: vec![] }, vec![]));
    id
}

fn image_run(image: String, commands: Vec<String>, env: BTreeMap<String, String>, mount_app: bool, tools: Vec<String>, lowers: Vec<String>) -> Action {
    Action::ImageRun { image, commands, env, network: true, mount_app, after: None, tools, lowers }
}

pub fn apply_build_apt(plan: &mut Plan, env: &Env, needed_by_custom: bool) -> Result<Option<String>> {
    let Some(pkgs) = crate::providers::node::build_apt_packages(env) else { return Ok(None) };
    if plan.facts.contains_key("build-apt-packages") {
        return Ok(None);
    }
    let runs_in_image = plan.steps.iter().any(is_build_run);
    if !runs_in_image && !needed_by_custom {
        if plan.provider == "node" || plan.provider == "static" {
            plan.warnings.push(format!("buildAptPackages ({pkgs}) not applied: this build path runs on the host"));
            return Ok(None);
        }
        bail!("buildAptPackages ({pkgs}) is not supported for {} projects yet", plan.provider);
    }
    let (image, deps) = work_image(plan);
    let id = unique_id(plan, "build-apt");
    let cmd = format!(
        "{}; apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends {pkgs} >/dev/null && rm -rf /var/lib/apt/lists/*",
        crate::providers::ruby::APT_ARCHIVE_FIX
    );
    let s = step(&id, format!("apt-get install {pkgs} (build only)"), image_run(image.clone(), vec![cmd], BTreeMap::new(), false, vec![], vec![]), deps);
    let idx = plan.steps.iter().position(is_build_run).unwrap_or_else(|| push_index(plan));
    plan.steps.insert(idx, s);
    for s in plan.steps.iter_mut() {
        let build_run = is_build_run(s);
        if let Action::ImageRun { image: img, lowers, .. } = &mut s.action
            && build_run
            && *img == image
        {
            lowers.push(id.clone());
            s.deps.push(id.clone());
        }
    }
    plan.facts.insert("build-apt-packages".into(), pkgs);
    Ok(Some(id))
}

fn mise_spec(p: &str) -> String {
    let p = p.trim();
    if p.contains('@') { p.to_string() } else { format!("{p}@latest") }
}

pub fn apply_packages(plan: &mut Plan, env: &Env) -> Option<String> {
    let mut pkgs: Vec<String> = env.vars.get("ACRO_MISE_PACKAGES").map(|v| v.split_whitespace().map(|s| s.to_string()).collect()).unwrap_or_default();
    if let Some((v, _)) = env.config("PACKAGES") {
        pkgs.extend(v.split([' ', ',']).filter(|p| !p.is_empty()).map(mise_spec));
    }
    let mut seen = std::collections::BTreeSet::new();
    pkgs.retain(|p| seen.insert(p.clone()));
    if pkgs.is_empty() {
        return None;
    }
    let (image, mut deps) = work_image(plan);
    let tool_id = unique_id(plan, "mise");
    plan.steps.insert(0, step(&tool_id, "mise".into(), Action::Toolchain { tool: "mise".into(), spec: String::new(), parts: vec![] }, vec![]));
    deps.push(tool_id);
    let mut menv: BTreeMap<String, String> = MISE_ENV.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    menv.insert("PATH".into(), "/mise/shims:/usr/local/bin:${PATH}".into());
    menv.insert("MISE_YES".into(), "1".into());
    let mut commands = vec![
        format!(
            "{{ [ -e /etc/ssl/certs/ca-certificates.crt ] || {{ {}; apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends ca-certificates >/dev/null && rm -rf /var/lib/apt/lists/*; }}; }}",
            crate::providers::ruby::APT_ARCHIVE_FIX
        ),
        "mkdir -p /usr/local/bin /mise".to_string(),
        "cp /opt/acro/mise/bin/mise /usr/local/bin/mise".to_string(),
        "export PATH=\"/mise/shims:/usr/local/bin:$(echo \"$PATH\" | sed 's#/opt/acro/mise/bin:##')\"".to_string(),
    ];
    for p in &pkgs {
        commands.push(format!("/usr/local/bin/mise use -g {p}"));
    }
    commands.push("/usr/local/bin/mise reshim".into());
    commands.push("rm -rf /mise/cache /mise/downloads".into());
    let id = unique_id(plan, "packages");
    let s = step(&id, format!("mise use {}", pkgs.join(" ")), image_run(image.clone(), commands, menv, false, vec!["mise".into()], vec![]), deps);
    let idx = plan.steps.iter().position(is_build_run).unwrap_or_else(|| push_index(plan));
    plan.steps.insert(idx, s);
    for s in plan.steps.iter_mut() {
        let build_run = is_build_run(s);
        if let Action::ImageRun { image: img, lowers, env: senv, .. } = &mut s.action
            && build_run
            && *img == image
        {
            lowers.push(id.clone());
            s.deps.push(id.clone());
            for (k, v) in MISE_ENV {
                senv.insert(k.to_string(), v.to_string());
            }
            let path = senv.get("PATH").cloned().unwrap_or_else(|| "${PATH}".into());
            senv.insert("PATH".into(), format!("/mise/shims:{path}"));
        }
    }
    let layer = unique_id(plan, "layer-packages");
    insert_before_push(
        plan,
        step(
            &layer,
            "layer mise packages".into(),
            Action::Layer { dest: String::new(), from: LayerFrom::Upper { step: id.clone(), include: vec!["mise".into(), "usr/local/bin/mise".into(), "etc/ssl".into(), "usr/share/ca-certificates".into(), "etc/ca-certificates".into(), "etc/ca-certificates.conf".into()], exclude: vec![] } },
            vec![id.clone()],
        ),
    );
    add_push_dep(plan, &layer);
    plan.image.layers.insert(0, layer);
    for (k, v) in MISE_ENV {
        plan.image.env.retain(|(ek, _)| ek != k);
        plan.image.env.push((k.to_string(), v.to_string()));
    }
    match plan.image.env.iter_mut().find(|(k, _)| k == "PATH") {
        Some((_, v)) => *v = format!("/mise/shims:{v}"),
        None => plan.image.env.push(("PATH".into(), "/mise/shims:${PATH}".into())),
    }
    plan.facts.insert("packages".into(), pkgs.join(" "));
    Some(id)
}

fn translate_commands(cmds: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for c in cmds {
        match c {
            Value::String(s) if s == "..." => {}
            Value::String(s) => out.push(s.clone()),
            Value::Object(m) => {
                if let Some(p) = m.get("path").and_then(|v| v.as_str()) {
                    out.push(format!("export PATH=\"{p}:$PATH\""));
                } else if let Some(cmd) = m.get("cmd").and_then(|v| v.as_str()) {
                    out.push(cmd.to_string());
                } else if let (Some(src), Some(dest)) = (m.get("src").and_then(|v| v.as_str()), m.get("dest").and_then(|v| v.as_str())) {
                    let src = src.trim_start_matches("./");
                    let dest = dest.trim_start_matches("./");
                    if !matches!(src, "." | "") || !matches!(dest, "." | "") {
                        out.push(format!("mkdir -p \"$(dirname '{dest}')\" && cp -a '/app/{src}' '{dest}'"));
                    }
                } else if let Some(name) = m.get("name").and_then(|v| v.as_str())
                    && let Some(value) = m.get("value").and_then(|v| v.as_str())
                {
                    out.push(format!("export {name}='{}'", value.replace('\'', "'\\''")));
                }
            }
            _ => {}
        }
    }
    out
}

fn output_paths(outputs: &[Value]) -> Vec<String> {
    let mut paths = Vec::new();
    for o in outputs {
        let items: Vec<&str> = match o {
            Value::String(s) => vec![s.as_str()],
            Value::Object(m) => m.get("include").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default(),
            _ => vec![],
        };
        for p in items {
            if p == "..." {
                continue;
            }
            paths.push(match p.strip_prefix('/') {
                Some(abs) => abs.to_string(),
                None => format!("app/{}", p.trim_start_matches("./")),
            });
        }
    }
    paths
}

pub fn apply_custom_steps(plan: &mut Plan, env: &Env, build_apt: Option<&str>, packages: Option<&str>) -> Result<()> {
    let Some(raw) = env.vars.get("ACRO_CUSTOM_STEPS") else { return Ok(()) };
    let steps: Vec<Value> = serde_json::from_str(raw)?;
    if steps.is_empty() {
        return Ok(());
    }
    let referenced: Vec<String> = env
        .vars
        .get("ACRO_DEPLOY_INPUTS")
        .and_then(|r| serde_json::from_str::<Vec<Value>>(r).ok())
        .unwrap_or_default()
        .iter()
        .filter_map(|i| i.get("step").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();
    let mut included = Vec::new();
    for cfg in &steps {
        let name = cfg.get("name").and_then(|v| v.as_str()).unwrap_or("custom");
        let has_outputs = cfg.get("deployOutputs").and_then(|v| v.as_array()).map(|a| !output_paths(a).is_empty()).unwrap_or(false);
        if has_outputs || referenced.iter().any(|r| r == name) {
            included.push(cfg);
        } else {
            plan.warnings.push(format!("step {name:?} is skipped: nothing in the deploy image uses its output (add deployOutputs or a deploy input from it)"));
        }
    }
    if included.is_empty() {
        return Ok(());
    }
    let source = ensure_source(plan);
    let anchor = plan
        .steps
        .iter()
        .rev()
        .find(|s| matches!(s.action, Action::Run { .. }) || is_build_run(s))
        .map(|s| s.id.clone())
        .unwrap_or_else(|| source.clone());
    let (image, base_deps) = work_image(plan);
    let tools: Vec<(String, String)> = plan
        .steps
        .iter()
        .filter_map(|s| match &s.action {
            Action::Toolchain { tool, .. } if tool != "mise" => Some((tool.clone(), s.id.clone())),
            _ => None,
        })
        .collect();
    let lowers: Vec<String> = build_apt.into_iter().chain(packages).map(|s| s.to_string()).collect();
    let mut prev = anchor.clone();
    let mut custom_ids = Vec::new();
    for cfg in included {
        let name = cfg.get("name").and_then(|v| v.as_str()).unwrap_or("custom");
        let commands = translate_commands(cfg.get("commands").and_then(|v| v.as_array()).map(|a| a.as_slice()).unwrap_or(&[]));
        let only: Option<Vec<String>> = cfg.get("secrets").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect());
        let mut senv = user_env(env, only.as_deref());
        if let Some(vars) = cfg.get("variables").and_then(|v| v.as_object()) {
            for (k, v) in vars {
                senv.insert(k.clone(), v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string()));
            }
        }
        if packages.is_some() {
            for (k, v) in MISE_ENV {
                senv.insert(k.to_string(), v.to_string());
            }
            senv.insert("PATH".into(), "/mise/shims:${PATH}".into());
        }
        let id = unique_id(plan, &format!("custom-{name}"));
        let mut deps = vec![prev.clone(), source.clone()];
        deps.extend(base_deps.iter().cloned());
        deps.extend(lowers.iter().cloned());
        deps.extend(tools.iter().map(|(_, sid)| sid.clone()));
        deps.dedup();
        let commands = if commands.is_empty() { vec!["true".to_string()] } else { commands };
        let s = step(
            &id,
            format!("step {name}"),
            image_run(image.clone(), commands, senv, true, tools.iter().map(|(t, _)| t.clone()).collect(), lowers.clone()),
            deps,
        );
        insert_before_push(plan, s);
        if let Some(outputs) = cfg.get("deployOutputs").and_then(|v| v.as_array()) {
            let include = output_paths(outputs);
            if !include.is_empty() {
                let layer = unique_id(plan, &format!("layer-{id}"));
                insert_before_push(
                    plan,
                    step(
                        &layer,
                        format!("layer outputs of {name}"),
                        Action::Layer { dest: String::new(), from: LayerFrom::Upper { step: id.clone(), include, exclude: vec![] } },
                        vec![id.clone()],
                    ),
                );
                add_push_dep(plan, &layer);
                plan.image.layers.push(layer);
            }
        }
        add_push_dep(plan, &id);
        custom_ids.push(id.clone());
        prev = id;
    }
    for s in plan.steps.iter_mut() {
        if let Action::Layer { from: LayerFrom::WorkDir { .. } | LayerFrom::Paths { .. }, .. } = &s.action {
            for c in &custom_ids {
                if !s.deps.contains(c) {
                    s.deps.push(c.clone());
                }
            }
        }
    }
    plan.facts.insert("custom-steps".into(), custom_ids.join(" "));
    Ok(())
}

pub fn apply_deploy_inputs(plan: &mut Plan, env: &Env) -> Result<()> {
    let Some(raw) = env.vars.get("ACRO_DEPLOY_INPUTS") else { return Ok(()) };
    let inputs: Vec<Value> = serde_json::from_str(raw)?;
    for input in inputs {
        let include: Vec<String> = input.get("include").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
        if let Some(image) = input.get("image").and_then(|v| v.as_str()) {
            if include.is_empty() {
                bail!("deploy input from image {image} needs an include list");
            }
            let id = unique_id(plan, "layer-input-image");
            insert_before_push(plan, step(&id, format!("layer {} from {image}", include.join(", ")), Action::Layer { dest: String::new(), from: LayerFrom::Image { image: image.into(), include } }, vec![]));
            add_push_dep(plan, &id);
            plan.image.layers.push(id);
        } else if input.get("local").and_then(|v| v.as_bool()).unwrap_or(false) {
            let source = ensure_source(plan);
            let items: Vec<(String, String)> = include.iter().map(|p| {
                let p = p.trim_start_matches("./").to_string();
                (p.clone(), p)
            }).collect();
            let id = unique_id(plan, "layer-input-local");
            insert_before_push(plan, step(&id, format!("layer local {}", include.join(", ")), Action::Layer { dest: "app".into(), from: LayerFrom::Paths { items } }, vec![source]));
            add_push_dep(plan, &id);
            plan.image.layers.push(id);
        } else if let Some(from) = input.get("step").and_then(|v| v.as_str()) {
            let target = [from.to_string(), format!("custom-{from}")].into_iter().find(|id| matches!(plan.step(id).map(|s| &s.action), Some(Action::ImageRun { .. })));
            match target {
                Some(sid) => {
                    let include = output_paths(&[serde_json::json!({ "include": include })]);
                    let id = unique_id(plan, &format!("layer-input-{sid}"));
                    insert_before_push(plan, step(&id, format!("layer outputs of {sid}"), Action::Layer { dest: String::new(), from: LayerFrom::Upper { step: sid.clone(), include, exclude: vec![] } }, vec![sid]));
                    add_push_dep(plan, &id);
                    plan.image.layers.push(id);
                }
                None => plan.warnings.push(format!("deploy input from step {from} ignored: the step does not run in an image")),
            }
        }
    }
    Ok(())
}

pub fn apply_deploy_paths(plan: &mut Plan, env: &Env) {
    let Some(paths) = env.vars.get("ACRO_DEPLOY_PATHS").filter(|p| !p.is_empty()) else { return };
    match plan.image.env.iter_mut().find(|(k, _)| k == "PATH") {
        Some((_, v)) => *v = format!("{v}:{paths}"),
        None => plan.image.env.push(("PATH".into(), format!("${{PATH}}:{paths}"))),
    }
}

pub fn apply_all(plan: &mut Plan, env: &Env) -> Result<()> {
    let custom = env
        .vars
        .get("ACRO_CUSTOM_STEPS")
        .and_then(|r| serde_json::from_str::<Vec<Value>>(r).ok())
        .map(|steps| steps.iter().any(|c| c.get("deployOutputs").and_then(|v| v.as_array()).map(|a| !output_paths(a).is_empty()).unwrap_or(false)))
        .unwrap_or(false);
    let build_apt = apply_build_apt(plan, env, custom)?;
    let packages = apply_packages(plan, env);
    apply_custom_steps(plan, env, build_apt.as_deref(), packages.as_deref())?;
    apply_deploy_inputs(plan, env)?;
    apply_deploy_paths(plan, env);
    toposort(plan)?;
    plan.finalize();
    Ok(())
}

pub fn toposort(plan: &mut Plan) -> Result<()> {
    let mut placed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut pending: Vec<Step> = std::mem::take(&mut plan.steps);
    let known: std::collections::HashSet<String> = pending.iter().map(|s| s.id.clone()).collect();
    while !pending.is_empty() {
        let idx = pending
            .iter()
            .position(|s| s.deps.iter().all(|d| placed.contains(d) || !known.contains(d)))
            .ok_or_else(|| anyhow::anyhow!("plan has a dependency cycle around {}", pending[0].id))?;
        let s = pending.remove(idx);
        placed.insert(s.id.clone());
        plan.steps.push(s);
    }
    Ok(())
}

pub const NATIVE_DEBS: &str = "/app/.acro-runtime-debs";
pub const NATIVE_DEBS_FILE: &str = ".acro-runtime-debs";

pub fn scan_native_libs(root: &str, out: &str) -> String {
    format!(
        "{{ find {root} -name '*.so*' -type f 2>/dev/null | xargs -r ldd 2>/dev/null | awk '/=> \\//{{print $3}}' | sort -u | while read -r f; do dpkg -S \"$f\" 2>/dev/null || dpkg -S \"$(readlink -f \"$f\")\" 2>/dev/null || dpkg -S \"/usr$f\" 2>/dev/null; done | cut -d: -f1 | sort -u > {out}; true; }}"
    )
}

pub fn install_missing_debs(list: &str, skip: &[&str]) -> String {
    let skip = if skip.is_empty() { String::new() } else { format!("case \" {} \" in *\" $p \"*) continue;; esac; ", skip.join(" ")) };
    format!(
        "m=''; for p in $(cat {list} 2>/dev/null); do {skip}dpkg -s \"$p\" >/dev/null 2>&1 || m=\"$m $p\"; done; if [ -n \"$m\" ]; then echo \"installing runtime libraries:$m\"; {}; apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends $m >/dev/null && rm -rf /var/lib/apt/lists/*; fi",
        crate::providers::ruby::APT_ARCHIVE_FIX
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn commands() {
        let cmds = vec![json!("..."), json!({ "path": "/usr/games" }), json!("cowsay hi"), json!({ "src": ".", "dest": "." }), json!({ "cmd": "echo x" })];
        assert_eq!(translate_commands(&cmds), vec!["export PATH=\"/usr/games:$PATH\"", "cowsay hi", "echo x"]);
    }

    #[test]
    fn outputs() {
        let outs = vec![json!({ "include": ["/hello", "dist", "..."] }), json!("./build")];
        assert_eq!(output_paths(&outs), vec!["hello", "app/dist", "app/build"]);
    }

    #[test]
    fn topo() {
        let mut plan = crate::plan::PlanBuilder::new("x", "shell").plan;
        plan.steps = vec![
            step("b", "b".into(), Action::Push, vec!["a".into()]),
            step("a", "a".into(), Action::CopyBase, vec![]),
            step("c", "c".into(), Action::Push, vec!["b".into(), "missing".into()]),
        ];
        toposort(&mut plan).unwrap();
        let ids: Vec<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        plan.steps.push(step("d", "d".into(), Action::Push, vec!["e".into()]));
        plan.steps.push(step("e", "e".into(), Action::Push, vec!["d".into()]));
        assert!(toposort(&mut plan).is_err());
    }

    #[test]
    fn mise_specs() {
        assert_eq!(mise_spec("jq"), "jq@latest");
        assert_eq!(mise_spec("pipx:httpie"), "pipx:httpie@latest");
        assert_eq!(mise_spec("node@22"), "node@22");
    }
}
