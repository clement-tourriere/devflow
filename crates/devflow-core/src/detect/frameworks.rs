//! Application frameworks and languages.
use super::facts::{Framework, FrameworkFact};
use super::manifest::{Discovery, Project};
use super::repo::Repo;

pub(super) fn detect(repo: &Repo, discovery: &Discovery) -> Vec<FrameworkFact> {
    let mut out: Vec<FrameworkFact> = Vec::new();
    for project in &discovery.projects {
        javascript(project, &mut out);
        python(repo, project, &mut out);
        rails(repo, project, &mut out);
        laravel(project, &mut out);
        phoenix(project, &mut out);
        spring(project, &mut out);
        language(project, &mut out);
    }
    out
}

fn push(
    out: &mut Vec<FrameworkFact>,
    project: &Project,
    framework: Framework,
    vite_based: bool,
    evidence: Vec<String>,
) {
    out.push(FrameworkFact {
        framework,
        path: project.path.clone(),
        vite_based,
        evidence,
    });
}

/// Evidence for a JS framework: dependencies and config files.
fn js_evidence(project: &Project, deps: &[&str], configs: &[&str]) -> Vec<String> {
    let mut evidence: Vec<String> = deps
        .iter()
        .filter_map(|d| project.js_deps.evidence(d))
        .collect();
    evidence.extend(
        configs
            .iter()
            .filter(|c| project.has_file(c))
            .map(|c| project.file(c)),
    );
    evidence
}

fn javascript(project: &Project, out: &mut Vec<FrameworkFact>) {
    if project.js.is_none() {
        return;
    }
    let vite = js_evidence(
        project,
        &["vite"],
        &[
            "vite.config.ts",
            "vite.config.js",
            "vite.config.mjs",
            "vite.config.mts",
            "vite.config.cjs",
            "vite.config.cts",
        ],
    );

    let next = js_evidence(
        project,
        &["next"],
        &[
            "next.config.js",
            "next.config.mjs",
            "next.config.ts",
            "next.config.cjs",
        ],
    );
    if !next.is_empty() {
        push(out, project, Framework::NextJs, false, next);
    }

    let mut vite_framework = false;
    let react_router = js_evidence(
        project,
        &["@react-router/dev"],
        &["react-router.config.ts", "react-router.config.js"],
    );
    if !react_router.is_empty() {
        vite_framework = true;
        push(out, project, Framework::ReactRouter, true, react_router);
    }

    let remix = js_evidence(
        project,
        &["@remix-run/dev", "@remix-run/react", "@remix-run/node"],
        &["remix.config.js"],
    );
    if !remix.is_empty() {
        vite_framework |= !vite.is_empty();
        push(out, project, Framework::Remix, !vite.is_empty(), remix);
    }

    let svelte = js_evidence(project, &["@sveltejs/kit"], &[]);
    if !svelte.is_empty() {
        vite_framework = true;
        push(out, project, Framework::SvelteKit, true, svelte);
    }

    let astro = js_evidence(
        project,
        &["astro"],
        &["astro.config.mjs", "astro.config.ts", "astro.config.js"],
    );
    if !astro.is_empty() {
        vite_framework = true;
        push(out, project, Framework::Astro, true, astro);
    }

    let nest = js_evidence(project, &["@nestjs/core"], &["nest-cli.json"]);
    if !nest.is_empty() {
        push(out, project, Framework::NestJs, false, nest);
    }

    if !vite.is_empty() && !vite_framework {
        push(out, project, Framework::Vite, true, vite);
    }
}

fn python(repo: &Repo, project: &Project, out: &mut Vec<FrameworkFact>) {
    if project.has_file("manage.py") {
        let manage = project.file("manage.py");
        let mentions_django = repo
            .read_head(&manage, 4096)
            .is_some_and(|head| head.to_ascii_lowercase().contains("django"));
        if mentions_django || project.py.has("django") {
            let mut evidence = vec![manage];
            evidence.extend(project.py.evidence("django"));
            push(out, project, Framework::Django, false, evidence);
        }
    }
    for (dep, framework) in [("fastapi", Framework::FastApi), ("flask", Framework::Flask)] {
        if let Some(evidence) = project.py.evidence(dep) {
            push(out, project, framework, false, vec![evidence]);
        }
    }
}

fn rails(repo: &Repo, project: &Project, out: &mut Vec<FrameworkFact>) {
    let bin_rails = project.has_dir("bin") && repo.is_file(&project.file("bin/rails"));
    let application =
        project.has_dir("config") && repo.is_file(&project.file("config/application.rb"));
    let gem = project.rb.evidence("rails");
    if gem.is_none() && !(bin_rails && application) {
        return;
    }
    let mut evidence: Vec<String> = gem.into_iter().collect();
    if bin_rails {
        evidence.push(project.file("bin/rails"));
    }
    if application {
        evidence.push(project.file("config/application.rb"));
    }
    push(out, project, Framework::Rails, false, evidence);
}

fn laravel(project: &Project, out: &mut Vec<FrameworkFact>) {
    let dependency = project.php.evidence("laravel/framework");
    if !project.has_file("artisan") && dependency.is_none() {
        return;
    }
    let mut evidence = Vec::new();
    if project.has_file("artisan") {
        evidence.push(project.file("artisan"));
    }
    evidence.extend(dependency);
    push(out, project, Framework::Laravel, false, evidence);
}

fn phoenix(project: &Project, out: &mut Vec<FrameworkFact>) {
    if let Some(evidence) = project.ex.evidence("phoenix") {
        push(out, project, Framework::Phoenix, false, vec![evidence]);
    }
}

fn spring(project: &Project, out: &mut Vec<FrameworkFact>) {
    let mut evidence = project.jvm.mentioning("spring-boot");
    for file in project.jvm.mentioning("org.springframework.boot") {
        if !evidence.contains(&file) {
            evidence.push(file);
        }
    }
    if !evidence.is_empty() {
        push(out, project, Framework::SpringBoot, false, evidence);
    }
}

/// Go modules and Cargo packages. A directory below an already reported
/// module of the same language is a member, not another project.
fn language(project: &Project, out: &mut Vec<FrameworkFact>) {
    for (framework, manifest, lock) in [
        (Framework::Go, "go.mod", "go.sum"),
        (Framework::Rust, "Cargo.toml", "Cargo.lock"),
    ] {
        if !project.has_file(manifest) {
            continue;
        }
        let nested = out.iter().any(|f| {
            f.framework == framework
                && (f.path == "." || project.path.starts_with(&format!("{}/", f.path)))
        });
        if nested {
            continue;
        }
        let mut evidence = vec![project.file(manifest)];
        if project.has_file(lock) {
            evidence.push(project.file(lock));
        }
        push(out, project, framework, false, evidence);
    }
}
