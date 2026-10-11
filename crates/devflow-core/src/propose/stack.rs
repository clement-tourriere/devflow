//! The commands of the project's own stack: dependency installs, code
//! generation, migrations and the `[env]` presets that point frameworks at the
//! workspace database.
//!
//! Commands are built from the argument vectors and install commands the
//! facts layer provides, never from text read out of the repository. A
//! script or task *name* found in the repository becomes one quoted word.
use super::compose::ComposeHook;
use super::hooks::{Purpose, slug};
use super::plan::{Builder, Pushed, Spec};
use super::{Confidence, ItemKind, PatchOp};
use crate::config::EnvPreset;
use crate::detect::{
    Engine, EnvCategory, Framework, MigrationFact, MigrationTool, PackageManager,
    PackageManagerFact, PackageManagerSource, PythonProject, PythonTool, shell_quote,
};
use serde_json::{Value, json};

/// One hook command to propose.
pub(super) struct Step {
    pub id: String,
    pub name: String,
    pub run: String,
    pub title: String,
    pub confidence: Confidence,
    pub evidence: Vec<String>,
    pub risks: Vec<String>,
    pub purpose: Purpose,
    pub depends_on: Vec<String>,
}

impl Builder<'_> {
    /// Add `step` to the hooks of `phase` unless the configuration already
    /// does the same job.
    pub fn hook(&mut self, phase: &'static str, step: Step) -> Pushed {
        self.hook_with(phase, step, None)
    }

    /// [`Self::hook`] for a compose hook, whose `value.compose` lets a
    /// selection render it again.
    pub fn hook_with(
        &mut self,
        phase: &'static str,
        step: Step,
        compose: Option<&ComposeHook>,
    ) -> Pushed {
        if self.hook_covered(phase, &step.purpose, &step.run) {
            self.satisfied.push(step.id);
            return Pushed::Satisfied;
        }
        let file = self.devflow_file();
        let id = step.id.clone();
        // A command run through mise has mise's `[env]` applied over the
        // variables devflow gives it. Commands built here go through
        // `mise exec -- devflow exec -- CMD`, where devflow sets them last,
        // so they need nothing of mise's configuration. A mise *task* cannot
        // be wrapped (its own `env` is applied after anything set before it),
        // and only gets the workspace's database when mise loads
        // `.devflow/env`: those stay tied to the wiring item.
        let relies_on_mise_env = relies_on_mise_env(&step.run);
        let mut value = json!({"phase": phase, "name": step.name, "run": step.run});
        if let Some(compose) = compose {
            value["compose"] = serde_json::to_value(compose).unwrap_or(Value::Null);
        }
        let spec = Spec::new(step.id, ItemKind::Hook, file, step.title)
            .value(value)
            .confidence(step.confidence)
            .evidence(step.evidence)
            .risks(step.risks)
            .depends_on(step.depends_on)
            .ops(vec![PatchOp::Append {
                path: vec!["hooks".into(), phase.into()],
                values: vec![json!({"name": step.name, "run": step.run})],
                front: false,
            }]);
        let pushed = self.push(spec);
        if relies_on_mise_env && pushed == Pushed::Item {
            self.mise_env_users.push(id);
        }
        pushed
    }

    /// Whether commands run through `mise exec --` (mise pins tools).
    pub fn mise_exec(&self) -> bool {
        self.facts
            .toolchain
            .mise
            .as_ref()
            .is_some_and(|mise| !mise.tools.is_empty())
    }

    /// `command`, run from `dir`, through mise when it pins tools, and then
    /// through `devflow exec`: mise runs first and sets its `[env]`, and
    /// devflow sets the workspace's variables over it, so what a `mise.toml`
    /// of the directory (or `MISE_ENV`, or a global setting) says about
    /// `DATABASE_URL` cannot win. The `cd` is safe for the same reason.
    pub fn wrap(&self, dir: &str, command: &str) -> String {
        let command = if self.mise_exec() {
            format!("mise exec -- devflow exec -- {command}")
        } else {
            command.to_string()
        };
        if dir == "." || dir.is_empty() {
            command
        } else {
            format!("cd {} && {command}", shell_quote(dir))
        }
    }

    /// The package manager that installs the JavaScript project holding `path`.
    fn js_pm(&self, path: &str) -> Option<&PackageManagerFact> {
        self.facts
            .js
            .iter()
            .filter(|project| {
                project.path == "."
                    || project.path == path
                    || path.starts_with(&format!("{}/", project.path))
            })
            .max_by_key(|project| project.path.len())
            .map(|project| &project.package_manager)
    }

    /// The Python project whose environment runs code in `path`.
    fn py_project(&self, path: &str) -> Option<&PythonProject> {
        self.facts
            .python
            .iter()
            .filter(|project| {
                project.path == "."
                    || project.path == path
                    || path.starts_with(&format!("{}/", project.path))
            })
            .max_by_key(|project| project.path.len())
    }

    fn py_prefix(&self, path: &str) -> &'static str {
        match self.py_project(path).map(|p| p.tool) {
            Some(PythonTool::Uv) => "uv run ",
            Some(PythonTool::Poetry) => "poetry run ",
            _ => "",
        }
    }

    // -- installs --------------------------------------------------------------------

    pub fn installs(&mut self) {
        let facts = self.facts;
        for project in &facts.js {
            let pm = &project.package_manager;
            let root = project.path == ".";
            let (id, name, purpose) = if root {
                (
                    "hook:setup:install".to_string(),
                    "install".to_string(),
                    Purpose::Install("js".into()),
                )
            } else {
                (
                    format!("hook:setup:install:{}", project.path),
                    format!("install-{}", slug(&project.path)),
                    Purpose::Install(format!("js:{}", project.path)),
                )
            };
            let mut risks = Vec::new();
            let mut confidence = Confidence::High;
            if pm.source == PackageManagerSource::Assumed {
                confidence = Confidence::Medium;
                risks.push(
                    "No lockfile or packageManager field names the package manager; npm was \
                     assumed."
                        .to_string(),
                );
            }
            if pm.lockfile.is_none() {
                if !root {
                    // A directory outside the workspace with no lockfile of
                    // its own is a guess about the project layout.
                    confidence = Confidence::Medium;
                }
                risks.push(
                    "No lockfile: the install resolves versions and writes one, which shows up \
                     as a change in the new worktree."
                        .to_string(),
                );
            }
            let mut evidence = pm.evidence.clone();
            evidence.extend(project.evidence.clone());
            let step = Step {
                id,
                name,
                run: self.wrap(&project.path, pm.install_command()),
                title: format!(
                    "Install {} dependencies with {}{}",
                    if root { "JavaScript" } else { &project.path },
                    manager_name(pm.kind),
                    if pm.lockfile.is_some() {
                        " (from the lockfile)"
                    } else {
                        ""
                    }
                ),
                confidence,
                evidence,
                risks,
                purpose,
                depends_on: Vec::new(),
            };
            self.hook("setup", step);
        }
        for project in &facts.python {
            let root = project.path == ".";
            let Some(command) = project.install_command() else {
                self.warn(format!(
                    "{}: pip requirements found, but pip has no safe install to propose \
                     (environments are not relocatable and not cloned). Create the environment \
                     in a hook of your own, or move to uv or poetry",
                    if root { "." } else { &project.path }
                ));
                continue;
            };
            let (id, name) = if root {
                (
                    "hook:setup:install:python".to_string(),
                    "install-python".to_string(),
                )
            } else {
                (
                    format!("hook:setup:install:python:{}", project.path),
                    format!("install-python-{}", slug(&project.path)),
                )
            };
            let step = Step {
                id,
                name,
                run: self.wrap(&project.path, command),
                title: format!(
                    "Create the Python environment with {}",
                    match project.tool {
                        PythonTool::Uv => "uv",
                        PythonTool::Poetry => "poetry",
                        PythonTool::Pip => "pip",
                    }
                ),
                confidence: Confidence::High,
                evidence: project.evidence.clone(),
                risks: vec![
                    "Virtual environments are not relocatable, so each worktree creates its own \
                     instead of cloning .venv."
                        .to_string(),
                ],
                purpose: Purpose::Install(if root {
                    "python".into()
                } else {
                    format!("python:{}", project.path)
                }),
                depends_on: Vec::new(),
            };
            self.hook("setup", step);
        }
        // Ecosystems whose install command is fixed by convention.
        for fact in &facts.frameworks {
            let (key, name, command, title) = match fact.framework {
                Framework::Rails => (
                    "ruby",
                    "install-ruby",
                    "bundle install",
                    "Install Ruby gems with Bundler",
                ),
                Framework::Laravel => (
                    "php",
                    "install-php",
                    "composer install --no-interaction",
                    "Install PHP dependencies with Composer",
                ),
                Framework::Phoenix => (
                    "elixir",
                    "install-elixir",
                    "mix deps.get",
                    "Fetch Elixir dependencies with Mix",
                ),
                _ => continue,
            };
            let root = fact.path == ".";
            let step = Step {
                id: if root {
                    format!("hook:setup:install:{key}")
                } else {
                    format!("hook:setup:install:{key}:{}", fact.path)
                },
                name: if root {
                    name.to_string()
                } else {
                    format!("{name}-{}", slug(&fact.path))
                },
                run: self.wrap(&fact.path, command),
                title: title.to_string(),
                confidence: Confidence::High,
                evidence: fact.evidence.clone(),
                risks: Vec::new(),
                purpose: Purpose::Install(if root {
                    key.to_string()
                } else {
                    format!("{key}:{}", fact.path)
                }),
                depends_on: Vec::new(),
            };
            self.hook("setup", step);
        }
    }

    // -- code generation -------------------------------------------------------------

    pub fn codegen(&mut self) {
        let facts = self.facts;
        let prisma: Vec<&MigrationFact> = facts
            .migrations
            .iter()
            .filter(|m| m.tool == MigrationTool::Prisma)
            .collect();
        for (index, fact) in prisma.iter().enumerate() {
            // `prisma generate` already runs when dependencies are installed.
            let on_install = facts
                .js
                .iter()
                .flat_map(|project| &project.packages)
                .any(|package| package.generates_prisma_on_install());
            if on_install {
                continue;
            }
            let Some(pm) = self.js_pm(&fact.path).cloned() else {
                continue;
            };
            let id = if index == 0 {
                "hook:setup:codegen".to_string()
            } else {
                format!("hook:setup:codegen:{}", fact.path)
            };
            let step = Step {
                id,
                name: if index == 0 {
                    "prisma-generate".into()
                } else {
                    format!("prisma-generate-{}", slug(&fact.path))
                },
                run: self.wrap(&fact.path, &js_exec(&pm, "prisma", "generate")),
                title: "Generate the Prisma client".to_string(),
                confidence: Confidence::High,
                evidence: fact.evidence.clone(),
                risks: vec![
                    "No script in package.json runs `prisma generate` when dependencies are \
                     installed, so a fresh worktree has no client until this runs."
                        .to_string(),
                ],
                purpose: Purpose::Codegen,
                depends_on: Vec::new(),
            };
            self.hook("setup", step);
        }
    }

    // -- migrations ------------------------------------------------------------------

    pub fn migrations(&mut self) {
        let facts = self.facts;
        if !self.has_pg {
            if !facts.migrations.is_empty() {
                let tools: Vec<&str> = facts.migrations.iter().map(|m| tool_name(m.tool)).collect();
                self.warn(format!(
                    "{} migrations were detected, but no PostgreSQL service is configured, so no \
                     migration hook is proposed (devflow proposes migration hooks only for its \
                     PostgreSQL service; a MySQL, Valkey or Redis service gets its URL in \
                     .devflow/env but no migration hook); run migrations from a setup hook of \
                     your own",
                    tools.join(", ")
                ));
            }
            return;
        }
        let candidates: Vec<&MigrationFact> = facts
            .migrations
            .iter()
            .filter(|m| matches!(m.database, None | Some(Engine::Postgres)))
            .collect();
        let mut seen: Vec<(MigrationTool, String)> = Vec::new();
        let mut steps = Vec::new();
        for fact in candidates {
            if seen.contains(&(fact.tool, fact.path.clone())) {
                continue;
            }
            seen.push((fact.tool, fact.path.clone()));
            if let Some(plan) = self.migration_command(fact) {
                steps.push((fact, plan));
            }
        }
        let several = steps.len() > 1;
        let produced: Vec<(MigrationTool, String)> = steps
            .iter()
            .map(|(fact, _)| (fact.tool, fact.path.clone()))
            .collect();
        for (fact, plan) in steps {
            let tool = tool_name(fact.tool);
            let (id, name) = if !several {
                ("hook:setup:migrate".to_string(), "migrate".to_string())
            } else {
                let mut suffix = tool_slug(fact.tool).to_string();
                if fact.path != "." {
                    suffix = format!("{suffix}:{}", fact.path);
                }
                let mut name = format!("migrate-{}", tool_slug(fact.tool));
                if fact.path != "." {
                    name = format!("{name}-{}", slug(&fact.path));
                }
                (format!("hook:setup:migrate:{suffix}"), name)
            };
            let mut evidence = fact.evidence.clone();
            evidence.extend(plan.evidence);
            let step = Step {
                id,
                name,
                run: plan.run,
                title: format!("Apply {tool} migrations to the workspace database"),
                confidence: plan.confidence,
                evidence,
                risks: plan.risks,
                purpose: Purpose::Migrate,
                depends_on: self.pg_item.iter().cloned().collect(),
            };
            self.hook("setup", step);
        }
        // Tools devflow knows of but cannot run for the person.
        for fact in &facts.migrations {
            let unsupported = matches!(
                fact.tool,
                MigrationTool::Flyway | MigrationTool::Liquibase | MigrationTool::TypeOrm
            );
            if unsupported && !produced.contains(&(fact.tool, fact.path.clone())) {
                self.warn(format!(
                    "{} migrations were detected in {}, but devflow does not propose a command \
                     for them (they need connection settings it cannot guess); add a setup hook \
                     that runs them",
                    tool_name(fact.tool),
                    fact.path
                ));
            }
        }
    }

    /// The command that applies `fact`'s migrations, preferring a script or
    /// task the project already has.
    fn migration_command(&self, fact: &MigrationFact) -> Option<MigrationCommand> {
        let facts = self.facts;
        // A mise task is the project's own entry point.
        if fact.path == "."
            && let Some(mise) = &facts.toolchain.mise
        {
            for task in ["db:migrate", "migrate", "db:migrate:deploy"] {
                if mise.tasks.iter().any(|t| t == task) {
                    return Some(MigrationCommand {
                        run: format!("mise run {}", shell_quote(task)),
                        confidence: Confidence::High,
                        risks: vec![
                            format!("The body of the mise task `{task}` is not inspected."),
                            "A mise task gets the workspace's database only from mise's \
                             configuration (a task's own `env` is applied after anything \
                             devflow sets, so `devflow exec` cannot wrap it): keep \
                             DATABASE_URL out of the task's `env`, or call `devflow exec -- \
                             COMMAND` inside the task."
                                .to_string(),
                        ],
                        evidence: mise
                            .evidence
                            .iter()
                            .filter(|e| e.ends_with(&format!(":tasks.{task}")))
                            .cloned()
                            .collect(),
                    });
                }
            }
        }
        let js_tool = matches!(
            fact.tool,
            MigrationTool::Prisma
                | MigrationTool::Drizzle
                | MigrationTool::Knex
                | MigrationTool::Sequelize
                | MigrationTool::TypeOrm
        );
        if js_tool {
            let pm = self.js_pm(&fact.path)?.clone();
            // A package script, when the package defines one.
            let package = facts
                .js
                .iter()
                .flat_map(|project| &project.packages)
                .find(|package| package.path == fact.path);
            if let Some(package) = package {
                for script in [
                    "db:migrate",
                    "migrate",
                    "db:migrate:deploy",
                    "migrate:deploy",
                    "db:deploy",
                ] {
                    if package.has_script(script) {
                        let mut risks = vec![format!(
                            "The body of the package.json script `{script}` is not inspected."
                        )];
                        let mut confidence = Confidence::High;
                        if fact.tool == MigrationTool::Prisma {
                            confidence = Confidence::Medium;
                            risks.push(
                                "If the script runs `prisma migrate dev`, it is interactive and \
                                 can propose a reset; `prisma migrate deploy` is the \
                                 non-interactive command."
                                    .into(),
                            );
                        }
                        let argv = pm.run_argv();
                        return Some(MigrationCommand {
                            run: self.wrap(
                                &fact.path,
                                &format!("{} {} {}", argv[0], argv[1], shell_quote(script)),
                            ),
                            confidence,
                            risks,
                            evidence: vec![format!(
                                "{}:scripts.{script}",
                                if fact.path == "." {
                                    "package.json".to_string()
                                } else {
                                    format!("{}/package.json", fact.path)
                                }
                            )],
                        });
                    }
                }
            }
            let (args, bin, confidence, risks): (&str, &str, Confidence, Vec<String>) =
                match fact.tool {
                    MigrationTool::Prisma => (
                        "migrate deploy",
                        "prisma",
                        if fact.has_migrations() {
                            Confidence::High
                        } else {
                            Confidence::Medium
                        },
                        if fact.has_migrations() {
                            Vec::new()
                        } else {
                            vec![
                                "No prisma/migrations directory yet: `migrate deploy` has \
                                 nothing to apply if the project uses `db push`."
                                    .into(),
                            ]
                        },
                    ),
                    MigrationTool::Drizzle => (
                        "migrate",
                        "drizzle-kit",
                        Confidence::Medium,
                        vec![
                            "drizzle-kit reads its connection from drizzle.config; check it uses \
                             DATABASE_URL."
                                .into(),
                        ],
                    ),
                    MigrationTool::Knex => (
                        "migrate:latest",
                        "knex",
                        Confidence::Medium,
                        vec!["knexfile must read DATABASE_URL.".into()],
                    ),
                    MigrationTool::Sequelize => (
                        "db:migrate",
                        "sequelize-cli",
                        Confidence::Medium,
                        vec!["The sequelize config must read DATABASE_URL.".into()],
                    ),
                    _ => return None,
                };
            return Some(MigrationCommand {
                run: self.wrap(&fact.path, &js_exec(&pm, bin, args)),
                confidence,
                risks,
                evidence: Vec::new(),
            });
        }

        let py = self.py_prefix(&fact.path);
        let relative = |dir: &str| -> String {
            if fact.path == "." {
                dir.to_string()
            } else {
                dir.strip_prefix(&format!("{}/", fact.path))
                    .unwrap_or(dir)
                    .to_string()
            }
        };
        let python_env_risk = |this: &Self| -> Vec<String> {
            if this.py_project(&fact.path).map(|p| p.tool) == Some(PythonTool::Pip) {
                vec![
                    "This project installs with pip, and no environment is created: the command \
                     must find python on PATH (activate a virtualenv or let mise provide it)."
                        .into(),
                ]
            } else {
                Vec::new()
            }
        };
        let (command, confidence, mut risks): (String, Confidence, Vec<String>) = match fact.tool {
            MigrationTool::Django => (
                format!("{py}python manage.py migrate --noinput"),
                Confidence::High,
                python_env_risk(self),
            ),
            MigrationTool::Alembic => {
                let flask_migrate = fact
                    .config_files
                    .iter()
                    .any(|f| f.ends_with("migrations/alembic.ini"));
                if flask_migrate {
                    (format!("{py}flask db upgrade"), Confidence::Medium, {
                        let mut risks = vec![
                            "Flask-Migrate needs FLASK_APP (or a wsgi.py/app.py) to find the \
                                 application."
                                .to_string(),
                        ];
                        risks.extend(python_env_risk(self));
                        risks
                    })
                } else {
                    let config = fact
                        .config_files
                        .iter()
                        .find(|f| f.ends_with("alembic.ini"))
                        .map(|f| relative(f))
                        .filter(|f| f != "alembic.ini");
                    let flag = config
                        .map(|f| format!("-c {} ", shell_quote(&f)))
                        .unwrap_or_default();
                    (
                        format!("{py}alembic {flag}upgrade head"),
                        Confidence::High,
                        python_env_risk(self),
                    )
                }
            }
            MigrationTool::Rails => ("bin/rails db:migrate".into(), Confidence::High, Vec::new()),
            MigrationTool::Laravel => (
                "php artisan migrate --force --no-interaction".into(),
                Confidence::High,
                Vec::new(),
            ),
            MigrationTool::Ecto => ("mix ecto.migrate".into(), Confidence::High, Vec::new()),
            MigrationTool::Sqlx => (
                "sqlx migrate run".into(),
                Confidence::Medium,
                vec!["Needs sqlx-cli installed (cargo install sqlx-cli) and DATABASE_URL.".into()],
            ),
            MigrationTool::Diesel => (
                "diesel migration run".into(),
                Confidence::Medium,
                vec![
                    "Needs diesel_cli installed; it may rewrite the schema file named in \
                     diesel.toml."
                        .into(),
                ],
            ),
            MigrationTool::Goose => {
                let dir = fact.migrations_dirs.first().map(|d| relative(d))?;
                (
                    format!(
                        "goose -dir {} postgres \"$DATABASE_URL\" up",
                        shell_quote(&dir)
                    ),
                    Confidence::Medium,
                    vec!["Needs the goose binary on PATH.".into()],
                )
            }
            MigrationTool::GolangMigrate => {
                let dir = fact.migrations_dirs.first().map(|d| relative(d))?;
                (
                    format!(
                        "migrate -path {} -database \"$DATABASE_URL\" up",
                        shell_quote(&dir)
                    ),
                    Confidence::Medium,
                    vec!["Needs the migrate binary (golang-migrate) on PATH.".into()],
                )
            }
            MigrationTool::Dbmate => {
                let dir = fact.migrations_dirs.first().map(|d| relative(d));
                let flag = match dir.as_deref() {
                    Some("db/migrations") | None => String::new(),
                    Some(dir) => format!("--migrations-dir {} ", shell_quote(dir)),
                };
                (
                    format!("dbmate --no-dump-schema {flag}up"),
                    Confidence::Medium,
                    vec!["Needs the dbmate binary on PATH; it reads DATABASE_URL.".into()],
                )
            }
            MigrationTool::Atlas => {
                let dir = fact.migrations_dirs.first().map(|d| relative(d))?;
                (
                    format!(
                        "atlas migrate apply --dir {} --url \"$DATABASE_URL\"",
                        shell_quote(&format!("file://{dir}"))
                    ),
                    Confidence::Medium,
                    vec![
                        "Needs the atlas binary on PATH; atlas.hcl environments are not used."
                            .into(),
                    ],
                )
            }
            _ => return None,
        };
        // Only one fact says which engine the tool is for; a tool that does not
        // say could be talking to something else.
        if fact.database.is_none() && confidence == Confidence::High {
            risks.push(
                "The migration tool does not say which database it targets; this assumes the \
                 PostgreSQL service."
                    .to_string(),
            );
        }
        Some(MigrationCommand {
            run: self.wrap(&fact.path, &command),
            confidence,
            risks,
            evidence: Vec::new(),
        })
    }

    // -- env presets -----------------------------------------------------------------

    pub fn presets(&mut self) {
        if !self.has_pg {
            return;
        }
        let facts = self.facts;
        let file = self.devflow_file();
        let env_has = |name: &str| facts.env.has_key(name);
        let mut wanted: Vec<(EnvPreset, Confidence, Vec<String>, String)> = Vec::new();

        let prisma = facts.migrations.iter().find(|m| {
            m.tool == MigrationTool::Prisma && matches!(m.database, None | Some(Engine::Postgres))
        });
        if let Some(fact) = prisma {
            wanted.push((
                EnvPreset::Prisma,
                Confidence::High,
                fact.evidence.clone(),
                "Export DIRECT_URL for Prisma (the same URL as DATABASE_URL)".into(),
            ));
        }
        if facts.has_framework(Framework::Laravel) {
            let evidence = facts
                .frameworks
                .iter()
                .find(|f| f.framework == Framework::Laravel)
                .map(|f| f.evidence.clone())
                .unwrap_or_default();
            wanted.push((
                EnvPreset::Laravel,
                Confidence::High,
                evidence,
                "Export the DB_* variables Laravel reads".into(),
            ));
        }
        if facts.has_framework(Framework::SpringBoot) {
            let evidence = facts
                .frameworks
                .iter()
                .find(|f| f.framework == Framework::SpringBoot)
                .map(|f| f.evidence.clone())
                .unwrap_or_default();
            wanted.push((
                EnvPreset::Spring,
                Confidence::High,
                evidence,
                "Export the SPRING_DATASOURCE_* variables Spring Boot reads".into(),
            ));
        }
        if let Some(fact) = facts
            .migrations
            .iter()
            .find(|m| m.tool == MigrationTool::Alembic)
        {
            let confidence =
                if env_has("SQLALCHEMY_DATABASE_URI") || env_has("SQLALCHEMY_DATABASE_URL") {
                    Confidence::High
                } else {
                    Confidence::Medium
                };
            let mut evidence = fact.evidence.clone();
            evidence.extend(
                facts
                    .env
                    .keys
                    .iter()
                    .filter(|k| k.name.starts_with("SQLALCHEMY_"))
                    .flat_map(|k| k.files.iter().map(|f| format!("{f}:{}", k.name))),
            );
            wanted.push((
                EnvPreset::Sqlalchemy,
                confidence,
                evidence,
                "Export SQLALCHEMY_DATABASE_URI (postgresql+psycopg://)".into(),
            ));
        }
        // The DB_* parts, for apps that do not read a URL.
        let laravel_wanted = wanted.iter().any(|(p, ..)| *p == EnvPreset::Laravel);
        let reads_url = facts
            .env
            .keys
            .iter()
            .any(|k| k.category == EnvCategory::DatabaseUrl);
        if !laravel_wanted
            && !reads_url
            && env_has("DB_HOST")
            && (env_has("DB_NAME") || env_has("DB_USER"))
        {
            let evidence = facts
                .env
                .keys
                .iter()
                .filter(|k| {
                    matches!(
                        k.name.as_str(),
                        "DB_HOST" | "DB_PORT" | "DB_NAME" | "DB_USER" | "DB_PASSWORD"
                    )
                })
                .flat_map(|k| k.files.iter().map(|f| format!("{f}:{}", k.name)))
                .collect();
            wanted.push((
                EnvPreset::DbParts,
                Confidence::Medium,
                evidence,
                "Export DB_HOST, DB_PORT, DB_NAME, DB_USER and DB_PASSWORD".into(),
            ));
        }
        for (preset, confidence, evidence, title) in wanted {
            let name = preset.as_str();
            let mut spec = Spec::new(format!("env:preset:{name}"), ItemKind::Env, file, title)
                .value(json!({"preset": name, "variables": preset.keys()}))
                .confidence(confidence)
                .evidence(evidence)
                .depends_on(self.pg_item.iter().cloned())
                .ops(vec![PatchOp::Append {
                    path: vec!["env".into(), "presets".into()],
                    values: vec![Value::String(name.to_string())],
                    front: false,
                }]);
            spec.risks.push(
                "Presets are part of what you approve before devflow applies them; the values are built from \
                 the workspace's connection and are never written to the project file."
                    .to_string(),
            );
            // A variable the preset sets that a recipe or another preset sets too.
            if let Some(cfg) = &self.cfg {
                let taken: Vec<&str> = preset
                    .keys()
                    .iter()
                    .copied()
                    .filter(|key| cfg.env.vars.contains_key(*key))
                    .collect();
                if !taken.is_empty() {
                    let why = format!(
                        "[env] already sets {} by hand; the {name} preset would set it too. Keep \
                         the recipe, or replace it with presets = [\"{name}\"]",
                        taken.join(", ")
                    );
                    spec.ops.clear();
                    self.report_conflict(spec, why);
                    continue;
                }
                let overlaps: Vec<String> = cfg
                    .env
                    .presets
                    .iter()
                    .filter(|other| {
                        **other != preset
                            && other.keys().iter().any(|key| preset.keys().contains(key))
                    })
                    .map(|other| other.as_str().to_string())
                    .collect();
                if !overlaps.is_empty() {
                    let why = format!(
                        "the {} preset already sets some of the same variables",
                        overlaps.join(", ")
                    );
                    spec.ops.clear();
                    self.report_conflict(spec, why);
                    continue;
                }
            }
            self.push(spec);
        }
    }
}

