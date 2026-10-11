//! The setup wizard: from a repository with no devflow configuration (or a
//! partial one) to a project that creates workspaces.
//!
//! Six steps, each a screen of the same dialog:
//!
//! 1. **Detected**: what devflow found, as plain facts.
//! 2. **Proposal**: the items core proposes, grouped, each with a checkbox set
//!    to its default, a confidence, the reason and a detail you can open.
//! 3. **Review**: the exact lines that would be added to each file.
//! 4. **Apply**: core writes the files; the result is listed per file.
//! 5. **Approve**: the commands and recipes an approval covers, and the button.
//! 6. **Done**: what to do next.
//!
//! The wizard decides nothing: scanning, planning, previewing, applying and
//! approving are `devflow_core` calls on the background executor, the same
//! ones `devflow init` and `devflow trust` make. Applying never installs a
//! tool, approves a hook or runs a command; approval is its own, explicit step.
use super::{
    pitchfork_panel,
    ui::{self, Tone},
    widgets::{banner, caption, card, chip, mono, section_header},
};
use crate::{
    jobs::{self, FailureKind},
    model::PitchforkOffer,
    state::{AppEvent, AppState},
};
use devflow_core::{
    detect::{self, Facts},
    error::StalePlan,
    project::Project,
    propose::{
        self, ApplyReport, Confidence, DiffKind, ExistingFiles, FileChange, FilePreview,
        FileStatus, Item, ItemKind, PlanOptions, PostgresChoice, Proposal,
    },
    redact, registry,
    trust::{self, Scope, TrustState},
};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        h_flex,
        notification::Notification,
        radio::{Radio, RadioGroup},
        spinner::Spinner,
        stepper::{Stepper, StepperItem},
        v_flex,
    },
    prelude::*,
    *,
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

actions!(
    devflow_wizard,
    [WizardNext, WizardBack, WizardUp, WizardDown, WizardToggle]
);

/// The wizard's key bindings; they apply while the wizard has focus.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", WizardNext, Some("SetupWizard")),
        KeyBinding::new("right", WizardNext, Some("SetupWizard")),
        KeyBinding::new("secondary-right", WizardNext, Some("SetupWizard")),
        KeyBinding::new("left", WizardBack, Some("SetupWizard")),
        KeyBinding::new("secondary-left", WizardBack, Some("SetupWizard")),
        KeyBinding::new("up", WizardUp, Some("SetupWizard")),
        KeyBinding::new("down", WizardDown, Some("SetupWizard")),
        KeyBinding::new("space", WizardToggle, Some("SetupWizard")),
    ]);
}

impl AppState {
    /// Ask the shell to open the setup wizard for the selected project.
    pub fn request_setup_wizard(&mut self, cx: &mut Context<Self>) {
        if let Some(root) = self.selection.current().map(|m| m.entry.root.clone()) {
            cx.emit(AppEvent::OfferInitialize(root));
        }
    }

    /// The project's configuration file changed on disk by an edit made
    /// outside the Config tab (the Setup tab, the wizard): the Config tab's
    /// copy is read again by the next refresh.
    pub fn config_changed_on_disk(&mut self, cx: &mut Context<Self>) {
        if let Some(model) = self.selection.current_mut() {
            model.config = None;
            model.invalidate_refresh();
        }
        self.refresh(cx);
    }
}

// ---- the steps -----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Detected,
    Proposal,
    Review,
    Apply,
    Approve,
    Done,
}

impl Step {
    pub const ALL: [Step; 6] = [
        Step::Detected,
        Step::Proposal,
        Step::Review,
        Step::Apply,
        Step::Approve,
        Step::Done,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Step::Detected => "Detected",
            Step::Proposal => "Proposal",
            Step::Review => "Review",
            Step::Apply => "Apply",
            Step::Approve => "Approve",
            Step::Done => "Done",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    fn next(self) -> Option<Step> {
        Self::ALL.get(self.index() + 1).copied()
    }

    fn previous(self) -> Option<Step> {
        self.index().checked_sub(1).map(|i| Self::ALL[i])
    }
}

/// The PostgreSQL decision, as the wizard offers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PgChoice {
    Managed,
    External,
    None,
}

impl PgChoice {
    const ALL: [PgChoice; 3] = [PgChoice::Managed, PgChoice::External, PgChoice::None];

    fn core(self) -> PostgresChoice {
        match self {
            PgChoice::Managed => PostgresChoice::Managed,
            PgChoice::External => PostgresChoice::External,
            PgChoice::None => PostgresChoice::None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            PgChoice::Managed => "Managed by devflow (a Docker container)",
            PgChoice::External => "An existing server",
            PgChoice::None => "No database",
        }
    }

    /// What the plan currently proposes for the service.
    fn of_plan(plan: &Proposal) -> Option<PgChoice> {
        if plan
            .already_configured
            .iter()
            .any(|id| id.starts_with("service:"))
        {
            return None;
        }
        Some(
            match plan
                .items
                .iter()
                .find(|i| is_postgres_service(i))
                .and_then(|i| i.value.get("mode")?.as_str())
            {
                Some("managed") => PgChoice::Managed,
                Some("external") => PgChoice::External,
                _ => PgChoice::None,
            },
        )
    }
}

// ---- facts as chips ------------------------------------------------------------------

/// A group of facts under one label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChipGroup {
    pub label: &'static str,
    pub chips: Vec<String>,
}

