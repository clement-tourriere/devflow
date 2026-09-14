use anyhow::{anyhow, Context, Result};
use bollard::exec::StartExecOptions;
use bollard::models::{ContainerCreateBody, ExecConfig, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptions, UploadToContainerOptions, WaitContainerOptions,
};
use bollard::Docker;
use futures_util::{StreamExt, TryStreamExt};
use std::path::PathBuf;
use tokio::io::AsyncReadExt;

#[derive(Debug)]
pub enum SeedSource {
    PostgresUrl(url::Url),
    LocalFile(PathBuf),
    S3Object { bucket: String, key: String },
}

pub fn parse_source(from: &str) -> Result<SeedSource> {
    if from.starts_with("postgresql://") || from.starts_with("postgres://") {
        let url =
            url::Url::parse(from).with_context(|| format!("Invalid PostgreSQL URL: {}", from))?;
        Ok(SeedSource::PostgresUrl(url))
    } else if let Some(without_scheme) = from.strip_prefix("s3://") {
        let (bucket, key) = without_scheme
            .split_once('/')
            .ok_or_else(|| anyhow!("Invalid S3 URL: expected s3://bucket/key"))?;
        Ok(SeedSource::S3Object {
            bucket: bucket.to_string(),
            key: key.to_string(),
        })
    } else {
        let path = PathBuf::from(from);
        if !path.exists() {
            anyhow::bail!("Seed file not found: {}. Provide a path to a .sql or .dump file, a postgresql:// URL, or an s3:// URL.", from);
        }
        Ok(SeedSource::LocalFile(path))
    }
}

pub async fn seed_branch(
    docker: &Docker,
    source: &SeedSource,
    container_name: &str,
    pg_user: &str,
    pg_db: &str,
    image: &str,
) -> Result<()> {
    match source {
        SeedSource::PostgresUrl(url) => {
            seed_from_postgres(docker, url, container_name, pg_user, pg_db, image).await
        }
        SeedSource::LocalFile(path) => {
            seed_from_file(docker, path, container_name, pg_user, pg_db).await
        }
        SeedSource::S3Object { bucket, key } => {
            seed_from_s3(docker, bucket, key, container_name, pg_user, pg_db, image).await
        }
    }
}

/// Detect dump format from file extension.
/// Returns true if this is a plain SQL file (use psql), false for custom/tar format (use pg_restore).
fn is_plain_sql(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("sql"))
        .unwrap_or(false)
}

/// Execute a command inside a container. Returns (exit_code, stderr_text).
pub(super) async fn docker_exec(
    docker: &Docker,
    container_name: &str,
    cmd: &[&str],
) -> Result<(i64, String)> {
    let config = ExecConfig {
        cmd: Some(cmd.iter().map(|s| s.to_string()).collect()),
        // Docker extracts uploaded archives as root. Keep dumps private (0600)
        // and run the import client as their owner, even when the database
        // container itself uses an unprivileged UID. `-U` selects the DB role.
        user: Some("0".to_string()),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        ..Default::default()
    };

    let exec = docker
        .create_exec(container_name, config)
        .await
        .context("failed to create exec instance")?;

    let start_opts = Some(StartExecOptions {
        detach: false,
        ..Default::default()
    });

    // Consume the output stream
    let mut stderr_buf = Vec::new();
    match docker.start_exec(&exec.id, start_opts).await? {
        bollard::exec::StartExecResults::Attached { mut output, .. } => {
            while let Some(msg) = output.try_next().await? {
                if let bollard::container::LogOutput::StdErr { message } = msg {
                    let remaining = (64 * 1024_usize).saturating_sub(stderr_buf.len());
                    stderr_buf.extend_from_slice(&message[..message.len().min(remaining)]);
                }
            }
        }
        bollard::exec::StartExecResults::Detached => {}
    }

    let inspect = docker.inspect_exec(&exec.id).await?;
    let exit_code = inspect.exit_code.unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&stderr_buf).to_string();

    Ok((exit_code, stderr))
}