struct MigrationCommand {
    run: String,
    confidence: Confidence,
    risks: Vec<String>,
    evidence: Vec<String>,
}

fn manager_name(kind: PackageManager) -> &'static str {
    match kind {
        PackageManager::Bun => "bun",
        PackageManager::Pnpm => "pnpm",
        PackageManager::Yarn => "yarn",
        PackageManager::Npm => "npm",
    }
}

/// Whether `run` starts a repository command under mise (`mise exec --`,
/// `mise x --`, `mise run`, `mise watch`, `mise tasks run`) without
/// `devflow exec` having the last word: mise then applies the `[env]` of its
/// configuration over the environment devflow gave the hook. `mise trust` and
/// `mise install` run no repository command. Found as a command word, not
/// anywhere in the text; see `env_file::mise_uses` for how and for what it
/// cannot see.
fn relies_on_mise_env(run: &str) -> bool {
    crate::env_file::mise_uses(run)
        .iter()
        .any(|found| found.runs_code && !found.through_devflow_exec)
}

/// Run a package binary with the project's package manager.
pub(super) fn js_exec(pm: &PackageManagerFact, bin: &str, args: &str) -> String {
    match pm.kind {
        PackageManager::Bun => format!("bunx {bin} {args}"),
        PackageManager::Pnpm => format!("pnpm exec {bin} {args}"),
        PackageManager::Yarn => format!("yarn {bin} {args}"),
        PackageManager::Npm => format!("npx {bin} {args}"),
    }
}

