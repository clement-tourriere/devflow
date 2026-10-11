//! The `devflow` command line: argument parsing, prompting and rendering.
//! Lifecycle policy lives in `devflow-core`.
mod cli;

fn main() -> std::process::ExitCode {
    cli::main()
}