/// Stream a tar header, the file, and padding; memory use stays bounded.
async fn upload_file_to_container(
    docker: &Docker,
    container_name: &str,
    filename: &str,
    path: &std::path::Path,
) -> Result<()> {
    let file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    anyhow::ensure!(len > 0, "seed file is empty");
    let mut header = tar::Header::new_gnu();
    header.set_path(filename)?;
    header.set_size(len);
    header.set_mode(0o600);
    header.set_cksum();
    let header = bytes::Bytes::copy_from_slice(header.as_bytes());
    let contents = futures_util::stream::try_unfold(file, |mut file| async move {
        let mut buf = vec![0; 64 * 1024];
        let len = file.read(&mut buf).await?;
        buf.truncate(len);
        Ok::<_, std::io::Error>(if len == 0 {
            None
        } else {
            Some((bytes::Bytes::from(buf), file))
        })
    });
    let padding = bytes::Bytes::from(vec![0; ((512 - len % 512) % 512 + 1024) as usize]);
    let stream = futures_util::stream::once(async { Ok::<_, std::io::Error>(header) })
        .chain(contents)
        .chain(futures_util::stream::once(async { Ok(padding) }));
    docker
        .upload_to_container(
            container_name,
            Some(UploadToContainerOptions {
                path: "/tmp".into(),
                ..Default::default()
            }),
            bollard::body_try_stream(stream),
        )
        .await
        .context("failed to stream seed into container")?;
    Ok(())
}

