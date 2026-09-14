//! In-process Jujutsu storage and working-copy operations.
//!
//! Keep the synchronous VCS interface by polling jj-lib's filesystem futures
//! directly. No nested Tokio runtime, CLI output parsing, or helper programs.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use jj_lib::{
    backend::CommitId,
    commit::Commit,
    config::{ConfigLayer, ConfigResolutionContext, ConfigSource, StackedConfig},
    default_backend_factories::{default_backend_factories, default_working_copy_factories},
    fileset::{self, FilesetAliasesMap, FilesetDiagnostics, FilesetParseContext},
    git::{self, GitImportOptions},
    gitignore::GitIgnoreFile,
    lock::FileLock,
    matchers::NothingMatcher,
    object_id::ObjectId,
    op_store::RefTarget,
    ref_name::{RefName, WorkspaceName},
    repo::{ReadonlyRepo, Repo},
    repo_path::RepoPath,
    revset::{
        self, RevsetAliasesMap, RevsetDiagnostics, RevsetExpression, RevsetExtensions,
        RevsetParseContext, RevsetWorkspaceContext, SymbolResolver,
    },
    settings::{HumanByteSize, UserSettings},
    transaction::Transaction,
    ui_path::RepoPathUiConverter,
    working_copy::{SnapshotOptions, WorkingCopyFreshness},
    workspace::Workspace,
    workspace_store::{SimpleWorkspaceStore, WorkspaceStore},
};

use super::jj::JjWorkspaceEntry;

fn immutable(
    repo: &ReadonlyRepo,
    settings: &UserSettings,
    root: &Path,
    workspace: &WorkspaceName,
    commit: &Commit,
) -> Result<bool> {
    let mut aliases = RevsetAliasesMap::new();
    // Match jj's default immutability policy while honoring user aliases.
    for (name, value) in [
        ("trunk()", "latest(remote_bookmarks(exact:'main', exact:'origin') | remote_bookmarks(exact:'master', exact:'origin') | remote_bookmarks(exact:'trunk', exact:'origin') | remote_bookmarks(exact:'main', exact:'upstream') | remote_bookmarks(exact:'master', exact:'upstream') | remote_bookmarks(exact:'trunk', exact:'upstream') | root())"),
        ("builtin_immutable_heads()", "trunk() | tags() | untracked_remote_bookmarks() | untracked_remote_tags()"),
        ("immutable_heads()", "builtin_immutable_heads()"),
        ("immutable()", "::(immutable_heads() | root())"),
        ("mutable()", "~immutable()"),
    ] { aliases.insert(name, value, None)?; }
    for key in settings.table_keys("revset-aliases") {
        aliases.insert(key, &settings.get_string(["revset-aliases", key])?, None)?;
    }
    let converter = RepoPathUiConverter::Fs {
        cwd: root.into(),
        base: root.into(),
    };
    let extensions = RevsetExtensions::default();
    let context = RevsetParseContext {
        aliases_map: &aliases,
        local_variables: HashMap::new(),
        user_email: settings.user_email(),
        date_pattern_context: chrono::Local::now().into(),
        default_ignored_remote: Some(git::REMOTE_NAME_FOR_LOCAL_GIT_REPO),
        fileset_aliases_map: &FilesetAliasesMap::new(),
        extensions: &extensions,
        workspace: Some(RevsetWorkspaceContext {
            path_converter: &converter,
            workspace_name: workspace,
        }),
    };
    let expression = revset::parse(&mut RevsetDiagnostics::new(), "immutable()", &context)?
        .intersection(&RevsetExpression::commit(commit.id().clone()));
    let resolver = SymbolResolver::new(
        repo,
        &([] as [&Box<dyn revset::SymbolResolverExtension>; 0]),
    );
    Ok(!expression
        .resolve_user_expression(repo, &resolver)?
        .evaluate(repo)?
        .is_empty()?)
}

fn load_config_path(config: &mut StackedConfig, source: ConfigSource, path: &Path) -> Result<()> {
    if path.is_dir() {
        config.load_dir(source, path)?;
    } else if path.exists() {
        config.load_file(source, path)?;
    }
    Ok(())
}

