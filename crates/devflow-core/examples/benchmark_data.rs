//! Disposable PostgreSQL measurements through core APIs, without a CLI harness.
//! cargo run -p devflow-core --example benchmark_data -- --sizes-mib 1,16 --concurrency 1,4 --output benchmark.json
use anyhow::{Context, Result};
use devflow_core::{
    application, baseline,
    config::Config,
    services::{factory, ServiceOperation},
    state::LocalStateManager,
    workspace::{self, LifecycleOptions},
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

fn lifecycle() -> LifecycleOptions {
    LifecycleOptions {
        skip_hooks: true,
        skip_processes: true,
        ..Default::default()
    }
}
fn footprint(root: &Path) -> Result<(u64, u64)> {
    let mut size = (0, 0);
    if !root.exists() {
        return Ok(size);
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            let child = footprint(&entry.path())?;
            size.0 += child.0;
            size.1 += child.1;
        } else if metadata.is_file() {
            size.0 += metadata.len();
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                size.1 += metadata.blocks() * 512;
            }
        }
    }
    Ok(size)
}
fn positive_list(value: &str) -> Result<Vec<usize>> {
    let list = value
        .split(',')
        .map(str::parse)
        .collect::<std::result::Result<Vec<usize>, _>>()?;
    anyhow::ensure!(
        list.iter().all(|n| *n > 0),
        "sizes and concurrency must be positive"
    );
    Ok(list)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut sizes = vec![1, 16, 64];
    let mut concurrency = vec![1, 4];
    let mut output = PathBuf::from("benchmark.json");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().context("expected a value after argument")?;
        match arg.as_str() {
            "--sizes-mib" => sizes = positive_list(&value)?,
            "--concurrency" => concurrency = positive_list(&value)?,
            "--output" => output = value.into(),
            _ => anyhow::bail!("unknown argument: {arg}"),
        }
    }
    let directory = tempfile::Builder::new()
        .prefix(".devflow-benchmark-")
        .tempdir_in(std::env::current_dir()?)?;
    let root = directory.path();
    let project = root.join("repo");
    std::fs::create_dir(&project)?;
    std::env::set_var("DEVFLOW_CONFIG_DIR", root.join("config"));
    std::env::set_var("DEVFLOW_PROCESS_STATE_DIR", root.join("processes"));
    devflow_core::vcs::GitRepository::init(&project)?;
    let config: Config = serde_json::from_value(json!({
        "name":"benchmark", "project_id":uuid::Uuid::new_v4().to_string(),
        "worktree":{"path_template":"../worktrees/{workspace}","copy_files":[]},
        "services":[{"name":"db","type":"local","service_type":"postgres","default":true,"auto_workspace":true,
            "local":{"data_root":root.join("data"),"image":"postgres:17"}}]
    }))?;
    std::fs::write(
        project.join(".devflow.yml"),
        serde_json::to_vec_pretty(&config)?,
    )?;
    let mut config = config;
    config.project_root = Some(project.clone());
    let measured = measure(&config, &project, root, &sizes, &concurrency, &output).await;
    // Preserve the fixture and its exact ownership state if cleanup fails.
    let cleanup = async {
        let provider = factory::resolve_provider(&config, Some("db")).await?;
        provider.provider.destroy_project().await?;
        anyhow::Ok(())
    }
    .await;
    if let Err(error) = cleanup {
        let preserved = directory.keep();
        anyhow::bail!("benchmark cleanup failed; fixture retained at {}: {error:#}; measurement result: {measured:?}", preserved.display());
    }
    measured?;
    println!("Wrote measurements to {}", output.display());
    Ok(())
}