async fn seed_from_postgres(
    docker: &Docker,
    url: &url::Url,
    container_name: &str,
    pg_user: &str,
    pg_db: &str,
    image: &str,
) -> Result<()> {
    // Rewrite localhost/127.0.0.1 to host.docker.internal for Docker access
    let mut dump_url = url.clone();
    if let Some(host) = dump_url.host_str() {
        if host == "localhost" || host == "127.0.0.1" {
            dump_url
                .set_host(Some("host.docker.internal"))
                .map_err(|_| anyhow!("Failed to rewrite host to host.docker.internal"))?;
        }
    }

    let dump_url_str = dump_url.to_string();
    let dump_path = format!("/tmp/devflow-dump-{}.Fc", uuid::Uuid::new_v4());

    // Create an ephemeral container to run pg_dump, writing to a file
    let dump_container_name = format!("devflow-dump-{}", uuid::Uuid::new_v4());
    let config = ContainerCreateBody {
        image: Some(image.to_string()),
        cmd: Some(vec![
            "pg_dump".to_string(),
            "-Fc".to_string(),
            dump_url_str,
            "-f".to_string(),
            dump_path.to_string(),
        ]),
        host_config: Some(HostConfig {
            extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };

    let options = CreateContainerOptions {
        name: Some(dump_container_name.clone()),
        ..Default::default()
    };

    docker
        .create_container(Some(options), config)
        .await
        .context("Failed to create pg_dump container")?;

    let result = async {
        docker
            .start_container(
                &dump_container_name,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await?;
        let results: Vec<_> = docker
            .wait_container(
                &dump_container_name,
                Some(WaitContainerOptions {
                    condition: "not-running".into(),
                }),
            )
            .try_collect()
            .await
            .context("pg_dump did not complete successfully")?;
        let code = results.first().map(|r| r.status_code).unwrap_or(-1);
        anyhow::ensure!(code == 0, "pg_dump failed (exit {code})");
        // Docker already supplies a tar stream: relay it without downloading or
        // extracting the complete dump into memory.
        let stream = docker
            .download_from_container(
                &dump_container_name,
                Some(bollard::query_parameters::DownloadFromContainerOptions {
                    path: dump_path.clone(),
                }),
            )
            .map_err(std::io::Error::other);
        docker
            .upload_to_container(
                container_name,
                Some(UploadToContainerOptions {
                    path: "/tmp".into(),
                    ..Default::default()
                }),
                bollard::body_try_stream(stream),
            )
            .await?;
        let (code, stderr) = docker_exec(
            docker,
            container_name,
            &[
                "pg_restore",
                "-U",
                pg_user,
                "-d",
                pg_db,
                "--no-owner",
                "--single-transaction",
                &dump_path,
            ],
        )
        .await?;
        anyhow::ensure!(
            code == 0,
            "pg_restore failed (exit {code}): {}",
            stderr.trim()
        );
        Ok(())
    }
    .await;
    let _ = docker_exec(docker, container_name, &["rm", "-f", &dump_path]).await;
    let cleanup = docker
        .remove_container(
            &dump_container_name,
            Some(bollard::query_parameters::RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
    if let Err(error) = cleanup {
        log::warn!("failed to clean up seed container '{dump_container_name}': {error}");
    }
    result
}

async fn seed_from_file(
    docker: &Docker,
    path: &std::path::Path,
    container_name: &str,
    pg_user: &str,
    pg_db: &str,
) -> Result<()> {
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    if !abs_path.exists() {
        anyhow::bail!(
            "Seed file not found: {}. Check the path and try again.",
            abs_path.display()
        );
    }

    let filename = format!("devflow-seed-{}", uuid::Uuid::new_v4());
    let container_path = format!("/tmp/{filename}");
    upload_file_to_container(docker, container_name, &filename, &abs_path).await?;

    // Restore
    let restore = async {
        if is_plain_sql(&abs_path) {
            docker_exec(
                docker,
                container_name,
                &[
                    "psql",
                    "-X",
                    "-v",
                    "ON_ERROR_STOP=1",
                    "--single-transaction",
                    "-U",
                    pg_user,
                    "-d",
                    pg_db,
                    "-f",
                    &container_path,
                ],
            )
            .await
            .context("Failed to run psql")
        } else {
            docker_exec(
                docker,
                container_name,
                &[
                    "pg_restore",
                    "-U",
                    pg_user,
                    "-d",
                    pg_db,
                    "--no-owner",
                    "--single-transaction",
                    &container_path,
                ],
            )
            .await
            .context("Failed to run pg_restore")
        }
    }
    .await;

    // Clean up temp file in container
    let _ = docker_exec(docker, container_name, &["rm", "-f", &container_path]).await;

    let (exit_code, stderr) = restore?;

    if exit_code != 0 {
        anyhow::bail!("Restore failed (exit {}): {}", exit_code, stderr.trim());
    }

    Ok(())
}

async fn seed_from_s3(
    docker: &Docker,
    bucket: &str,
    key: &str,
    container_name: &str,
    pg_user: &str,
    pg_db: &str,
    _image: &str,
) -> Result<()> {
    let region = std::env::var("AWS_DEFAULT_REGION")
        .or_else(|_| std::env::var("AWS_REGION"))
        .unwrap_or_else(|_| "us-east-1".to_string());

    let s3_bucket = s3::Bucket::new(
        bucket,
        s3::Region::Custom {
            region: region.clone(),
            endpoint: format!("https://s3.{}.amazonaws.com", region),
        },
        s3::creds::Credentials::from_env()?,
    )?;

    let temp_dir = tempfile::tempdir().context("Failed to create temp directory")?;

    // Derive filename from key
    let filename = if key.to_ascii_lowercase().ends_with(".sql") {
        "seed.sql"
    } else {
        "seed.dump"
    };
    let temp_path = temp_dir.path().join(filename);

    log::info!("Downloading s3://{}/{}", bucket, key);
    let mut file = tokio::fs::File::create(&temp_path).await?;
    let status = s3_bucket
        .get_object_to_writer(key, &mut file)
        .await
        .context("failed to stream seed from S3")?;
    anyhow::ensure!(status == 200, "S3 download failed with status {status}");
    file.sync_all().await?;
    drop(file);

    // Delegate to file-based seeding
    seed_from_file(docker, &temp_path, container_name, pg_user, pg_db).await
}
