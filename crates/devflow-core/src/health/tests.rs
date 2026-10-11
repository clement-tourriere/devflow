use super::*;

const ID: &str = "3f2b1c7e-0b3c-4d2e-9a7a-8f6d5c4b3a21";

fn entry(root: &Path) -> Entry {
    Entry {
        id: Uuid::parse_str(ID).unwrap(),
        name: "p".into(),
        root: root.to_path_buf(),
    }
}

fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git2::Repository::init(dir.path()).unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    dir
}

fn problem(root: &Path) -> ProjectProblem {
    check(&entry(root)).problem().expect("a problem")
}

fn kinds(problem: &ProjectProblem) -> Vec<&'static str> {
    problem
        .remedies
        .iter()
        .map(|r| match r {
            Remedy::Retry => "retry",
            Remedy::ForgetProject { .. } => "forget",
            Remedy::LocateProject { .. } => "locate",
            Remedy::MigrateConfig { .. } => "migrate",
            Remedy::Init { .. } => "init",
            Remedy::OpenFile { .. } => "open_file",
            Remedy::Reveal { .. } => "reveal",
            Remedy::OpenGuide { .. } => "guide",
        })
        .collect()
}

fn current() -> String {
    format!("[project]\nname = \"p\"\nid = \"{ID}\"\n")
}

#[test]
fn a_healthy_project_has_no_problem() {
    let dir = repo(&[("devflow.toml", &current())]);
    assert_eq!(check(&entry(dir.path())), Health::Ok);
}

#[test]
fn a_missing_folder_can_be_located_or_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("blog");
    let problem = problem(&gone);
    assert_eq!(problem.kind, ProblemKind::FolderMissing);
    assert_eq!(kinds(&problem), ["locate", "forget"]);
    assert!(
        problem.message.contains("no longer exists"),
        "{}",
        problem.message
    );
    assert!(!problem.message.contains("devflow "), "{}", problem.message);

    // A file where the folder was is the same kind of problem.
    let file = dir.path().join("file");
    std::fs::write(&file, "x").unwrap();
    assert_eq!(self::problem(&file).kind, ProblemKind::FolderMissing);
}

#[test]
fn a_folder_that_is_not_a_repository_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let problem = problem(dir.path());
    assert_eq!(problem.kind, ProblemKind::NotGitCheckout);
    assert_eq!(kinds(&problem), ["locate", "reveal", "forget"]);
}

#[test]
fn a_repository_without_a_project_file_can_be_set_up() {
    let dir = repo(&[]);
    let problem = problem(dir.path());
    assert_eq!(problem.kind, ProblemKind::NotInitialized);
    assert_eq!(kinds(&problem), ["init", "forget"]);
    assert!(
        !problem.message.contains("devflow init"),
        "{}",
        problem.message
    );
}

#[test]
fn a_pre_release_configuration_can_be_migrated() {
    let dir = repo(&[(
        "devflow.toml",
        &format!("{}\n[postgres]\nmode = \"managed\"\n", current()),
    )]);
    let problem = problem(dir.path());
    assert_eq!(problem.kind, ProblemKind::PreReleaseConfig);
    assert_eq!(kinds(&problem), ["migrate", "open_file", "forget"]);
    assert!(problem.message.contains("rewrite"), "{}", problem.message);
    assert!(
        !problem.message.contains("devflow config"),
        "{}",
        problem.message
    );
}

#[test]
fn a_pre_release_configuration_that_cannot_be_rewritten_says_why() {
    let dir = repo(&[(
        "devflow.toml",
        &format!(
            "{}\n[postgres]\nmode = \"managed\"\n[services.main]\nengine = \"postgres\"\n",
            current()
        ),
    )]);
    let problem = problem(dir.path());
    assert_eq!(problem.kind, ProblemKind::PreReleaseConfig);
    assert_eq!(kinds(&problem), ["open_file", "forget"]);
    assert!(problem.message.contains("both"), "{}", problem.message);
}

