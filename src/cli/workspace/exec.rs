use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::execution::CommandSpec;
use std::path::PathBuf;

pub(super) use devflow_core::execution::ExecutionOutput;

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_in_workspace(
    config: &Config,
    config_path: &Option<PathBuf>,
    workspace: &str,
    command: &str,
    args: &[String],
    detach: bool,
    json_output: bool,
) -> Result<ExecutionOutput> {
    devflow_core::execution::execute_in_workspace(
        config,
        &crate::cli::operation_project_dir(config_path),
        workspace,
        &CommandSpec::legacy(command, args),
        detach,
        json_output,
    )
    .await
}