/// A fact's name as a person writes it (`next_js` is `Next.js`).
pub fn pretty<T: serde::Serialize>(value: &T) -> String {
    let raw = match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        _ => return String::new(),
    };
    let known = match raw.as_str() {
        "next_js" => "Next.js",
        "nest_js" => "NestJS",
        "fast_api" => "FastAPI",
        "spring_boot" => "Spring Boot",
        "svelte_kit" => "SvelteKit",
        "react_router" => "React Router",
        "type_orm" => "TypeORM",
        "golang_migrate" => "golang-migrate",
        "sql_server" => "SQL Server",
        "postgres" => "PostgreSQL",
        "mysql" => "MySQL",
        "mariadb" => "MariaDB",
        "mongodb" => "MongoDB",
        "clickhouse" => "ClickHouse",
        "rabbitmq" => "RabbitMQ",
        "opensearch" => "OpenSearch",
        "localstack" => "LocalStack",
        "minio" => "MinIO",
        "npm" | "pnpm" | "uv" | "pip" | "sqlx" | "goose" | "dbmate" => return raw,
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    raw.split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn unique(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        if !item.is_empty() && !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// What the scan found, in friendly groups. Empty groups are left out.
pub fn chip_groups(facts: &Facts) -> Vec<ChipGroup> {
    let mut groups =
        vec![
            ChipGroup {
                label: "Languages and frameworks",
                chips: unique(facts.frameworks.iter().map(|f| pretty(&f.framework))),
            },
            ChipGroup {
                label: "Package managers",
                chips: unique(
                    facts
                        .js
                        .iter()
                        .map(|p| pretty(&p.package_manager.kind))
                        .chain(facts.python.iter().map(|p| pretty(&p.tool))),
                ),
            },
            ChipGroup {
                label: "Databases and migrations",
                chips: unique(
                    facts
                        .migrations
                        .iter()
                        .map(|m| pretty(&m.tool))
                        .chain(facts.drivers.iter().map(|d| pretty(&d.engine))),
                ),
            },
            ChipGroup {
                label: "Compose services",
                chips: unique(facts.compose.iter().flat_map(|c| &c.services).map(
                    |s| match &s.tag {
                        Some(tag) => format!("{} ({} {tag})", s.name, pretty(&s.engine)),
                        None => format!("{} ({})", s.name, pretty(&s.engine)),
                    },
                )),
            },
            ChipGroup {
                label: "Processes",
                chips: unique(facts.processes.iter().map(|p| {
                    let source = pretty(&p.source);
                    format!("{} ({source})", p.name)
                })),
            },
        ];
    let mut tooling = Vec::new();
    if let Some(mise) = &facts.toolchain.mise {
        tooling.push(match mise.tools.len() {
            0 => "mise".to_string(),
            _ => format!("mise: {}", mise.tools.join(", ")),
        });
    }
    tooling.extend(facts.toolchain.version_files.iter().map(|f| f.file.clone()));
    if facts.wiring.pitchfork.is_some() {
        tooling.push("pitchfork.toml".to_string());
    }
    if facts.compose.is_some() && facts.wiring.devflow_config.is_none() {
        tooling.push("docker compose".to_string());
    }
    if let Some(config) = &facts.wiring.devflow_config {
        tooling.push(format!("{config} (already there)"));
    }
    let env_files = facts.env.files.len();
    if env_files > 0 {
        tooling.push(format!(
            "{env_files} .env file{} (names only, no values)",
            if env_files == 1 { "" } else { "s" }
        ));
    }
    groups.push(ChipGroup {
        label: "Tooling and files",
        chips: unique(tooling),
    });
    groups.retain(|g| !g.chips.is_empty());
    groups
}

// ---- items ---------------------------------------------------------------------------

/// Items grouped under their heading, in the order of the plan. A kind
/// another part of devflow adds later gets its own group without a change here.
pub fn grouped_items(plan: &Proposal) -> Vec<(&'static str, Vec<&Item>)> {
    let mut groups: Vec<(&'static str, Vec<&Item>)> = Vec::new();
    for item in &plan.items {
        let heading = item.kind.heading();
        match groups.iter_mut().find(|(h, _)| *h == heading) {
            Some((_, items)) => items.push(item),
            None => groups.push((heading, vec![item])),
        }
    }
    groups
}

fn kind_icon(kind: ItemKind) -> Lucide {
    match kind {
        ItemKind::Project => Lucide::FolderGit2,
        ItemKind::Copy => Lucide::FilePlus,
        ItemKind::Service => Lucide::Database,
        ItemKind::Hook => Lucide::ListChecks,
        ItemKind::Env => Lucide::Variable,
        ItemKind::Daemon => Lucide::Server,
        ItemKind::Mise => Lucide::Package,
        #[allow(unreachable_patterns)]
        _ => Lucide::SlidersHorizontal,
    }
}

/// The items that can be ticked: they have something to write and nothing in
/// the way.
pub fn selectable(plan: &Proposal, item: &Item) -> bool {
    let ids: BTreeSet<&str> = plan.items.iter().map(|i| i.id.as_str()).collect();
    item.is_actionable() && item.blocking_conflicts(&ids).is_empty()
}

/// The default selection: what `devflow init` applies without being told.
pub fn default_selection(plan: &Proposal) -> BTreeSet<String> {
    plan.select(&[], &[], true)
        .map(|s| s.accepted.into_iter().collect())
        .unwrap_or_default()
}

/// Tick or untick `id`. Ticking brings the items it depends on; unticking
/// drops the items that depend on it; ticking one of two items that exclude
/// each other unticks the other.
pub fn toggle(plan: &Proposal, selected: &mut BTreeSet<String>, id: &str) {
    let Some(item) = plan.item(id) else {
        return;
    };
    if !selectable(plan, item) {
        return;
    }
    if selected.remove(id) {
        // Whatever needs it goes too.
        loop {
            let dependents: Vec<String> = plan
                .items
                .iter()
                .filter(|i| {
                    selected.contains(&i.id) && i.depends_on.iter().any(|d| !selected.contains(d))
                })
                .map(|i| i.id.clone())
                .collect();
            if dependents.is_empty() {
                break;
            }
            for dependent in dependents {
                selected.remove(&dependent);
            }
        }
        return;
    }
    selected.insert(id.to_string());
    for needed in plan.with_dependencies(&[id.to_string()]) {
        if plan.item(&needed).is_some_and(|i| selectable(plan, i)) {
            selected.insert(needed);
        }
    }
    for conflict in &item.conflicts {
        selected.remove(conflict);
    }
    for other in &plan.items {
        if other.conflicts.iter().any(|c| c == id) {
            selected.remove(&other.id);
        }
    }
}

/// What is applied: the ticked items and everything they need.
pub fn accepted(plan: &Proposal, selected: &BTreeSet<String>) -> Vec<String> {
    let ids: Vec<String> = selected.iter().cloned().collect();
    plan.with_dependencies(&ids)
        .into_iter()
        .filter(|id| plan.item(id).is_some_and(|i| i.is_actionable()))
        .collect()
}

/// Whether `item` is the PostgreSQL service (the one the choice above is
/// about), not a Valkey, Redis or MySQL one: those name their engine.
fn is_postgres_service(item: &Item) -> bool {
    item.kind == ItemKind::Service
        && item
            .value
            .get("engine")
            .is_none_or(|engine| engine == "postgres")
}

/// A service item the plan marks as a preview (MySQL).
pub fn is_preview(item: &Item) -> bool {
    item.value.get("preview").and_then(|p| p.as_bool()) == Some(true)
}

fn confidence_chip(confidence: Confidence, cx: &App) -> Div {
    match confidence {
        Confidence::High => chip("recommended", Tone::Success, cx),
        Confidence::Medium => chip("likely", Tone::Warning, cx),
        Confidence::Low => chip("a guess", Tone::Neutral, cx),
    }
}

// ---- background work -----------------------------------------------------------------

/// A scan and the plan made from it.
#[derive(Clone)]
pub struct Scanned {
    pub facts: Facts,
    pub plan: Proposal,
}

fn scan(root: &Path, postgres: Option<PostgresChoice>) -> anyhow::Result<Scanned> {
    let facts = detect::scan(root)?;
    let existing = ExistingFiles::read(root)?;
    let options = PlanOptions {
        postgres,
        ..PlanOptions::default()
    };
    let plan = propose::plan_with(root, &facts, &existing, &options)?;
    Ok(Scanned { facts, plan })
}

/// The result of applying.
enum Applied {
    Done(Box<ApplyReport>, Option<uuid::Uuid>),
    /// The plan no longer describes the repository (core says so, with what
    /// changed); nothing was overwritten.
    Stale(String),
}

/// Apply the plan. Core decides whether it still fits the repository: it
/// resumes an interrupted apply of the same plan from its journal, which a
/// check made here, before asking, would refuse (the files it already wrote
/// differ from what the plan saw).
fn apply(root: &Path, plan: &Proposal, accepted: &[String]) -> anyhow::Result<Applied> {
    let report = match propose::apply(root, plan, accepted) {
        Ok(report) => report,
        Err(error) if error.downcast_ref::<StalePlan>().is_some() => {
            return Ok(Applied::Stale(redact::text(&format!("{error:#}"))));
        }
        Err(error) => return Err(error),
    };
    // The project exists now: register it so the sidebar lists it. Failing to
    // register is not failing to apply.
    let id = Project::open(root).ok().and_then(|project| {
        registry::add(&project).ok()?;
        Some(project.id())
    });
    Ok(Applied::Done(Box::new(report), id))
}

#[derive(Clone)]
enum ApplyState {
    Idle,
    Running,
    Done(Box<ApplyReport>),
    Stale(String),
    Failed(String),
}

#[derive(Clone)]
enum PreviewState {
    Idle,
    Loading,
    Ready(Vec<FilePreview>),
    Failed(String),
}

enum ScanState {
    Loading,
    Ready(Box<Scanned>),
    Failed(String),
}

// ---- the wizard ---------------------------------------------------------------------

pub struct OnboardingView {
    state: Entity<AppState>,
    root: PathBuf,
    step: Step,
    scan: ScanState,
    pg: Option<PgChoice>,
    selected: BTreeSet<String>,
    expanded: BTreeSet<String>,
    /// The row the arrow keys are on, among the selectable items.
    cursor: usize,
    preview: PreviewState,
    apply: ApplyState,
    approval: Option<Entity<ApprovalView>>,
    approved: bool,
    /// What the machine has of pitchfork, read when the wizard opened and
    /// again after an install. `None` until it was read.
    offer: Option<PitchforkOffer>,
    offer_epoch: u64,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

/// Open the wizard for the project at `root`.
pub fn open(state: &Entity<AppState>, root: PathBuf, window: &mut Window, cx: &mut App) {
    open_view(state, root, window, cx);
}

/// [`open`], giving the wizard's view.
pub(super) fn open_view(
    state: &Entity<AppState>,
    root: PathBuf,
    window: &mut Window,
    cx: &mut App,
) -> Entity<OnboardingView> {
    let view = cx.new(|cx| OnboardingView::new(state.clone(), root, window, cx));
    let focus = view.read(cx).focus.clone();
    let hosted = view.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Set up devflow")
            .w(px(900.))
            .child(hosted.clone())
    });
    window.defer(cx, move |window, cx| window.focus(&focus, cx));
    view
}

impl OnboardingView {
    pub fn new(
        state: Entity<AppState>,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            state,
            root,
            step: Step::Detected,
            scan: ScanState::Loading,
            pg: None,
            selected: BTreeSet::new(),
            expanded: BTreeSet::new(),
            cursor: 0,
            preview: PreviewState::Idle,
            apply: ApplyState::Idle,
            approval: None,
            approved: false,
            offer: None,
            offer_epoch: 0,
            focus: cx.focus_handle(),
            _subscriptions: Vec::new(),
        };
        this.offer_epoch = this.state.read(cx).pitchfork_epoch;
        let state = this.state.clone();
        this._subscriptions
            .push(cx.observe_in(&state, window, |this, state, window, cx| {
                // An install ran: pitchfork may be there now.
                let epoch = state.read(cx).pitchfork_epoch;
                if epoch != this.offer_epoch {
                    this.offer_epoch = epoch;
                    this.reload_offer(window, cx);
                }
                cx.notify();
            }));
        this.reload_offer(window, cx);
        this.rescan(None, window, cx);
        this
    }

    /// Ask what the machine has of pitchfork (off the UI thread).
    fn reload_offer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.state.read(cx).pitchfork.clone();
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            let offer = cx
                .background_spawn(async move { jobs::load_offer(&source, &root) })
                .await;
            this.update(cx, |this, cx| {
                this.offer = Some(offer);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Whether the plan involves processes, so pitchfork matters: it proposes
    /// a daemon or the autostart, or the project has them already.
    fn involves_processes(plan: &Proposal) -> bool {
        plan.items.iter().any(|i| i.kind == ItemKind::Daemon)
            || plan
                .already_configured
                .iter()
                .any(|id| id.starts_with("daemon:") || id == "pitchfork:autostart")
    }

    /// pitchfork, when the proposal involves processes: whether it is
    /// installed, how to get it (a click runs the command shown), and its
    /// proxy (commands to copy, never run).
    fn pitchfork_section(&self, plan: &Proposal, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !Self::involves_processes(plan) {
            return None;
        }
        let offer = self.offer.clone().unwrap_or_default();
        let install = self.state.read(cx).pitchfork_install.clone();
        let (_, line) = pitchfork_panel::state_line(&offer);
        Some(
            card(cx)
                .id("wizard-pitchfork")
                .child(section_header(
                    Lucide::Server,
                    "pitchfork",
                    Some(line.into()),
                    None,
                    cx,
                ))
                .child(
                    v_flex()
                        .gap_2()
                        .p_3()
                        .child(
                            div().text_sm().text_color(ui::muted(cx)).child(
                                "Processes run under pitchfork, one set per workspace, each with its own URL. Installing it is your click; devflow runs nothing until you ask.",
                            ),
                        )
                        .children(pitchfork_panel::install_card(
                            pitchfork_panel::WIZARD,
                            &offer,
                            &install,
                            &self.state,
                            cx,
                        ))
                        .children(pitchfork_panel::proxy_card(
                            pitchfork_panel::WIZARD,
                            &offer,
                            cx,
                        )),
                )
                .test_support()
                .into_any_element(),
        )
    }

    /// Scan and plan again (first time, a different PostgreSQL choice, or
    /// after the repository changed under the plan). A choice that still
    /// exists in the new plan stays ticked or unticked as the person left it.
    fn rescan(&mut self, postgres: Option<PgChoice>, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let wanted = postgres.map(PgChoice::core);
        let previous = match &self.scan {
            ScanState::Ready(scanned) if postgres.is_some() || !self.selected.is_empty() => Some((
                scanned
                    .plan
                    .items
                    .iter()
                    .map(|i| i.id.clone())
                    .collect::<BTreeSet<_>>(),
                self.selected.clone(),
            )),
            _ => None,
        };
        self.scan = ScanState::Loading;
        self.preview = PreviewState::Idle;
        self.apply = ApplyState::Idle;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let scanned = cx
                .background_spawn(async move { jobs::catch(|| scan(&root, wanted)) })
                .await;
            this.update(cx, |this, cx| {
                match scanned {
                    Ok(scanned) => {
                        let defaults = default_selection(&scanned.plan);
                        this.selected = match &previous {
                            // Keep the person's choice for items that were in the
                            // old plan too; new ones start at their default.
                            Some((known, kept)) => scanned
                                .plan
                                .items
                                .iter()
                                .filter(|i| {
                                    if known.contains(&i.id) {
                                        kept.contains(&i.id)
                                    } else {
                                        defaults.contains(&i.id)
                                    }
                                })
                                .map(|i| i.id.clone())
                                .collect(),
                            None => defaults,
                        };
                        this.pg = PgChoice::of_plan(&scanned.plan);
                        this.cursor = 0;
                        this.scan = ScanState::Ready(Box::new(scanned));
                    }
                    Err(message) => this.scan = ScanState::Failed(message),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn plan(&self) -> Option<&Proposal> {
        match &self.scan {
            ScanState::Ready(scanned) => Some(&scanned.plan),
            _ => None,
        }
    }

    /// The ticked items that can be ticked, in plan order: what the arrow keys walk.
    fn rows(&self) -> Vec<String> {
        self.plan()
            .map(|plan| {
                plan.items
                    .iter()
                    .filter(|i| selectable(plan, i))
                    .map(|i| i.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    // ---- navigation ------------------------------------------------------------------

    /// Whether the wizard may leave the current step forward.
    fn can_advance(&self) -> bool {
        match self.step {
            Step::Detected => matches!(self.scan, ScanState::Ready(_)),
            Step::Proposal => matches!(self.scan, ScanState::Ready(_)),
            Step::Review => {
                matches!(self.preview, PreviewState::Ready(_)) && self.has_nothing_blocking()
            }
            Step::Apply => matches!(self.apply, ApplyState::Done(_)),
            Step::Approve => true,
            Step::Done => false,
        }
    }

    /// Nothing selected writes nothing: the wizard then skips straight to
    /// approval of what is already there.
    fn has_nothing_blocking(&self) -> bool {
        match &self.preview {
            PreviewState::Ready(files) => files.iter().all(|f| f.conflicts.is_empty()),
            _ => false,
        }
    }

    fn can_go_back(&self) -> bool {
        match self.step {
            Step::Detected | Step::Done => false,
            Step::Proposal | Step::Review => true,
            // While the files are being written, and once they are, the
            // screens before the result no longer apply; a failed or stale
            // apply goes back to the review.
            Step::Apply => matches!(self.apply, ApplyState::Failed(_) | ApplyState::Stale(_)),
            // From the approval the result of the apply can be seen again.
            Step::Approve => true,
        }
    }

    fn go_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.step {
            Step::Proposal => self.enter_review(window, cx),
            Step::Review => {
                if self.can_advance() {
                    self.step = Step::Apply;
                    self.start_apply(window, cx);
                }
            }
            Step::Apply => {
                if self.can_advance() {
                    self.enter_approve(window, cx);
                }
            }
            Step::Approve | Step::Detected => {
                self.step = self.step.next().unwrap_or(self.step);
            }
            Step::Done => self.close(window, cx),
        }
        cx.notify();
    }

    fn go_back(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if !self.can_go_back() {
            return;
        }
        if self.step == Step::Apply {
            self.step = Step::Review;
        } else if let Some(previous) = self.step.previous() {
            self.step = previous;
        }
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
    }

    fn enter_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(plan) = self.plan().cloned() else {
            return;
        };
        let ids = accepted(&plan, &self.selected);
        self.step = Step::Review;
        self.preview = PreviewState::Loading;
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            let previews = cx
                .background_spawn(
                    async move { jobs::catch(|| propose::preview(&root, &plan, &ids)) },
                )
                .await;
            this.update(cx, |this, cx| {
                this.preview = match previews {
                    Ok(files) => PreviewState::Ready(files),
                    Err(message) => PreviewState::Failed(message),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn start_apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(plan) = self.plan().cloned() else {
            return;
        };
        let ids = accepted(&plan, &self.selected);
        self.apply = ApplyState::Running;
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            let outcome = cx
                .background_spawn(async move { jobs::catch(|| apply(&root, &plan, &ids)) })
                .await;
            this.update_in(cx, |this, window, cx| {
                match outcome {
                    Ok(Applied::Done(report, id)) => {
                        this.apply = ApplyState::Done(report);
                        // The project is configured now: the app reads it.
                        this.state
                            .update(cx, |s, cx| s.reload_entries(id, None, cx));
                    }
                    Ok(Applied::Stale(changed)) => this.apply = ApplyState::Stale(changed),
                    Err(message) => this.apply = ApplyState::Failed(message),
                }
                let _ = window;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn enter_approve(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let approval =
            cx.new(|cx| ApprovalView::new(self.state.clone(), self.root.clone(), window, cx));
        self._subscriptions.push(
            cx.subscribe(&approval, |this, _, event: &ApprovalEvent, cx| {
                if matches!(event, ApprovalEvent::Approved) {
                    this.approved = true;
                    cx.notify();
                }
            }),
        );
        self.approval = Some(approval);
        self.step = Step::Approve;
    }

    /// Plan again after the repository changed under the plan.
    fn replan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step = Step::Proposal;
        let choice = self.pg;
        self.rescan(choice, window, cx);
    }

    fn create_first_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        self.state
            .update(cx, |s, cx| s.request_new_workspace(None, cx));
    }

    // ---- test hooks ------------------------------------------------------------------

    #[cfg(test)]
    pub(super) fn step(&self) -> Step {
        self.step
    }

    /// Whether the plan involves processes (pitchfork is offered), for tests.
    #[cfg(test)]
    pub(super) fn proposes_processes(&self) -> bool {
        self.plan().is_some_and(Self::involves_processes)
    }

    #[cfg(test)]
    pub(super) fn selected(&self) -> &BTreeSet<String> {
        &self.selected
    }

    #[cfg(test)]
    pub(super) fn scanned(&self) -> Option<&Scanned> {
        match &self.scan {
            ScanState::Ready(s) => Some(s),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn applied(&self) -> Option<&ApplyReport> {
        match &self.apply {
            ApplyState::Done(report) => Some(report),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn is_stale(&self) -> bool {
        matches!(self.apply, ApplyState::Stale(_))
    }

    #[cfg(test)]
    pub(super) fn approval(&self) -> Option<Entity<ApprovalView>> {
        self.approval.clone()
    }
}

// ---- drawing the steps -----------------------------------------------------------------

impl OnboardingView {
    fn detected(&self, cx: &mut Context<Self>) -> AnyElement {
        let ScanState::Ready(scanned) = &self.scan else {
            return self.scan_status(cx);
        };
        let groups = chip_groups(&scanned.facts);
        let warnings = scanned.facts.warnings.len();
        v_flex()
            .id("wizard-detected")
            .gap_3()
            .child(
                div().text_sm().text_color(ui::muted(cx)).child(
                    "devflow read the files of this repository. It ran nothing, and it never reads the values in .env files.",
                ),
            )
            .children(groups.into_iter().map(|group| {
                v_flex()
                    .gap_1()
                    .child(caption(group.label, cx))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .flex_wrap()
                            .children(group.chips.into_iter().map(|c| chip(c, Tone::Neutral, cx))),
                    )
            }))
            .when(chip_groups(&scanned.facts).is_empty(), |col| {
                col.child(ui::empty_state(
                    Lucide::Rocket,
                    "Nothing recognized yet",
                    "That is fine: the proposal starts from a small project file you can grow.",
                    None,
                ))
            })
            .when(warnings > 0, |col| {
                col.child(
                    div()
                        .text_xs()
                        .text_color(ui::muted(cx))
                        .child(format!("{warnings} file(s) could not be read; the proposal ignores them.")),
                )
            })
            .test_support()
            .into_any_element()
    }

    fn scan_status(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.scan {
            ScanState::Loading => ui::loading("Looking at the repository…", cx)
                .h(px(200.))
                .into_any_element(),
            ScanState::Failed(message) => v_flex()
                .gap_3()
                .child(ui::error_banner(message.clone(), cx).id("wizard-scan-error"))
                .child(
                    Button::new("wizard-rescan")
                        .label("Try again")
                        .on_click(cx.listener(|this, _, window, cx| this.rescan(None, window, cx))),
                )
                .into_any_element(),
            ScanState::Ready(_) => div().into_any_element(),
        }
    }

    fn item_row(
        &self,
        plan: &Proposal,
        item: &Item,
        row_index: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = item.id.clone();
        let checked = self.selected.contains(&id);
        let can = selectable(plan, item);
        let open = self.expanded.contains(&id);
        let on_cursor = row_index.is_some_and(|r| r == self.cursor);
        let reason = item
            .evidence
            .first()
            .map(|e| format!("because of {e}"))
            .unwrap_or_default();
        let blocking = item.blocking_conflicts(
            &plan
                .items
                .iter()
                .map(|i| i.id.as_str())
                .collect::<BTreeSet<_>>(),
        );
        let toggle_id = id.clone();
        let expand_id = id.clone();
        v_flex()
            .id(SharedString::from(format!("item-row-{id}")))
            .w_full()
            .border_b_1()
            .border_color(cx.theme().border.opacity(0.5))
            .when(on_cursor, |row| row.bg(cx.theme().muted.opacity(0.5)))
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_1p5()
                    .gap_3()
                    .items_start()
                    .child(
                        Checkbox::new(SharedString::from(format!("item-{id}")))
                            .checked(checked)
                            .disabled(!can)
                            .on_click(cx.listener(move |this, _: &bool, _, cx| {
                                if let ScanState::Ready(scanned) = &this.scan {
                                    let plan = scanned.plan.clone();
                                    toggle(&plan, &mut this.selected, &toggle_id);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(div().text_sm().child(item.title.clone()))
                                    .child(confidence_chip(item.confidence, cx))
                                    .when(is_preview(item), |row| {
                                        row.child(chip("preview", Tone::Warning, cx))
                                    }),
                            )
                            .when(!reason.is_empty(), |col| {
                                col.child(
                                    div()
                                        .text_xs()
                                        .truncate()
                                        .text_color(ui::muted(cx))
                                        .child(reason),
                                )
                            })
                            .when(!blocking.is_empty(), |col| {
                                col.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().warning)
                                        .child(format!(
                                            "Cannot be applied: {}. Resolve it in the file, then plan again.",
                                            blocking.join("; ")
                                        )),
                                )
                            })
                            .when(blocking.is_empty() && !item.is_actionable(), |col| {
                                col.child(
                                    div()
                                        .text_xs()
                                        .text_color(ui::muted(cx))
                                        .child("Nothing devflow can write: do this one by hand."),
                                )
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("expand-{id}")))
                            .ghost()
                            .xsmall()
                            .icon(if open { IconName::ChevronUp } else { IconName::ChevronDown })
                            .tooltip(if open { "Hide the detail" } else { "Show the detail" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.expanded.remove(&expand_id) {
                                    this.expanded.insert(expand_id.clone());
                                }
                                cx.notify();
                            })),
                    ),
            )
            .when(open, |col| col.child(self.item_detail(item, cx)))
            .test_support()
            .into_any_element()
    }

    fn item_detail(&self, item: &Item, cx: &mut Context<Self>) -> impl IntoElement {
        let list = |label: &'static str, entries: &[String]| -> Option<Div> {
            (!entries.is_empty()).then(|| {
                v_flex().gap_0p5().child(caption(label, cx)).children(
                    entries
                        .iter()
                        .map(|e| div().text_xs().child(redact::text(e)).into_any_element()),
                )
            })
        };
        let mut value = item.value.clone();
        redact::json(&mut value);
        let patch = if item.is_actionable() {
            Some(format!(
                "{}: {}",
                item.patch.file,
                item.patch
                    .ops
                    .iter()
                    .map(|op| op.target())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        } else {
            None
        };
        v_flex()
            .id(SharedString::from(format!("item-detail-{}", item.id)))
            .gap_2()
            .px_10()
            .pb_3()
            .children(list("Evidence", &item.evidence))
            .children(list("What can go wrong", &item.risks))
            .children(list("Needs", &item.depends_on))
            .children(patch.map(|p| {
                v_flex()
                    .gap_0p5()
                    .child(caption("Writes", cx))
                    .child(mono(p, cx).text_xs())
            }))
            .when(!value.is_null(), |col| {
                col.child(
                    v_flex()
                        .gap_0p5()
                        .child(caption("Proposed content", cx))
                        .child(
                            mono(serde_json::to_string_pretty(&value).unwrap_or_default(), cx)
                                .text_xs()
                                .text_color(ui::muted(cx)),
                        ),
                )
            })
            .test_support()
    }

    fn proposal(&self, cx: &mut Context<Self>) -> AnyElement {
        let ScanState::Ready(scanned) = &self.scan else {
            return self.scan_status(cx);
        };
        let plan = &scanned.plan;
        let rows = self.rows();
        let groups = grouped_items(plan);
        let pg = self.pg;
        let choice = pg.map(|choice| {
            let index = PgChoice::ALL.iter().position(|c| *c == choice).unwrap_or(0);
            v_flex()
                .id("wizard-pg-choice")
                .gap_1()
                .child(caption("PostgreSQL", cx))
                .child(
                    RadioGroup::horizontal("pg-choice")
                        .selected_index(Some(index))
                        .children(
                            PgChoice::ALL
                                .iter()
                                .map(|c| Radio::new(c.label()).label(c.label())),
                        )
                        .on_click(cx.listener(|this, ix: &usize, window, cx| {
                            let choice = PgChoice::ALL[*ix];
                            if this.pg != Some(choice) {
                                this.rescan(Some(choice), window, cx);
                            }
                        })),
                )
                .test_support()
        });
        v_flex()
            .id("wizard-proposal")
            .gap_3()
            .child(
                div().text_sm().text_color(ui::muted(cx)).child(
                    "Tick what you want. Recommended items are ticked already; nothing is written until you apply, and nothing runs.",
                ),
            )
            .children(choice)
            .children(groups.into_iter().map(|(heading, items)| {
                let icon = kind_icon(items[0].kind);
                card(cx)
                    .id(SharedString::from(format!("group-{heading}")))
                    .child(section_header(icon, heading, None, None, cx))
                    .children(items.into_iter().map(|item| {
                        let row_index = rows.iter().position(|r| *r == item.id);
                        self.item_row(plan, item, row_index, cx)
                    }))
            }))
            .children(self.pitchfork_section(plan, cx))
            .when(plan.items.is_empty(), |col| {
                col.child(ui::empty_state(
                    Lucide::ListChecks,
                    "Everything is already configured",
                    "devflow found nothing to add. You can still review the hooks in the Setup tab.",
                    None,
                ))
            })
            .children(plan.warnings.iter().map(|w| {
                div()
                    .text_xs()
                    .text_color(cx.theme().warning)
                    .child(redact::text(w))
            }))
            .test_support()
            .into_any_element()
    }

    fn diff_card(&self, file: &FilePreview, cx: &mut Context<Self>) -> AnyElement {
        const SHOWN: usize = 300;
        let change = match file.change {
            FileChange::Created => chip("will be created", Tone::Success, cx),
            FileChange::Modified => chip("will be modified", Tone::Warning, cx),
            FileChange::Unchanged => chip("already up to date", Tone::Neutral, cx),
        };
        let lines = file
            .diff
            .iter()
            .take(SHOWN)
            .enumerate()
            .map(|(index, line)| {
                let (sign, bg, color) = match line.kind {
                    DiffKind::Added => ("+", Some(cx.theme().success.opacity(0.14)), None),
                    DiffKind::Removed => ("-", Some(cx.theme().danger.opacity(0.14)), None),
                    DiffKind::Context => (" ", None, Some(ui::muted(cx))),
                    DiffKind::Gap => ("…", None, Some(ui::muted(cx))),
                };
                h_flex()
                    .id(SharedString::from(format!("diff-{}-{index}", file.file)))
                    .w_full()
                    .px_3()
                    .gap_2()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_xs()
                    .when_some(bg, |row, bg| row.bg(bg))
                    .when_some(color, |row, color| row.text_color(color))
                    .child(div().w(px(10.)).child(sign))
                    .child(div().flex_1().min_w_0().child(redact::text(&line.text)))
            });
        card(cx)
            .id(SharedString::from(format!("review-{}", file.file)))
            .child(section_header(
                Lucide::FileDiff,
                file.file.clone(),
                Some(format!("+{} −{}", file.added, file.removed).into()),
                Some(change.into_any_element()),
                cx,
            ))
            .child(v_flex().w_full().py_1().children(lines))
            .when(file.diff.len() > SHOWN, |c| {
                c.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(ui::muted(cx))
                        .child(format!(
                            "… {} more",
                            devflow_core::text::counted((file.diff.len() - SHOWN) as u64, "line")
                        )),
                )
            })
            .when(!file.conflicts.is_empty(), |c| {
                c.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(format!(
                            "Cannot be applied: {} already has a different value.",
                            file.conflicts.join(", ")
                        )),
                )
            })
            .test_support()
            .into_any_element()
    }

    fn review(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.preview {
            PreviewState::Idle | PreviewState::Loading => {
                ui::loading("Working out the changes…", cx).h(px(200.)).into_any_element()
            }
            PreviewState::Failed(message) => ui::error_banner(message.clone(), cx)
                .id("wizard-review-error")
                .into_any_element(),
            PreviewState::Ready(files) if files.is_empty() => ui::empty_state(
                Lucide::ListChecks,
                "Nothing to write",
                "No item is ticked. Go back to tick some, or continue to review what the project already runs.",
                None,
            )
            .into_any_element(),
            PreviewState::Ready(files) => v_flex()
                .id("wizard-review")
                .gap_3()
                .child(
                    div().text_sm().text_color(ui::muted(cx)).child(
                        "These are the exact changes. Lines in your files stay as they are; comments and formatting are kept.",
                    ),
                )
                .children(files.iter().map(|f| self.diff_card(f, cx)))
                .test_support()
                .into_any_element(),
        }
    }

    fn applying(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.apply {
            ApplyState::Idle | ApplyState::Running => v_flex()
                .id("wizard-applying")
                .h(px(200.))
                .items_center()
                .justify_center()
                .gap_3()
                .child(Spinner::new())
                .child(
                    div()
                        .text_sm()
                        .text_color(ui::muted(cx))
                        .child("Writing the files…"),
                )
                .test_support()
                .into_any_element(),
            ApplyState::Stale(why) => v_flex()
                .id("wizard-stale")
                .gap_3()
                .child(banner(
                    Tone::Warning,
                    Lucide::TriangleAlert,
                    format!("This plan no longer fits the repository: {why}"),
                    Some(
                        Button::new("wizard-replan")
                            .primary()
                            .label("Plan again")
                            .on_click(cx.listener(|this, _, window, cx| this.replan(window, cx))),
                    ),
                    cx,
                ))
                .test_support()
                .into_any_element(),
            ApplyState::Failed(message) => {
                v_flex()
                    .id("wizard-apply-failed")
                    .gap_3()
                    .child(ui::error_banner(message.clone(), cx))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("wizard-retry-apply")
                                    .primary()
                                    .label("Try again")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.start_apply(window, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(Button::new("wizard-replan").label("Plan again").on_click(
                                cx.listener(|this, _, window, cx| this.replan(window, cx)),
                            )),
                    )
                    .test_support()
                    .into_any_element()
            }
            ApplyState::Done(report) => {
                let rows = report.files.iter().map(|file| {
                    let (label, tone) = match file.status {
                        FileStatus::Created => ("created", Tone::Success),
                        FileStatus::Modified => ("modified", Tone::Success),
                        FileStatus::AlreadyApplied => ("already applied", Tone::Neutral),
                    };
                    h_flex()
                        .id(SharedString::from(format!("applied-{}", file.file)))
                        .px_3()
                        .py_1p5()
                        .gap_3()
                        .items_center()
                        .border_b_1()
                        .border_color(cx.theme().border.opacity(0.5))
                        .child(
                            Icon::new(IconName::CircleCheck)
                                .size_4()
                                .text_color(ui::tone_color(Tone::Success, cx)),
                        )
                        .child(mono(file.file.clone(), cx).flex_1())
                        .child(chip(label, tone, cx))
                        .test_support()
                });
                v_flex()
                    .id("wizard-applied")
                    .gap_3()
                    .child(div().text_sm().child(format!(
                        "{} item(s) are in your files. {}",
                        report.applied.len(),
                        if report.resumed {
                            "An interrupted apply was finished."
                        } else if report.already_applied {
                            "They were already there."
                        } else {
                            "Nothing was installed, approved or run."
                        }
                    )))
                    .child(card(cx).children(rows))
                    .when(!report.manual.is_empty(), |col| {
                        col.child(
                            div()
                                .text_sm()
                                .text_color(ui::muted(cx))
                                .child(format!("To do by hand: {}.", report.manual.join(", "))),
                        )
                    })
                    .test_support()
                    .into_any_element()
            }
        }
    }

    fn done(&self, cx: &mut Context<Self>) -> AnyElement {
        let committed = match &self.apply {
            ApplyState::Done(report) => report
                .files
                .iter()
                .filter(|f| f.status != FileStatus::AlreadyApplied)
                .map(|f| f.file.clone())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let step = |icon: Lucide, text: String, cx: &App| {
            h_flex()
                .gap_3()
                .items_start()
                .child(Icon::new(icon).size_4().text_color(ui::muted(cx)))
                .child(div().text_sm().child(text))
        };
        v_flex()
            .id("wizard-done")
            .gap_4()
            .child(banner(
                if self.approved { Tone::Success } else { Tone::Warning },
                if self.approved { Lucide::ShieldCheck } else { Lucide::ShieldAlert },
                if self.approved {
                    "devflow is set up and what it runs is approved."
                } else {
                    "devflow is set up. What it runs is not approved yet: new workspaces skip the hooks until you approve them in the Setup tab."
                },
                None,
                cx,
            ))
            .child(
                v_flex()
                    .gap_2()
                    .child(caption("Next", cx))
                    .when(!committed.is_empty(), |col| {
                        col.child(step(
                            Lucide::FolderGit2,
                            format!("Review and commit {}.", committed.join(", ")),
                            cx,
                        ))
                    })
                    .child(step(
                        Lucide::SlidersHorizontal,
                        "Adjust hooks and variables any time in the Setup tab.".into(),
                        cx,
                    ))
                    .child(step(
                        Lucide::GitBranch,
                        "Create a workspace: it gets its own worktree, files and database.".into(),
                        cx,
                    )),
            )
            .child(
                Button::new("wizard-first-workspace")
                    .primary()
                    .icon(IconName::Plus)
                    .label("Create your first workspace")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.create_first_workspace(window, cx)
                    })),
            )
            .test_support()
            .into_any_element()
    }

    fn body(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.step {
            Step::Detected => self.detected(cx),
            Step::Proposal => self.proposal(cx),
            Step::Review => self.review(cx),
            Step::Apply => self.applying(cx),
            Step::Approve => match &self.approval {
                Some(approval) => approval.clone().into_any_element(),
                None => div().into_any_element(),
            },
            Step::Done => self.done(cx),
        }
    }

    fn primary_label(&self) -> &'static str {
        match self.step {
            Step::Detected => "Next",
            Step::Proposal => "Review changes",
            Step::Review => "Apply",
            Step::Apply => "Next",
            Step::Approve => {
                if self.approved {
                    "Continue"
                } else {
                    "Skip for now"
                }
            }
            Step::Done => "Close",
        }
    }

    fn primary_enabled(&self) -> bool {
        match self.step {
            Step::Proposal => matches!(self.scan, ScanState::Ready(_)),
            Step::Done => true,
            _ => self.can_advance(),
        }
    }
}

impl Render for OnboardingView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let step = self.step;
        let can_back = self.can_go_back();
        let stepper = Stepper::new("wizard-steps")
            .selected_index(step.index())
            .items(
                Step::ALL
                    .iter()
                    .map(|s| StepperItem::new().child(s.title())),
            )
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                let target = Step::ALL[*ix];
                // Back to a screen already passed; forward only with Next.
                if target < this.step
                    && this.can_go_back()
                    && !matches!(this.apply, ApplyState::Done(_))
                {
                    this.step = target;
                    cx.notify();
                }
            }));
        let body = self.body(cx);
        let hint = match step {
            Step::Proposal => "↑↓ move · space toggle · ⏎ next · ← back",
            _ => "⏎ next · ← back · esc close",
        };
        v_flex()
            .id("setup-wizard")
            .key_context("SetupWizard")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &WizardNext, window, cx| {
                if this.primary_enabled() {
                    this.go_next(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &WizardBack, window, cx| this.go_back(window, cx)))
            .on_action(cx.listener(|this, _: &WizardUp, _, cx| {
                if this.step == Step::Proposal {
                    this.cursor = this.cursor.saturating_sub(1);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &WizardDown, _, cx| {
                if this.step == Step::Proposal {
                    let last = this.rows().len().saturating_sub(1);
                    this.cursor = (this.cursor + 1).min(last);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &WizardToggle, _, cx| {
                if this.step != Step::Proposal {
                    return;
                }
                let rows = this.rows();
                let Some(id) = rows.get(this.cursor).cloned() else {
                    return;
                };
                if let ScanState::Ready(scanned) = &this.scan {
                    let plan = scanned.plan.clone();
                    toggle(&plan, &mut this.selected, &id);
                }
                cx.notify();
            }))
            .w_full()
            .h(px(580.))
            .gap_3()
            .child(stepper)
            .child(
                div()
                    .id("wizard-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body)
                    .test_support(),
            )
            .child(
                h_flex()
                    .pt_2()
                    .gap_2()
                    .items_center()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(ui::muted(cx))
                            .child(hint),
                    )
                    .when(step != Step::Done, |row| {
                        row.child(
                            Button::new("wizard-cancel").label("Cancel").on_click(
                                cx.listener(|this, _, window, cx| this.close(window, cx)),
                            ),
                        )
                    })
                    .child(
                        Button::new("wizard-back")
                            .label("Back")
                            .disabled(!can_back)
                            .on_click(cx.listener(|this, _, window, cx| this.go_back(window, cx))),
                    )
                    .child(
                        Button::new("wizard-next")
                            .primary()
                            .label(self.primary_label())
                            .disabled(!self.primary_enabled())
                            .on_click(cx.listener(|this, _, window, cx| this.go_next(window, cx))),
                    ),
            )
    }
}

// ---- approving ------------------------------------------------------------------------

/// What the person is asked to approve.
pub struct ReviewSection {
    pub id: String,
    pub title: String,
    pub lines: Vec<String>,
    pub warning: bool,
}

/// The blocks of an approval's scope as shown to the person: the hook phases
/// that have commands, the timeout, `[env]` and the pitchfork processes, with
/// credentials masked.
pub fn review_sections(scope: &Scope) -> Vec<ReviewSection> {
    let mut processes = 0;
    scope
        .sections()
        .into_iter()
        .filter(|s| !matches!(s.id, "setup" | "switch" | "teardown") || !s.lines.is_empty())
        .map(|s| ReviewSection {
            id: match s.id {
                "processes" => {
                    processes += 1;
                    format!("processes-{processes}")
                }
                other => other.to_string(),
            },
            title: s.title,
            lines: s.lines.iter().map(|l| redact::text(l)).collect(),
            warning: s.warning,
        })
        .collect()
}

/// Approve exactly `scope` as the main checkout's: the one place the wizard
/// calls the approval API.
fn approve_main_checkout(project: &Project, scope: &Scope) -> anyhow::Result<()> {
    trust::approve_scope(project, scope)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalEvent {
    Approved,
}

#[derive(Clone)]
enum ApprovalStatus {
    Loading,
    Ready,
    /// No hook, no `[env]`, no pitchfork configuration.
    NothingToApprove,
    Approved,
    Failed(String),
}

/// The scope an approval covers, with the button that approves it.
pub struct ApprovalView {
    state: Entity<AppState>,
    root: PathBuf,
    status: ApprovalStatus,
    loaded: Option<(Arc<Project>, Scope)>,
    previously_trusted: bool,
    working: bool,
    error: Option<String>,
}

impl EventEmitter<ApprovalEvent> for ApprovalView {}

impl ApprovalView {
    pub fn new(
        state: Entity<AppState>,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            state,
            root,
            status: ApprovalStatus::Loading,
            loaded: None,
            previously_trusted: false,
            working: false,
            error: None,
        };
        this.load(window, cx);
        this
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    jobs::catch(|| {
                        let project = Project::open(&root)?;
                        let scope = trust::scope(&project);
                        let state = trust::state(&project)?;
                        Ok((Arc::new(project), scope, state))
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                match loaded {
                    Ok((project, scope, state)) => {
                        this.status = match &state {
                            TrustState::NothingToApprove => ApprovalStatus::NothingToApprove,
                            TrustState::Trusted => ApprovalStatus::Approved,
                            TrustState::Untrusted { .. } => ApprovalStatus::Ready,
                        };
                        this.previously_trusted = matches!(
                            state,
                            TrustState::Untrusted {
                                previously_trusted: true,
                                ..
                            }
                        );
                        this.loaded = Some((project, scope));
                    }
                    Err(message) => this.status = ApprovalStatus::Failed(message),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn approve(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((project, scope)) = self.loaded.clone() else {
            return;
        };
        if self.working {
            return;
        }
        self.working = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    jobs::catch_typed(|| approve_main_checkout(&project, &scope))
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.working = false;
                match result {
                    Ok(()) => {
                        this.status = ApprovalStatus::Approved;
                        cx.emit(ApprovalEvent::Approved);
                        this.state.update(cx, |s, cx| s.refresh(cx));
                    }
                    Err(failure) => {
                        // What the project runs may have changed since it was
                        // shown: read it again and show the new content.
                        this.error = Some(if failure.kind == FailureKind::Other {
                            format!("Nothing was approved: {}", failure.message)
                        } else {
                            failure.message
                        });
                        this.load(window, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    #[cfg(test)]
    pub(super) fn is_approved(&self) -> bool {
        matches!(self.status, ApprovalStatus::Approved)
    }

    #[cfg(test)]
    pub(super) fn is_ready(&self) -> bool {
        matches!(self.status, ApprovalStatus::Ready)
    }
}

impl Render for ApprovalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status.clone();
        let sections = self
            .loaded
            .as_ref()
            .map(|(_, scope)| review_sections(scope))
            .unwrap_or_default();
        let intro = if self.previously_trusted {
            "What this project runs changed since you approved it. Approving covers everything below."
        } else {
            "Hooks run shell commands as you, [env] sets variables for every process that loads .devflow/env, and pitchfork processes run as listed. Approving covers everything below."
        };
        v_flex()
            .id("wizard-approve")
            .gap_3()
            .child(match &status {
                ApprovalStatus::Loading => ui::loading("Reading what the project runs…", cx)
                    .h(px(160.))
                    .into_any_element(),
                ApprovalStatus::Failed(message) => ui::error_banner(message.clone(), cx).into_any_element(),
                ApprovalStatus::NothingToApprove => banner(
                    Tone::Success,
                    Lucide::ShieldCheck,
                    "Nothing to approve: no hooks, [env] or pitchfork processes are configured yet.",
                    None,
                    cx,
                )
                .id("approve-nothing")
                .test_support()
                .into_any_element(),
                ApprovalStatus::Approved => banner(
                    Tone::Success,
                    Lucide::ShieldCheck,
                    "Approved. New workspaces run these on this machine.",
                    None,
                    cx,
                )
                .id("approve-done")
                .test_support()
                .into_any_element(),
                ApprovalStatus::Ready => div().text_sm().text_color(ui::muted(cx)).child(intro).into_any_element(),
            })
            .children(self.error.clone().map(|e| ui::error_banner(e, cx).id("approve-error").test_support()))
            .children(sections.into_iter().map(|section| {
                let tone = if section.warning { Tone::Danger } else { Tone::Neutral };
                v_flex()
                    .id(SharedString::from(format!("approve-section-{}", section.id)))
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui::tone_color(tone, cx))
                            .child(section.title.clone()),
                    )
                    .children(section.lines.iter().map(|line| {
                        mono(line.clone(), cx)
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(cx.theme().muted)
                            .text_xs()
                    }))
                    .test_support()
            }))
            .when(matches!(status, ApprovalStatus::Ready), |col| {
                col.child(
                    h_flex().justify_end().child(
                        Button::new("approve-hooks")
                            .primary()
                            .icon(Lucide::ShieldCheck)
                            .label("Approve")
                            .loading(self.working)
                            .disabled(self.working)
                            .on_click(cx.listener(|this, _, window, cx| this.approve(window, cx))),
                    ),
                )
            })
            .test_support()
    }
}

/// The "Review & approve" dialog of the Setup tab. `on_approved` runs once
/// the approval is stored (the tab reads its trust state again).
pub fn open_review(
    state: &Entity<AppState>,
    on_approved: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(root) = state
        .read(cx)
        .selection
        .current()
        .map(|m| m.entry.root.clone())
    else {
        return;
    };
    let view = cx.new(|cx| ApprovalView::new(state.clone(), root, window, cx));
    window
        .subscribe(&view, cx, move |_, event: &ApprovalEvent, window, cx| {
            if matches!(event, ApprovalEvent::Approved) {
                window.close_dialog(cx);
                window.push_notification(
                    Notification::success(
                        "Approved. New workspaces run these hooks and variables.",
                    ),
                    cx,
                );
                on_approved(window, cx);
            }
        })
        .detach();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title("Review and approve").w(px(640.)).child(
            div()
                .id("review-scroll")
                .max_h(px(480.))
                .overflow_y_scroll()
                .child(view.clone())
                .test_support(),
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` also exports a `test` attribute; these are plain tests.
    use devflow_core::propose::{FilePatch, PatchOp};
    use serde_json::json;
    use std::prelude::v1::test;

    fn item(
        id: &str,
        kind: ItemKind,
        confidence: Confidence,
        depends: &[&str],
        conflicts: &[&str],
    ) -> Item {
        Item {
            id: id.into(),
            kind,
            title: id.into(),
            value: json!(null),
            confidence,
            evidence: vec![format!("{id}.file:key")],
            risks: vec![],
            depends_on: depends.iter().map(|s| s.to_string()).collect(),
            conflicts: conflicts.iter().map(|s| s.to_string()).collect(),
            patch: FilePatch {
                file: "devflow.toml".into(),
                ops: vec![PatchOp::Table {
                    path: vec![id.into()],
                    comment: None,
                    dotted: false,
                }],
            },
        }
    }

    fn plan(items: Vec<Item>) -> Proposal {
        Proposal {
            proposal_version: 1,
            schema_version: 1,
            input_hashes: Default::default(),
            items,
            already_configured: vec![],
            warnings: vec![],
        }
    }

    #[test]
    fn ticking_an_item_brings_what_it_needs_and_unticking_drops_what_needs_it() {
        let plan = plan(vec![
            item(
                "project:init",
                ItemKind::Project,
                Confidence::High,
                &[],
                &[],
            ),
            item(
                "service:db",
                ItemKind::Service,
                Confidence::High,
                &["project:init"],
                &[],
            ),
            item(
                "hook:setup:migrate",
                ItemKind::Hook,
                Confidence::Medium,
                &["service:db"],
                &[],
            ),
            item("daemon:web", ItemKind::Daemon, Confidence::Low, &[], &[]),
        ]);
        let mut selected = default_selection(&plan);
        assert_eq!(
            selected.iter().map(String::as_str).collect::<Vec<_>>(),
            ["project:init", "service:db"],
            "high-confidence items are ticked by default"
        );
        toggle(&plan, &mut selected, "hook:setup:migrate");
        assert!(selected.contains("hook:setup:migrate"));
        toggle(&plan, &mut selected, "project:init");
        assert!(
            selected.is_empty(),
            "{selected:?}: everything above it went too"
        );
        // Ticking a deep item ticks the chain.
        toggle(&plan, &mut selected, "hook:setup:migrate");
        assert_eq!(selected.len(), 3);
        assert_eq!(
            accepted(&plan, &selected),
            ["project:init", "service:db", "hook:setup:migrate"]
        );
        toggle(&plan, &mut selected, "daemon:web");
        toggle(&plan, &mut selected, "daemon:web");
        assert!(!selected.contains("daemon:web"));
        // An unknown id changes nothing.
        let before = selected.clone();
        toggle(&plan, &mut selected, "nope");
        assert_eq!(selected, before);
    }

    #[test]
    fn items_that_exclude_each_other_and_items_with_a_conflict_in_a_file() {
        let blocked = item(
            "env:preset:prisma",
            ItemKind::Env,
            Confidence::High,
            &[],
            &["[env] already sets DIRECT_URL"],
        );
        let plan = plan(vec![
            item(
                "daemon:web",
                ItemKind::Daemon,
                Confidence::Low,
                &[],
                &["daemon:app"],
            ),
            item(
                "daemon:app",
                ItemKind::Daemon,
                Confidence::Low,
                &[],
                &["daemon:web"],
            ),
            blocked,
        ]);
        let mut selected = BTreeSet::new();
        toggle(&plan, &mut selected, "daemon:web");
        toggle(&plan, &mut selected, "daemon:app");
        assert_eq!(
            selected.iter().collect::<Vec<_>>(),
            ["daemon:app"],
            "one of two"
        );
        // An item with a description of what stands in its way cannot be ticked.
        toggle(&plan, &mut selected, "env:preset:prisma");
        assert!(!selected.contains("env:preset:prisma"));
        assert!(!selectable(&plan, plan.item("env:preset:prisma").unwrap()));
        assert!(!default_selection(&plan).contains("env:preset:prisma"));
    }

    #[test]
    fn items_are_grouped_under_their_headings_in_plan_order() {
        let plan = plan(vec![
            item("hook:setup:a", ItemKind::Hook, Confidence::High, &[], &[]),
            item("service:db", ItemKind::Service, Confidence::High, &[], &[]),
            item("hook:setup:b", ItemKind::Hook, Confidence::High, &[], &[]),
        ]);
        let groups = grouped_items(&plan);
        let shape: Vec<_> = groups
            .iter()
            .map(|(h, items)| (*h, items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>()))
            .collect();
        assert_eq!(
            shape,
            [
                ("Hooks", vec!["hook:setup:a", "hook:setup:b"]),
                ("Services", vec!["service:db"]),
            ]
        );
    }

    #[test]
    fn the_autostart_item_is_a_process_item_like_any_other() {
        // `pitchfork:autostart` is a Daemon item with no special case in the wizard.
        let plan = plan(vec![
            item("daemon:web", ItemKind::Daemon, Confidence::Medium, &[], &[]),
            item(
                "pitchfork:autostart",
                ItemKind::Daemon,
                Confidence::Medium,
                &["daemon:web"],
                &[],
            ),
        ]);
        let groups = grouped_items(&plan);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "Processes (pitchfork)");
        let ids: Vec<_> = groups[0].1.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["daemon:web", "pitchfork:autostart"]);
        assert!(OnboardingView::involves_processes(&plan));
        // Ticking it brings the daemon it depends on, like any dependency.
        let mut selected = BTreeSet::new();
        toggle(&plan, &mut selected, "pitchfork:autostart");
        assert!(selected.contains("daemon:web") && selected.contains("pitchfork:autostart"));
        assert_eq!(kind_icon(ItemKind::Daemon), Lucide::Server);
    }

    /// A service item as the planner writes it for a Valkey, Redis or MySQL
    /// compose service.
    fn container(key: &str, engine: &str, confidence: Confidence, preview: bool) -> Item {
        let mut item = item(
            &format!("service:{key}"),
            ItemKind::Service,
            confidence,
            &["project:init"],
            &[],
        );
        item.value = if preview {
            json!({"key": key, "engine": engine, "image": "mysql:8.4", "preview": true})
        } else {
            json!({"key": key, "engine": engine, "image": "redis:7"})
        };
        item
    }

    #[test]
    fn container_services_are_service_items_like_the_database() {
        let plan = plan(vec![
            item(
                "project:init",
                ItemKind::Project,
                Confidence::High,
                &[],
                &[],
            ),
            container("cache", "redis", Confidence::High, false),
            container("mysql", "mysql", Confidence::Medium, true),
        ]);
        // One heading, in plan order, with the database's icon.
        let groups = grouped_items(&plan);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].0, "Services");
        assert_eq!(
            groups[1]
                .1
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            ["service:cache", "service:mysql"]
        );
        assert_eq!(kind_icon(ItemKind::Service), Lucide::Database);
        // Valkey and Redis are ticked by default; MySQL (a preview) is not.
        let mut selected = default_selection(&plan);
        assert!(selected.contains("service:cache"), "{selected:?}");
        assert!(!selected.contains("service:mysql"), "{selected:?}");
        assert!(is_preview(plan.item("service:mysql").unwrap()));
        assert!(!is_preview(plan.item("service:cache").unwrap()));
        // Ticking MySQL brings the file the plan creates; both are applied.
        toggle(&plan, &mut selected, "service:mysql");
        assert_eq!(
            accepted(&plan, &selected),
            ["project:init", "service:cache", "service:mysql"]
        );
    }

    #[test]
    fn the_postgres_choice_is_about_the_postgres_item_only() {
        let postgres = |mode: &str| {
            let mut item = item("service:db", ItemKind::Service, Confidence::High, &[], &[]);
            item.value = json!({"key": "db", "engine": "postgres", "mode": mode});
            item
        };
        // A cache ahead of the database does not hide the choice or change it.
        let plan_with = |items: Vec<Item>| plan(items);
        let cache = container("cache", "redis", Confidence::High, false);
        assert_eq!(
            PgChoice::of_plan(&plan_with(vec![cache.clone(), postgres("external")])),
            Some(PgChoice::External)
        );
        assert_eq!(
            PgChoice::of_plan(&plan_with(vec![postgres("managed"), cache.clone()])),
            Some(PgChoice::Managed)
        );
        // No database proposed: the choice reads "No database", not the cache's mode.
        assert_eq!(
            PgChoice::of_plan(&plan_with(vec![cache])),
            Some(PgChoice::None)
        );
        // The fixtures of the other tests have no engine in their value: postgres.
        assert!(is_postgres_service(&item(
            "service:db",
            ItemKind::Service,
            Confidence::High,
            &[],
            &[]
        )));
    }

    #[test]
    fn steps_follow_each_other_and_have_titles() {
        assert_eq!(Step::Detected.previous(), None);
        assert_eq!(Step::Detected.next(), Some(Step::Proposal));
        assert_eq!(Step::Done.next(), None);
        assert_eq!(Step::Apply.index(), 3);
        let titles: Vec<_> = Step::ALL.iter().map(|s| s.title()).collect();
        assert_eq!(
            titles,
            ["Detected", "Proposal", "Review", "Apply", "Approve", "Done"]
        );
    }

    #[test]
    fn names_of_facts_read_like_names() {
        use devflow_core::detect::{Engine, Framework, MigrationTool, PackageManager};
        assert_eq!(pretty(&Framework::NextJs), "Next.js");
        assert_eq!(pretty(&Framework::SpringBoot), "Spring Boot");
        assert_eq!(pretty(&Framework::Rails), "Rails");
        assert_eq!(pretty(&MigrationTool::GolangMigrate), "golang-migrate");
        assert_eq!(pretty(&Engine::Postgres), "PostgreSQL");
        assert_eq!(pretty(&PackageManager::Pnpm), "pnpm");
        assert_eq!(pretty(&PackageManager::Bun), "Bun");
    }

    #[test]
    fn a_scanned_repository_becomes_friendly_chips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"shop","scripts":{"dev":"next dev"},"dependencies":{"next":"15.1.0","@prisma/client":"6.1.0"},"devDependencies":{"prisma":"6.1.0"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("bun.lock"), "{}\n").unwrap();
        std::fs::write(
            root.join("compose.yaml"),
            "services:\n  db:\n    image: postgres:16\n  cache:\n    image: redis:7\n",
        )
        .unwrap();
        std::fs::write(root.join(".env"), "DATABASE_URL=SENTINEL_SECRET_VALUE\n").unwrap();
        std::fs::write(root.join("mise.toml"), "[tools]\nnode = \"22\"\n").unwrap();
        let facts = detect::scan(&root).unwrap();
        let groups = chip_groups(&facts);
        let find = |label: &str| -> Vec<String> {
            groups
                .iter()
                .find(|g| g.label == label)
                .map(|g| g.chips.clone())
                .unwrap_or_default()
        };
        assert!(
            find("Languages and frameworks").contains(&"Next.js".to_string()),
            "{groups:?}"
        );
        assert!(
            find("Package managers").contains(&"Bun".to_string()),
            "{groups:?}"
        );
        assert!(
            find("Databases and migrations").contains(&"Prisma".to_string()),
            "{groups:?}"
        );
        let compose = find("Compose services");
        assert!(
            compose.iter().any(|c| c.starts_with("db (PostgreSQL")),
            "{compose:?}"
        );
        assert!(
            compose.iter().any(|c| c.starts_with("cache (Redis")),
            "{compose:?}"
        );
        let tooling = find("Tooling and files").join(" | ");
        assert!(
            tooling.contains("mise: node") && tooling.contains("1 .env file"),
            "{tooling}"
        );
        // No value is ever shown.
        let everything = format!("{groups:?}");
        assert!(!everything.contains("SENTINEL_SECRET_VALUE"));
    }

    #[test]
    fn the_review_masks_credentials_and_drops_empty_phases() {
        let config = devflow_core::Config::parse(
            "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n\n[hooks]\nsetup = [\"psql postgres://u:hunter2@h/db -f seed.sql\"]\n\n[env]\nA = \"${PGHOST}\"\n", // kingfisher:ignore
        )
        .unwrap();
        let scope = Scope::build(&config, Path::new("/nonexistent"));
        let sections = review_sections(&scope);
        let ids: Vec<_> = sections.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["setup", "timeout", "env"]);
        let all: String = sections
            .iter()
            .flat_map(|s| s.lines.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("postgres://u:***@h/db"), "{all}");
        assert!(!all.contains("hunter2"), "{all}");
    }

    #[test]
    fn the_wizards_review_masks_credential_named_variables_by_name() {
        let config = devflow_core::Config::parse(
            "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n\n[env]\nDB_PASS = \"lit-1\"\nSESSION_KEY = \"lit-2\"\nAPP_LABEL = \"app\"\n", // kingfisher:ignore
        )
        .unwrap();
        let scope = Scope::build(&config, Path::new("/nonexistent"));
        let env = review_sections(&scope)
            .into_iter()
            .find(|s| s.id == "env")
            .unwrap();
        assert_eq!(
            env.lines,
            ["APP_LABEL = app", "DB_PASS = ***", "SESSION_KEY = ***"]
        );
    }
}