#[test]
fn a_pre_release_table_in_the_local_file_is_found_there() {
    let dir = repo(&[
        ("devflow.toml", &current()),
        ("devflow.local.toml", "[postgres]\nport = 1\n"),
    ]);
    let problem = problem(dir.path());
    assert_eq!(problem.kind, ProblemKind::PreReleaseConfig);
    assert!(
        problem.message.contains("devflow.local.toml"),
        "{}",
        problem.message
    );
    assert_eq!(kinds(&problem), ["migrate", "open_file", "forget"]);
}

#[test]
fn zero_x_configuration_points_at_the_guide_and_is_never_migrated() {
    let dir = repo(&[(".devflow.yml", "version: 1\n")]);
    let legacy = problem(dir.path());
    assert_eq!(legacy.kind, ProblemKind::LegacyConfig);
    assert_eq!(kinds(&legacy), ["guide", "forget"]);

    let mixed = repo(&[("devflow.toml", &format!("{}\n[agent]\nx = 1\n", current()))]);
    let mixed = problem(mixed.path());
    assert_eq!(mixed.kind, ProblemKind::LegacyConfig);
    assert!(!kinds(&mixed).contains(&"migrate"));
    assert!(kinds(&mixed).contains(&"guide"));
}

#[test]
fn an_invalid_configuration_names_the_file_and_never_a_value() {
    let dir = repo(&[(
        "devflow.toml",
        &format!("{}\n[hooks]\nsetup = \"SECRETVALUE\"\n", current()),
    )]);
    let problem = problem(dir.path());
    assert_eq!(problem.kind, ProblemKind::InvalidConfig);
    assert_eq!(kinds(&problem), ["open_file", "retry", "forget"]);
    assert!(
        !problem.message.contains("SECRETVALUE"),
        "{}",
        problem.message
    );
    assert!(
        problem.message.contains("devflow.toml"),
        "{}",
        problem.message
    );

    let broken = repo(&[("devflow.toml", "this is = = not toml")]);
    assert_eq!(problem_of(&broken).kind, ProblemKind::InvalidConfig);
}

fn problem_of(dir: &tempfile::TempDir) -> ProjectProblem {
    problem(dir.path())
}

#[test]
fn a_folder_holding_another_project_is_reported() {
    let dir = repo(&[(
        "devflow.toml",
        "[project]\nname = \"other\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n",
    )]);
    let problem = problem(dir.path());
    assert_eq!(problem.kind, ProblemKind::WrongProject);
    assert_eq!(kinds(&problem), ["reveal", "forget"]);
    assert!(problem.message.contains("'other'"), "{}", problem.message);
}

#[test]
fn explaining_a_failed_open_uses_the_disk_and_falls_back_to_retry() {
    let dir = repo(&[]);
    let error = crate::Project::open(dir.path()).unwrap_err();
    assert_eq!(
        explain(dir.path(), &error).kind,
        ProblemKind::NotInitialized
    );

    let healthy = repo(&[("devflow.toml", &current())]);
    let transient = anyhow::anyhow!("the lock is busy");
    let explained = explain(healthy.path(), &transient);
    assert_eq!(explained.kind, ProblemKind::Unreadable);
    assert_eq!(explained.message, "the lock is busy");
    assert_eq!(kinds(&explained), ["retry", "forget"]);
}

#[test]
fn a_problem_serializes_with_the_texts_of_its_remedies() {
    let dir = tempfile::tempdir().unwrap();
    let problem = problem(&dir.path().join("gone"));
    let json = serde_json::to_value(&problem).unwrap();
    assert_eq!(json["kind"], "folder_missing");
    let forget = &json["remedies"][1];
    assert_eq!(forget["kind"], "forget_project");
    assert_eq!(forget["label"], "Remove from list");
    assert!(
        forget["command"]
            .as_str()
            .unwrap()
            .starts_with("devflow project forget ")
    );
    assert_eq!(forget["destructive"], true);
}