fn tool_name(tool: MigrationTool) -> &'static str {
    match tool {
        MigrationTool::Prisma => "Prisma",
        MigrationTool::Drizzle => "Drizzle",
        MigrationTool::Knex => "Knex",
        MigrationTool::TypeOrm => "TypeORM",
        MigrationTool::Sequelize => "Sequelize",
        MigrationTool::Alembic => "Alembic",
        MigrationTool::Django => "Django",
        MigrationTool::Rails => "Rails",
        MigrationTool::Laravel => "Laravel",
        MigrationTool::Ecto => "Ecto",
        MigrationTool::Sqlx => "sqlx",
        MigrationTool::Diesel => "Diesel",
        MigrationTool::Goose => "goose",
        MigrationTool::GolangMigrate => "golang-migrate",
        MigrationTool::Atlas => "Atlas",
        MigrationTool::Dbmate => "dbmate",
        MigrationTool::Flyway => "Flyway",
        MigrationTool::Liquibase => "Liquibase",
    }
}

fn tool_slug(tool: MigrationTool) -> &'static str {
    match tool {
        MigrationTool::Prisma => "prisma",
        MigrationTool::Drizzle => "drizzle",
        MigrationTool::Knex => "knex",
        MigrationTool::TypeOrm => "typeorm",
        MigrationTool::Sequelize => "sequelize",
        MigrationTool::Alembic => "alembic",
        MigrationTool::Django => "django",
        MigrationTool::Rails => "rails",
        MigrationTool::Laravel => "laravel",
        MigrationTool::Ecto => "ecto",
        MigrationTool::Sqlx => "sqlx",
        MigrationTool::Diesel => "diesel",
        MigrationTool::Goose => "goose",
        MigrationTool::GolangMigrate => "golang-migrate",
        MigrationTool::Atlas => "atlas",
        MigrationTool::Dbmate => "dbmate",
        MigrationTool::Flyway => "flyway",
        MigrationTool::Liquibase => "liquibase",
    }
}
