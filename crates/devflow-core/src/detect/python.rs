//! Python environment tools: uv, Poetry and pip.
use super::facts::{PythonProject, PythonTool};
use super::manifest::{Discovery, Project};

pub(super) fn detect(discovery: &Discovery) -> Vec<PythonProject> {
    discovery.projects.iter().filter_map(project).collect()
}

fn project(project: &Project) -> Option<PythonProject> {
    let requirements_files: Vec<String> = project
        .py
        .files
        .iter()
        .filter(|f| f.ends_with(".txt"))
        .cloned()
        .collect();

    // A lockfile is the strongest signal, then the tool's own table, then plain requirements.
    let (tool, evidence) = if project.has_file("uv.lock") {
        (PythonTool::Uv, vec![project.file("uv.lock")])
    } else if project.has_file("poetry.lock") {
        (PythonTool::Poetry, vec![project.file("poetry.lock")])
    } else if project.has_file("uv.toml") {
        (PythonTool::Uv, vec![project.file("uv.toml")])
    } else if project.py.has_marker("tool.uv") {
        (PythonTool::Uv, vec![project.file("pyproject.toml:tool.uv")])
    } else if project.py.has_marker("tool.poetry") {
        (
            PythonTool::Poetry,
            vec![project.file("pyproject.toml:tool.poetry")],
        )
    } else if !requirements_files.is_empty() {
        (PythonTool::Pip, requirements_files.clone())
    } else {
        return None;
    };
    let lockfile = match tool {
        PythonTool::Uv => "uv.lock",
        PythonTool::Poetry => "poetry.lock",
        PythonTool::Pip => "",
    };
    Some(PythonProject {
        path: project.path.clone(),
        tool,
        requirements_files,
        lockfile: project.has_file(lockfile).then(|| project.file(lockfile)),
        evidence,
    })
}