/// Read jj's secure config locations without migrating files during discovery.
fn local_config(
    config: &mut StackedConfig,
    source: ConfigSource,
    dir: &Path,
    config_root: Option<&Path>,
) -> Result<()> {
    let (id_file, legacy, kind) = if source == ConfigSource::Repo {
        ("config-id", "config.toml", "repos")
    } else {
        ("workspace-config-id", "workspace-config.toml", "workspaces")
    };
    let id_path = dir.join(id_file);
    if id_path.exists() {
        let id = std::fs::read_to_string(&id_path)?;
        anyhow::ensure!(
            id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit()),
            "Invalid jj config ID in {}",
            id_path.display()
        );
        let root = config_root.context("Cannot locate jj's user configuration directory")?;
        let location = root.join(kind).join(id);
        let metadata = jj_lib::secure_config::read_metadata(&location)?;
        anyhow::ensure!(
            jj_lib::secure_config::metadata_path(&metadata)?
                .is_some_and(|p| super::paths_equal(p, dir)),
            "Jujutsu config belongs to a different repository; run jj to reconcile {}",
            id_path.display()
        );
        load_config_path(config, source, &location.join("config.toml"))?;
    } else {
        load_config_path(config, source, &dir.join(legacy))?;
    }
    Ok(())
}

pub(super) fn settings(root: &Path, repo_path: Option<&Path>) -> Result<UserSettings> {
    let home = dirs::home_dir();
    // jj follows XDG on Unix (including macOS).
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            if cfg!(windows) {
                dirs::config_dir()
            } else {
                home.as_ref().map(|p| p.join(".config"))
            }
        });
    let jj_config = config_home.as_ref().map(|p| p.join("jj"));
    let mut config = StackedConfig::with_defaults();
    config.add_layer(ConfigLayer::parse(
        ConfigSource::Default,
        "[snapshot]\nmax-new-file-size = '1MiB'\nauto-track = 'all()'\n",
    )?);
    if std::env::var_os("JJ_CONFIG").is_none() {
        load_config_path(
            &mut config,
            ConfigSource::System,
            Path::new("/etc/jj/config.toml"),
        )?;
    }
    let paths = if let Some(paths) = std::env::var_os("JJ_CONFIG") {
        std::env::split_paths(&paths)
            .filter(|p| !p.as_os_str().is_empty())
            .collect::<Vec<_>>()
    } else {
        home.iter()
            .map(|p| p.join(".jjconfig.toml"))
            .chain(
                jj_config
                    .iter()
                    .flat_map(|p| [p.join("config.toml"), p.join("conf.d")]),
            )
            .collect()
    };
    for path in paths {
        load_config_path(&mut config, ConfigSource::User, &path)?;
    }
    if let Some(repo) = repo_path {
        local_config(&mut config, ConfigSource::Repo, repo, jj_config.as_deref())?;
        local_config(
            &mut config,
            ConfigSource::Workspace,
            &root.join(".jj"),
            jj_config.as_deref(),
        )?;
    }
    let mut overrides = ConfigLayer::empty(ConfigSource::EnvOverrides);
    for (env, key) in [("JJ_USER", "user.name"), ("JJ_EMAIL", "user.email")] {
        if let Ok(value) = std::env::var(env) {
            overrides.set_value(key, value)?;
        }
    }
    config.add_layer(overrides);
    let environment = std::env::vars().collect();
    let context = ConfigResolutionContext {
        home_dir: home.as_deref(),
        repo_path,
        workspace_path: Some(root),
        command: Some("devflow"),
        hostname: "",
        environment: &environment,
    };
    let mut config = jj_lib::config::resolve(&config, &context)?;
    // Devflow snapshots and creates workspace commits; explicit signed commits
    // remain the user's jj command. Config must not launch gpg/ssh/watchman.
    config.add_layer(ConfigLayer::parse(
        ConfigSource::CommandArg,
        "[signing]\nbackend = 'none'\nbehavior = 'drop'\n[fsmonitor]\nbackend = 'none'\n",
    )?);
    Ok(UserSettings::from_config(config)?)
}

