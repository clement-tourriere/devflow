use super::*;

#[test]
fn test_hooks_yaml_parsing_simple() {
    let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main, master]
behavior: {}
hooks:
  post-service-create:
    install: "npm ci"
    migrate: "npx prisma migrate deploy"
  post-switch:
    env: "echo DATABASE_URL=postgresql://{{ service.db.user }}@{{ service.db.host }}:{{ service.db.port }}/{{ service.db.database }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");

    let hooks = config.hooks.expect("hooks should be Some");
    assert_eq!(hooks.len(), 2);

    let post_create = hooks
        .get(&crate::hooks::HookPhase::PostServiceCreate)
        .expect("post-service-create phase should exist");
    assert_eq!(post_create.len(), 2);

    // Simple hook entries
    match post_create.get("install").unwrap() {
        crate::hooks::HookEntry::Simple(cmd) => assert_eq!(cmd, "npm ci"),
        _ => panic!("Expected Simple hook entry"),
    }
}

#[test]
fn test_hooks_yaml_parsing_extended() {
    let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior: {}
hooks:
  post-switch:
    setup:
      command: "npm run setup"
      working_dir: frontend
      condition: "file_exists:frontend/package.json"
      continue_on_error: true
      environment:
        NODE_ENV: development
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");

    let hooks = config.hooks.expect("hooks should be Some");
    let post_switch = hooks
        .get(&crate::hooks::HookPhase::PostSwitch)
        .expect("post-switch phase should exist");

    match post_switch.get("setup").unwrap() {
        crate::hooks::HookEntry::Extended(ext) => {
            assert_eq!(ext.command, "npm run setup");
            assert_eq!(ext.working_dir.as_deref(), Some("frontend"));
            assert_eq!(
                ext.condition.as_deref(),
                Some("file_exists:frontend/package.json")
            );
            assert_eq!(ext.continue_on_error, Some(true));
            assert!(ext.environment.is_some());
            assert_eq!(
                ext.environment.as_ref().unwrap().get("NODE_ENV").unwrap(),
                "development"
            );
        }
        _ => panic!("Expected Extended hook entry"),
    }
}

#[test]
fn test_no_hooks_parses_as_none() {
    let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    assert!(config.hooks.is_none());
}

#[test]
fn test_processes_yaml_parsing() {
    let yaml = r#"
processes:
  provider: pitchfork
  auto_start: true
  daemons:
    api:
      run: "npm run dev"
      required: false
      port: { expect: [3000], bump: 25 }
      ready_http: "http://127.0.0.1:3000/health"
      ready_timeout: 15
      stop_timeout: 5
      shutdown_signal: INT
      watch: ["src/**/*.ts"]
      env:
        DATABASE_URL: "{{ service['db'].url }}"
    worker:
      run: "npm run worker"
      depends: [api]
      retry: 2
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    let processes = config.processes.expect("processes should parse");
    assert_eq!(processes.provider, "pitchfork");
    assert!(processes.auto_start);
    assert_eq!(processes.daemons.len(), 2);
    assert!(!processes.daemons["api"].required);
    assert_eq!(processes.daemons["api"].ready_timeout, Some(15));
    assert_eq!(processes.daemons["api"].stop_timeout, Some(5));
    assert_eq!(
        processes.daemons["api"].shutdown_signal.as_deref(),
        Some("INT")
    );
    assert_eq!(processes.daemons["worker"].depends, vec!["api"]);
    assert_eq!(processes.daemons["worker"].retry, Some(2));
}