async fn measure(
    config: &Config,
    project: &Path,
    root: &Path,
    sizes: &[usize],
    concurrency: &[usize],
    output: &Path,
) -> Result<()> {
    let options = application::SwitchOptions {
        lifecycle: lifecycle(),
        ..Default::default()
    };
    let initial = application::switch_workspace(config, project, "main", &options).await?;
    anyhow::ensure!(initial.ready(), "initial workspace failed: {initial:?}");
    let provider = factory::resolve_provider(config, Some("db")).await?;
    let key = LocalStateManager::new()?.resolve_workspace_service_key_by_dir(project, "main")?;
    let connection = provider.provider.get_connection_info(&key).await?;
    let mut measurements: Vec<Value> = Vec::new();
    for size in sizes {
        let fixture = root.join("seed.sql");
        std::fs::write(&fixture, format!("DROP TABLE IF EXISTS benchmark_data; CREATE TABLE benchmark_data AS SELECT n, repeat(md5(n::text),32) AS payload FROM generate_series(1,{}) n;\n", size * 1024))?;
        provider
            .provider
            .seed_from_source(&key, fixture.to_str().context("non-UTF8 fixture path")?)
            .await?;
        let preparation = Instant::now();
        let baseline = baseline::prepare(
            config,
            project,
            &format!("data{size}"),
            "db",
            &fixture,
            None,
            None,
        )
        .await?;
        let prepare_seconds = preparation.elapsed().as_secs_f64();
        for count in concurrency {
            for origin in ["main", &baseline.reference] {
                let names: Vec<_> = (0..*count)
                    .map(|i| format!("bench/{size}-{count}-{}-{i}", measurements.len()))
                    .collect();
                let before = footprint(&root.join("data"))?;
                let (stop, mut stopped) = tokio::sync::watch::channel(false);
                let address = (connection.host.clone(), connection.port);
                let observer = tokio::spawn(async move {
                    let mut previous = Instant::now();
                    let mut previous_available = true;
                    let mut unavailable = 0.0;
                    loop {
                        let now = Instant::now();
                        if !previous_available {
                            unavailable += now.duration_since(previous).as_secs_f64();
                        }
                        previous = now;
                        previous_available = matches!(
                            tokio::time::timeout(
                                std::time::Duration::from_millis(100),
                                tokio::net::TcpStream::connect((address.0.as_str(), address.1))
                            )
                            .await,
                            Ok(Ok(_))
                        );
                        tokio::select! {
                            _ = stopped.changed() => break,
                            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => (),
                        }
                    }
                    unavailable
                });
                let started = Instant::now();
                let runs = futures_util::future::join_all(names.iter().map(|name| async move {
                    let started = Instant::now();
                    let options = application::SwitchOptions {
                        lifecycle: lifecycle(),
                        create_if_missing: true,
                        from_workspace: Some("main".into()),
                        data_from_workspace: Some(origin.into()),
                        ..Default::default()
                    };
                    let result =
                        application::switch_workspace(config, project, name, &options).await?;
                    anyhow::ensure!(result.ready(), "workspace failed: {result:?}");
                    anyhow::Ok(started.elapsed().as_secs_f64())
                }))
                .await;
                let clone_group_seconds = started.elapsed().as_secs_f64();
                let _ = stop.send(true);
                let downtime = observer.await?;
                let clones = runs.into_iter().collect::<Result<Vec<_>>>()?;
                let mut resets = Vec::new();
                for name in &names {
                    let started = Instant::now();
                    application::service_operation(
                        config,
                        project,
                        "db",
                        name,
                        provider.provider.as_ref(),
                        ServiceOperation::Reset,
                    )
                    .await?;
                    resets.push(started.elapsed().as_secs_f64());
                }
                let after = footprint(&root.join("data"))?;
                measurements.push(json!({"dataset_mib":size,"concurrency":count,"data_origin":origin,"prepare_seconds":prepare_seconds,
                    "clone_group_seconds":clone_group_seconds,"clone_seconds":clones,"reset_seconds":resets,
                    "logical_bytes_added":after.0 as i128-before.0 as i128,"allocated_bytes_added":after.1 as i128-before.1 as i128,
                    "physical_bytes_copied":null,"source_tcp_unavailable_seconds":downtime,"probe_interval_seconds":0.1}));
                std::fs::write(
                    output,
                    serde_json::to_vec_pretty(
                        &json!({"schema_version":2,"measurement_boundary":"core_api","os":std::env::consts::OS,
                    "arch":std::env::consts::ARCH,"devflow_version":env!("CARGO_PKG_VERSION"),"measurements":measurements}),
                    )?,
                )?;
                for name in &names {
                    let result = workspace::delete::delete_workspace(
                        config,
                        project,
                        name,
                        &workspace::delete::DeleteOptions {
                            lifecycle: lifecycle(),
                            force: true,
                            ..Default::default()
                        },
                    )
                    .await?;
                    anyhow::ensure!(
                        !result.services.iter().any(|r| !r.success),
                        "workspace cleanup failed: {result:?}"
                    );
                }
            }
        }
        baseline::remove(config, project, &baseline.reference).await?;
    }
    Ok(())
}