pub(super) struct State {
    pub workspace: Workspace,
    pub repo: Arc<ReadonlyRepo>,
    pub untracked: bool,
    _git_lock: FileLock,
}

#[derive(Debug)]
struct StaleWorkingCopy;
impl std::fmt::Display for StaleWorkingCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Jujutsu working copy requires native recovery")
    }
}
impl std::error::Error for StaleWorkingCopy {}

struct SnapshotConfig {
    ignores: Arc<GitIgnoreFile>,
    matcher: Box<dyn jj_lib::matchers::Matcher>,
    limit: u64,
}
impl SnapshotConfig {
    fn options(&self) -> SnapshotOptions<'_> {
        SnapshotOptions {
            base_ignores: self.ignores.clone(),
            progress: None,
            start_tracking_matcher: self.matcher.as_ref(),
            force_tracking_matcher: &NothingMatcher,
            max_new_file_size: self.limit,
        }
    }
}

impl State {
    /// Open the recorded repository view without snapshotting or changing Git.
    pub async fn inspect(root: &Path) -> Result<Self> {
        let initial = settings(root, None)?;
        let workspace = Workspace::load(
            &initial,
            root,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .context("Could not load this Jujutsu repository with the embedded jj-lib 0.45.1")?;
        let settings = settings(root, Some(workspace.repo_path()))?;
        let workspace = Workspace::load(
            &settings,
            root,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )?;
        let lock = FileLock::lock(workspace.repo_path().join("git_import_export.lock"))?;
        let repo = workspace.repo_loader().load_at_head().await?;
        Ok(Self {
            workspace,
            repo,
            untracked: false,
            _git_lock: lock,
        })
    }

    /// Prepare for an explicit mutation, preserving local working-copy edits.
    pub async fn load(root: &Path) -> Result<Self> {
        let mut state = Self::inspect(root).await?;
        if state.colocated() {
            anyhow::ensure!(git2::Repository::open(root)?.state() == git2::RepositoryState::Clean,
                "Finish or abort the active Git operation before changing this colocated jj workspace");
        }
        state.import_git_head().await?;
        match state.snapshot().await {
            Err(error) if error.is::<StaleWorkingCopy>() => state.recover_stale().await?,
            result => result?,
        }
        state.import_git_refs().await?;
        Ok(state)
    }

    fn colocated(&self) -> bool {
        self.workspace.workspace_root().join(".git").exists()
    }

    pub fn entries(&self) -> Result<Vec<JjWorkspaceEntry>> {
        let store = SimpleWorkspaceStore::load(self.workspace.repo_path())?;
        self.repo.view().wc_commit_ids().keys().map(|name| {
            let path = store.get_workspace_path(name)?
                .with_context(|| format!("No path recorded for jj workspace '{name:?}'; open that workspace with jj to register it"))?;
            let path = self.workspace.repo_path().join(path);
            Ok(JjWorkspaceEntry { internal_name: name.as_str().to_owned(), path: path.canonicalize().unwrap_or(path) })
        }).collect()
    }

    pub fn bookmarks(&self) -> Vec<String> {
        self.repo
            .view()
            .local_bookmarks()
            .filter(|(_, target)| target.is_present())
            .map(|(name, _)| name.as_str().to_owned())
            .collect()
    }

    pub async fn working_commit(&self) -> Result<Commit> {
        self.commit_for_internal(self.workspace.workspace_name())
            .await
    }

    pub async fn commit_for_internal(&self, name: &WorkspaceName) -> Result<Commit> {
        let id = self
            .repo
            .view()
            .get_wc_commit_id(name)
            .context("Jujutsu workspace has been forgotten")?;
        Ok(self.repo.store().get_commit_async(id).await?)
    }

    pub async fn bookmark_commit(&self, name: &str) -> Result<Commit> {
        let id = self
            .repo
            .view()
            .get_local_bookmark(RefName::new(name))
            .as_normal()
            .with_context(|| format!("jj bookmark '{name}' is missing or conflicted"))?;
        Ok(self.repo.store().get_commit_async(id).await?)
    }

    pub async fn finish(&mut self, mut tx: Transaction, description: &str) -> Result<()> {
        tx.set_workspace_name(self.workspace.workspace_name());
        tx.repo_mut().rebase_descendants().await?;
        if self.colocated() {
            let id = tx
                .repo()
                .view()
                .get_wc_commit_id(self.workspace.workspace_name())
                .context("Current jj workspace is missing")?;
            let commit = tx.repo().store().get_commit_async(id).await?;
            let old = self.working_commit().await?;
            if old.parent_ids() != commit.parent_ids() {
                self.require_git_checkout_safe()?;
                git::reset_head(
                    tx.repo_mut(),
                    self.workspace.workspace_name(),
                    self.workspace.workspace_root(),
                    &commit,
                )
                .await?;
            } else if old.tree().tree_ids_and_labels() != commit.tree().tree_ids_and_labels() {
                git::update_intent_to_add(
                    self.repo.as_ref(),
                    self.workspace.workspace_root(),
                    &old.tree(),
                    &commit.tree(),
                )
                .await?;
            }
            let stats = git::export_refs(tx.repo_mut())?;
            for failure in stats.failed_bookmarks {
                log::warn!("Could not export jj bookmark to Git: {failure:?}");
            }
        }
        self.repo = tx.commit(description).await?;
        Ok(())
    }

    async fn import_git_head(&mut self) -> Result<()> {
        if !self.colocated() {
            return Ok(());
        }
        let name = self.workspace.workspace_name().to_owned();
        let mut tx = self.repo.start_transaction();
        git::import_head(tx.repo_mut(), &name, self.workspace.workspace_root()).await?;
        if !tx.repo().has_changes() {
            return Ok(());
        }
        if let Some(id) = tx.repo().git_head(&name).as_normal().cloned() {
            let head = tx.repo().store().get_commit_async(&id).await?;
            let wc = tx.repo_mut().check_out(name, &head).await?;
            let mut locked = self.workspace.start_working_copy_mutation().await?;
            locked.locked_wc().reset(&wc).await?;
            tx.repo_mut().rebase_descendants().await?;
            self.repo = tx.commit("devflow: import Git HEAD").await?;
            locked.finish(self.repo.op_id().clone()).await?;
        } else {
            self.finish(tx, "devflow: import Git HEAD").await?;
        }
        Ok(())
    }

    async fn import_git_refs(&mut self) -> Result<()> {
        if !self.colocated() {
            return Ok(());
        }
        let mut tx = self.repo.start_transaction();
        let remote_auto_track_bookmarks = self
            .workspace
            .settings()
            .remote_settings()?
            .into_iter()
            .filter_map(|(name, settings)| {
                settings.auto_track_bookmarks.map(|pattern| (name, pattern))
            })
            .map(|(name, pattern)| {
                Ok((
                    name,
                    revset::parse_string_expression(&mut RevsetDiagnostics::new(), &pattern)?
                        .to_matcher(),
                ))
            })
            .collect::<Result<_>>()?;
        git::import_refs(
            tx.repo_mut(),
            &GitImportOptions {
                abandon_unreachable_commits: self
                    .workspace
                    .settings()
                    .get_bool("git.abandon-unreachable-commits")?,
                record_synthetic_predecessors: self
                    .workspace
                    .settings()
                    .get_bool("git.record-synthetic-predecessors")?,
                remote_auto_track_bookmarks,
            },
        )
        .await?;
        if tx.repo().has_changes() {
            self.finish(tx, "devflow: import Git references").await?;
            let commit = self.working_commit().await?;
            self.workspace
                .check_out(self.repo.op_id().clone(), None, &commit)
                .await?;
        }
        Ok(())
    }

    fn base_ignores(&self) -> Result<Arc<GitIgnoreFile>> {
        let backend = git::get_git_backend(self.repo.store())?;
        let repo = backend.git_repo();
        let config = repo.config_snapshot();
        let excludes = config
            .string("core.excludesFile")
            .map(|s| String::from_utf8(s.into()))
            .transpose()?
            .map(|s| jj_lib::file_util::expand_home_path(&s))
            .or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .or_else(|| dirs::home_dir().map(|p| p.join(".config")))
                    .map(|p| p.join("git/ignore"))
            });
        let mut ignores = GitIgnoreFile::empty();
        if let Some(path) = excludes {
            ignores = ignores
                .chain_with_file(RepoPath::root(), self.workspace.workspace_root().join(path))?;
        }
        Ok(ignores.chain_with_file(
            RepoPath::root(),
            backend.git_repo_path().join("info/exclude"),
        )?)
    }

    fn snapshot_config(&self) -> Result<SnapshotConfig> {
        let ignores = self.base_ignores()?;
        let HumanByteSize(limit) = self
            .workspace
            .settings()
            .get_value_with("snapshot.max-new-file-size", TryInto::try_into)?;
        let auto_track = self
            .workspace
            .settings()
            .get_string("snapshot.auto-track")?;
        let mut aliases = FilesetAliasesMap::new();
        for key in self.workspace.settings().table_keys("fileset-aliases") {
            aliases.insert(
                key,
                &self
                    .workspace
                    .settings()
                    .get_string(["fileset-aliases", key])?,
                None,
            )?;
        }
        let paths = RepoPathUiConverter::Fs {
            cwd: self.workspace.workspace_root().to_path_buf(),
            base: self.workspace.workspace_root().to_path_buf(),
        };
        let context = FilesetParseContext {
            aliases_map: &aliases,
            path_converter: &paths,
        };
        let matcher =
            fileset::parse(&mut FilesetDiagnostics::new(), &auto_track, &context)?.to_matcher();
        Ok(SnapshotConfig {
            ignores,
            matcher,
            limit: if limit == 0 { u64::MAX } else { limit },
        })
    }

    pub async fn is_dirty(&mut self) -> Result<bool> {
        let commit = self.working_commit().await?;
        let config = self.snapshot_config()?;
        let mut locked = self.workspace.start_working_copy_mutation().await?;
        let (tree, stats) = locked.locked_wc().snapshot(&config.options()).await?;
        // Dropping the lock leaves working-copy metadata and Git state unchanged.
        Ok(!stats.untracked_paths.is_empty()
            || tree.tree_ids_and_labels()
                != commit
                    .parent_tree(self.repo.as_ref())
                    .await?
                    .tree_ids_and_labels())
    }

    fn require_git_checkout_safe(&self) -> Result<()> {
        if self.colocated() {
            let repo = git2::Repository::open(self.workspace.workspace_root())?;
            anyhow::ensure!(repo.state() == git2::RepositoryState::Clean, "Finish or abort the active Git operation before updating this colocated working copy");
            let parent = repo.head().ok().and_then(|head| head.peel_to_tree().ok());
            let mut index = repo.index()?;
            index.read(true)?;
            anyhow::ensure!(repo.diff_tree_to_index(parent.as_ref(), Some(&index), None)?.deltas().len() == 0, "Git has staged changes; preserve or commit them before updating this colocated working copy");
        }
        Ok(())
    }

    async fn recover_stale(&mut self) -> Result<()> {
        self.require_git_checkout_safe()?;
        let target = self.working_commit().await?;
        let old_op = self.workspace.working_copy().operation_id().clone();
        match self.workspace.repo_loader().load_operation(&old_op).await {
            Ok(op) => {
                // Snapshot against the actual old checkout, then let jj merge
                // concurrent operation heads and rebase those saved edits.
                self.repo = self.workspace.repo_loader().load_at(&op).await?;
                let base = self.working_commit().await?;
                self.snapshot().await?;
                let saved = self.working_commit().await?;
                let loader = self.workspace.repo_loader();
                let op = jj_lib::op_heads_store::resolve_op_heads(
                    loader.op_heads_store().as_ref(),
                    loader.op_store(),
                    async |heads| -> Result<_, jj_lib::repo::RepoLoaderError> {
                        let (merged, _) = loader
                            .merge_operations(
                                heads,
                                Some(self.workspace.workspace_name()),
                                Some("devflow: reconcile workspace edits"),
                                [],
                            )
                            .await?;
                        Ok(merged.operation().clone())
                    },
                )
                .await?;
                self.repo = loader.load_at(&op).await?;
                // Operation merging retains both revisions, but may select
                // the already-rebased WC and leave the saved edit divergent.
                // Apply the actual checkout delta explicitly to the target;
                // jj represents overlapping edits as native tree conflicts.
                let tree = jj_lib::merged_tree::MergedTree::merge(
                    jj_lib::merge::Merge::from_removes_adds(
                        [(base.tree(), "previous checkout".into())],
                        [
                            (target.tree(), "updated workspace".into()),
                            (saved.tree(), "local working-copy edits".into()),
                        ],
                    ),
                )
                .await?;
                let mut desired = target;
                if tree.tree_ids_and_labels() != desired.tree().tree_ids_and_labels()
                    || self.working_commit().await?.id() != desired.id()
                {
                    let mut tx = self.repo.start_transaction();
                    let recovered =
                        if tree.tree_ids_and_labels() != desired.tree().tree_ids_and_labels() {
                            tx.repo_mut()
                                .rewrite_commit(&desired)
                                .set_tree(tree)
                                .write()
                                .await?
                        } else {
                            desired.clone()
                        };
                    tx.repo_mut().set_wc_commit(
                        self.workspace.workspace_name().to_owned(),
                        recovered.id().clone(),
                    )?;
                    tx.repo_mut().rebase_descendants().await?;
                    self.repo = tx
                        .commit("devflow: preserve stale working-copy edits")
                        .await?;
                    desired = recovered;
                }
                if self.colocated() {
                    let mut tx = self.repo.start_transaction();
                    git::reset_head(
                        tx.repo_mut(),
                        self.workspace.workspace_name(),
                        self.workspace.workspace_root(),
                        &desired,
                    )
                    .await?;
                    if tx.repo().has_changes() {
                        self.repo = tx.commit("devflow: recover Git HEAD").await?;
                    }
                }
                let mut locked = self.workspace.start_working_copy_mutation().await?;
                anyhow::ensure!(
                    locked.locked_wc().old_tree().tree_ids_and_labels()
                        == saved.tree().tree_ids_and_labels(),
                    "Concurrent jj working-copy update; retry native recovery"
                );
                locked.locked_wc().check_out(&desired).await?;
                locked.finish(self.repo.op_id().clone()).await?;
            }
            Err(jj_lib::op_store::OpStoreError::ObjectNotFound { .. }) => {
                let name = self.workspace.workspace_name().to_owned();
                let mut locked = self.workspace.start_working_copy_mutation().await?;
                let (repo, commit) = jj_lib::working_copy::create_and_check_out_recovery_commit(
                    locked.locked_wc(),
                    &self.repo,
                    name,
                    "Recovered by devflow: previous working-copy operation was unavailable",
                )
                .await?;
                locked.finish(repo.op_id().clone()).await?;
                self.repo = repo;
                log::warn!(
                    "Preserved jj working copy in recovery commit {}",
                    commit.id().hex()
                );
            }
            Err(error) => return Err(error.into()),
        }
        self.snapshot().await
    }

    async fn snapshot(&mut self) -> Result<()> {
        let mut commit = self.working_commit().await?;
        let settings = self.workspace.settings().clone();
        let root = self.workspace.workspace_root().to_path_buf();
        let colocated = self.colocated();
        let config = self.snapshot_config()?;
        let options = config.options();
        let name = self.workspace.workspace_name().to_owned();
        let mut locked = self.workspace.start_working_copy_mutation().await?;
        match WorkingCopyFreshness::check_stale(locked.locked_wc(), &commit, &self.repo).await? {
            WorkingCopyFreshness::Fresh => {}
            WorkingCopyFreshness::Updated(op) => {
                self.repo = self.repo.reload_at(&op).await?;
                let id = self
                    .repo
                    .view()
                    .get_wc_commit_id(&name)
                    .context("Jujutsu workspace was forgotten")?;
                commit = self.repo.store().get_commit_async(id).await?;
            }
            _ => return Err(StaleWorkingCopy.into()),
        }
        let (tree, stats) = locked.locked_wc().snapshot(&options).await?;
        self.untracked = !stats.untracked_paths.is_empty();
        for (path, reason) in stats.untracked_paths {
            log::warn!("jj did not track {path:?}: {reason:?}");
        }
        if tree.tree_ids_and_labels() != commit.tree().tree_ids_and_labels() {
            let mut tx = self.repo.start_transaction();
            tx.set_is_snapshot(true);
            tx.set_workspace_name(&name);
            let is_immutable = immutable(&self.repo, &settings, &root, &name, &commit)?;
            let new = if is_immutable {
                tx.repo_mut()
                    .new_commit(vec![commit.id().clone()], tree)
                    .write()
                    .await?
            } else {
                tx.repo_mut()
                    .rewrite_commit(&commit)
                    .set_tree(tree)
                    .write()
                    .await?
            };
            tx.repo_mut()
                .set_wc_commit(name.clone(), new.id().clone())?;
            tx.repo_mut().rebase_descendants().await?;
            if colocated {
                if is_immutable {
                    // Moving to a new checkout requires a deliberate Git reset.
                    // Refuse while Git owns staged/merge state.
                    let git_repo = git2::Repository::open(&root)?;
                    anyhow::ensure!(
                        git_repo.state() == git2::RepositoryState::Clean,
                        "Finish the active Git operation before changing immutable jj history"
                    );
                    let parent = git_repo.head().ok().and_then(|h| h.peel_to_tree().ok());
                    anyhow::ensure!(git_repo.diff_tree_to_index(parent.as_ref(), None, None)?.deltas().len() == 0, "Git has staged changes; preserve them before changing immutable jj history");
                    git::reset_head(tx.repo_mut(), &name, &root, &new).await?;
                } else {
                    git::update_intent_to_add(
                        self.repo.as_ref(),
                        &root,
                        &commit.tree(),
                        &new.tree(),
                    )
                    .await?;
                }
                git::export_refs(tx.repo_mut())?;
            }
            self.repo = tx.commit("devflow: snapshot working copy").await?;
        }
        locked.finish(self.repo.op_id().clone()).await?;
        Ok(())
    }

    pub async fn verify_removal_generation(
        &self,
        name: &str,
        saved: &str,
        path: &Path,
    ) -> Result<()> {
        if let Some(id) = self.repo.view().get_wc_commit_id(WorkspaceName::new(name)) {
            let current = self.repo.store().get_commit_async(id).await?;
            let saved = CommitId::try_from_hex(saved).context("Invalid saved jj cleanup commit")?;
            let saved = self.repo.store().get_commit_async(&saved).await?;
            anyhow::ensure!(
                current.change_id() == saved.change_id(),
                "jj workspace registration belongs to another generation; cleanup receipt retained"
            );
            anyhow::ensure!(
                self.entries()?
                    .iter()
                    .any(|entry| entry.internal_name == name
                        && super::paths_equal(&entry.path, path)),
                "jj workspace registration moved to another path; cleanup receipt retained"
            );
        }
        Ok(())
    }

    pub async fn forget(&mut self, name: &str) -> Result<()> {
        let name = WorkspaceName::new(name);
        if self.repo.view().get_wc_commit_id(name).is_some() {
            let mut tx = self.repo.start_transaction();
            tx.repo_mut().remove_workspace(name).await?;
            self.finish(tx, "devflow: forget workspace").await?;
        }
        SimpleWorkspaceStore::load(self.workspace.repo_path())?.forget(&[name])?;
        Ok(())
    }

    pub async fn set_bookmark(&mut self, name: &str, commit: &Commit) -> Result<()> {
        let mut tx = self.repo.start_transaction();
        tx.repo_mut()
            .set_local_bookmark_target(RefName::new(name), RefTarget::normal(commit.id().clone()));
        self.finish(tx, "devflow: set workspace bookmark").await
    }
}