#[test]
fn test_multi_services_parsing() {
    let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior: {}
services:
  - name: db
    type: local
    service_type: postgres
    auto_workspace: true
    local:
      image: postgres:16
      port_range_start: 15432
  - name: analytics
    type: local
    service_type: clickhouse
    auto_workspace: true
    clickhouse:
      image: clickhouse/clickhouse-server:24
      port_range_start: 18123
      user: analytics
  - name: legacy-db
    type: local
    service_type: mysql
    auto_workspace: false
    mysql:
      image: mysql:8
      root_password: secret
      database: legacy
      user: app
      password: apppass
  - name: cache
    type: local
    service_type: generic
    auto_workspace: true
    generic:
      image: redis:7
      port_mapping: "6379:6379"
      environment:
        REDIS_MAXMEMORY: "256mb"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");

    let services = config.resolve_services();
    assert_eq!(services.len(), 4);

    // Postgres service
    assert_eq!(services[0].name, "db");
    assert_eq!(services[0].service_type, "postgres");
    assert!(services[0].auto_workspace);
    assert!(services[0].local.is_some());
    assert_eq!(
        services[0].local.as_ref().unwrap().port_range_start,
        Some(15432)
    );

    // ClickHouse service
    assert_eq!(services[1].name, "analytics");
    assert_eq!(services[1].service_type, "clickhouse");
    assert!(services[1].auto_workspace);
    let ch = services[1].clickhouse.as_ref().expect("clickhouse config");
    assert_eq!(ch.image, "clickhouse/clickhouse-server:24");
    assert_eq!(ch.port_range_start, Some(18123));
    assert_eq!(ch.user, "analytics");

    // MySQL service — auto_workspace is false
    assert_eq!(services[2].name, "legacy-db");
    assert_eq!(services[2].service_type, "mysql");
    assert!(!services[2].auto_workspace);
    let mysql = services[2].mysql.as_ref().expect("mysql config");
    assert_eq!(mysql.root_password, "secret");
    assert_eq!(mysql.database.as_deref(), Some("legacy"));
    assert_eq!(mysql.user.as_deref(), Some("app"));
    assert_eq!(mysql.password.as_deref(), Some("apppass"));

    // Generic Docker service
    assert_eq!(services[3].name, "cache");
    assert_eq!(services[3].service_type, "generic");
    assert!(services[3].auto_workspace);
    let generic = services[3].generic.as_ref().expect("generic config");
    assert_eq!(generic.image, "redis:7");
    assert_eq!(generic.port_mapping.as_deref(), Some("6379:6379"));
    assert_eq!(generic.environment.get("REDIS_MAXMEMORY").unwrap(), "256mb");
}

#[test]
fn test_clickhouse_config_defaults() {
    let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior: {}
services:
  - name: ch
    type: local
    service_type: clickhouse
    clickhouse: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    let services = config.resolve_services();
    let ch = services[0].clickhouse.as_ref().unwrap();
    assert_eq!(ch.image, "clickhouse/clickhouse-server:latest");
    assert_eq!(ch.user, "default");
    assert!(ch.password.is_none());
    assert!(ch.port_range_start.is_none());
}

#[test]
fn test_mysql_config_defaults() {
    let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior: {}
services:
  - name: mysql
    type: local
    service_type: mysql
    mysql: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    let services = config.resolve_services();
    let mysql = services[0].mysql.as_ref().unwrap();
    assert_eq!(mysql.image, "mysql:8");
    assert_eq!(mysql.root_password, "dev");
    assert!(mysql.database.is_none());
    assert!(mysql.user.is_none());
}

#[test]
fn test_generic_docker_config_parsing() {
    let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior: {}
services:
  - name: mq
    type: local
    service_type: generic
    generic:
      image: rabbitmq:3-management
      port_range_start: 15672
      environment:
        RABBITMQ_DEFAULT_USER: guest
        RABBITMQ_DEFAULT_PASS: guest
      volumes:
        - "/tmp/rabbitmq:/var/lib/rabbitmq"
      command: "rabbitmq-server"
      healthcheck: "rabbitmq-diagnostics -q ping"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    let services = config.resolve_services();
    let generic = services[0].generic.as_ref().unwrap();
    assert_eq!(generic.image, "rabbitmq:3-management");
    assert_eq!(generic.port_range_start, Some(15672));
    assert_eq!(generic.environment.len(), 2);
    assert_eq!(generic.volumes, vec!["/tmp/rabbitmq:/var/lib/rabbitmq"]);
    assert_eq!(generic.command.as_deref(), Some("rabbitmq-server"));
    assert_eq!(
        generic.healthcheck.as_deref(),
        Some("rabbitmq-diagnostics -q ping")
    );
}

#[test]
fn test_service_type_defaults_to_postgres() {
    let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior: {}
services:
  - name: mydb
    type: local
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    let services = config.resolve_services();
    assert_eq!(services[0].service_type, "postgres");
    assert!(services[0].auto_workspace); // default is true
}

#[test]
fn test_shared_provider_parses() {
    let yaml = r#"
services:
  - name: app-db
    type: shared
    service_type: postgres
    auto_workspace: true
    shared:
      image: postgres:17
      port: 5440
      template_branching: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    let services = config.resolve_services();
    assert_eq!(services[0].provider_type, "shared");
    assert_eq!(services[0].service_type, "postgres");
    let shared = services[0].shared.as_ref().expect("shared section present");
    assert_eq!(shared.image.as_deref(), Some("postgres:17"));
    assert_eq!(shared.port, Some(5440));
    assert_eq!(shared.template_branching, Some(true));
}

#[test]
fn test_devflow_toml_parses_via_from_file() {
    let toml = r#"
[git]
main_workspace = "main"

[[services]]
name = "app-db"
type = "shared"
service_type = "postgres"
auto_workspace = true

[services.shared]
image = "postgres:17"
port = 5432
template_branching = true
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devflow.toml");
    std::fs::write(&path, toml).unwrap();

    let config = Config::from_file(&path).expect("devflow.toml should parse");
    assert_eq!(config.git.main_workspace, "main");
    let services = config.resolve_services();
    assert_eq!(services[0].name, "app-db");
    assert_eq!(services[0].provider_type, "shared");
    assert_eq!(services[0].service_type, "postgres");
    let shared = services[0].shared.as_ref().expect("shared section");
    assert_eq!(shared.port, Some(5432));
    assert_eq!(shared.template_branching, Some(true));
}

#[test]
fn test_redis_service_parses() {
    let yaml = r#"
services:
  - name: cache
    service_type: redis
    auto_workspace: true
    shared:
      image: redis:7
      port: 6379
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    let services = config.resolve_services();
    assert_eq!(services[0].service_type, "redis");
    assert_eq!(services[0].shared.as_ref().unwrap().port, Some(6379));
}

#[test]
fn test_shared_clickhouse_parses() {
    let yaml = r#"
services:
  - name: analytics
    type: shared
    service_type: clickhouse
    auto_workspace: true
    shared:
      image: clickhouse/clickhouse-server:latest
      port: 8123
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    let services = config.resolve_services();
    assert_eq!(services[0].provider_type, "shared");
    assert_eq!(services[0].service_type, "clickhouse");
    assert_eq!(services[0].shared.as_ref().unwrap().port, Some(8123));
}

#[test]
fn test_rustfs_service_parses() {
    let yaml = r#"
services:
  - name: storage
    service_type: rustfs
    auto_workspace: true
    shared:
      image: rustfs/rustfs:latest
      port: 9000
      user: rustfsadmin
      password: rustfsadmin
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    let services = config.resolve_services();
    assert_eq!(services[0].service_type, "rustfs");
    let shared = services[0].shared.as_ref().expect("shared section present");
    assert_eq!(shared.port, Some(9000));
    assert_eq!(shared.user.as_deref(), Some("rustfsadmin"));
}

#[test]
fn test_auto_branch_filtering() {
    let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior: {}
services:
  - name: primary
    type: local
    auto_workspace: true
  - name: shared
    type: local
    auto_workspace: false
  - name: analytics
    type: local
    service_type: clickhouse
    auto_workspace: true
    clickhouse: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    let services = config.resolve_services();
    let auto_branch_services: Vec<_> = services.iter().filter(|b| b.auto_workspace).collect();
    assert_eq!(auto_branch_services.len(), 2);
    assert_eq!(auto_branch_services[0].name, "primary");
    assert_eq!(auto_branch_services[1].name, "analytics");
}

#[test]
fn test_plugin_service_config_parsing() {
    let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior: {}
services:
  - name: my-redis
    service_type: plugin
    auto_workspace: true
    plugin:
      path: "./plugins/devflow-redis"
      timeout: 45
      config:
        image: "redis:7-alpine"
        port: 16379
  - name: my-cache
    service_type: plugin
    plugin:
      name: memcached
      config:
        memory: 256
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    let services = config.resolve_services();
    assert_eq!(services.len(), 2);

    // First plugin service
    assert_eq!(services[0].name, "my-redis");
    assert_eq!(services[0].service_type, "plugin");
    assert!(services[0].auto_workspace);
    let plugin = services[0].plugin.as_ref().unwrap();
    assert_eq!(plugin.path.as_deref(), Some("./plugins/devflow-redis"));
    assert!(plugin.name.is_none());
    assert_eq!(plugin.timeout, 45);
    let cfg = plugin.config.as_ref().unwrap();
    assert_eq!(cfg["image"], "redis:7-alpine");
    assert_eq!(cfg["port"], 16379);

    // Second plugin service (name-based resolution)
    assert_eq!(services[1].name, "my-cache");
    assert_eq!(services[1].service_type, "plugin");
    let plugin2 = services[1].plugin.as_ref().unwrap();
    assert!(plugin2.path.is_none());
    assert_eq!(plugin2.name.as_deref(), Some("memcached"));
    assert_eq!(plugin2.timeout, 30); // default
}

#[test]
fn test_should_create_workspace_uses_workspace_filter_regex() {
    let mut config = Config::default();
    config.git.workspace_filter_regex = Some("^feature/.*".to_string());

    assert!(config.should_create_workspace("feature/auth"));
    assert!(!config.should_create_workspace("bugfix/auth"));
}

#[test]
fn test_should_create_workspace_falls_back_to_auto_create_workspace_filter() {
    let mut config = Config::default();
    config.git.workspace_filter_regex = None;
    config.git.auto_create_workspace_filter = Some("^chore/.*".to_string());

    assert!(config.should_create_workspace("chore/deps"));
    assert!(!config.should_create_workspace("feature/deps"));
}

#[test]
fn test_worktree_respect_gitignore_defaults_true() {
    let yaml = r#"
worktree:
  copy_files: [".env"]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    assert!(config.worktree.respect_gitignore);
}

#[test]
fn test_worktree_respect_gitignore_explicit_false() {
    let yaml = r#"
worktree:
  respect_gitignore: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    assert!(!config.worktree.respect_gitignore);
}

#[test]
fn test_worktree_recommended_default_includes_respect_gitignore() {
    let wt = WorktreeConfig::recommended_default();
    assert!(wt.respect_gitignore);
    assert!(!wt.copy_ignored);
}

#[test]
fn test_worktree_enabled_is_accepted_but_inert_legacy_configuration() {
    // Every pre-worktree-always-on release (init and the GUI) wrote
    // `worktree.enabled` into .devflow.yml; upgrading must not hard-break
    // those configs. The key parses, is ignored, and is never re-serialized.
    let yaml = r#"
worktree:
  enabled: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("legacy configs must keep parsing");
    assert_eq!(config.worktree.enabled, Some(false));

    let serialized = serde_yaml_ng::to_string(&config).expect("serialize");
    assert!(!serialized.contains("enabled"));

    // A truly unknown worktree key is still rejected.
    let error = serde_yaml_ng::from_str::<Config>("worktree:\n  bogus_key: 1\n").unwrap_err();
    assert!(error.to_string().contains("unknown field `bogus_key`"));
}

#[test]
fn test_partial_worktree_section_keeps_copy_files_default() {
    // A partial `worktree:` section must behave like an absent one: the
    // documented copy_files default is [.env, .env.local] either way.
    let yaml = r#"
worktree:
  copy_ignored: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    assert_eq!(
        config.worktree.copy_files,
        vec![".env".to_string(), ".env.local".to_string()]
    );
    assert_eq!(
        config.worktree.copy_files,
        WorktreeConfig::default().copy_files
    );

    // An explicit empty list still disables copying.
    let yaml = r#"
worktree:
  copy_files: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
    assert!(config.worktree.copy_files.is_empty());
}
