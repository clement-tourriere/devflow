//! Keep incidental helper commands from creeping back into native operations.
//! This is an architecture check, not a sandbox for arbitrary plugin code.
use std::{collections::BTreeMap, path::Path};

fn inspect(root: &Path, directory: &Path, found: &mut BTreeMap<String, usize>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            inspect(root, &path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let source = std::fs::read_to_string(&path).unwrap();
            // Production modules keep their unit tests at the end. Integration
            // tests and maintained examples are outside these source roots.
            let production = source.split("\n#[cfg(test)]").next().unwrap();
            let count = production.matches("Command::new(").count()
                + production.matches("CommandBuilder::new(").count();
            if count > 0 {
                found.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .replace('\\', "/"),
                    count,
                );
            }
        }
    }
}

#[test]
fn process_construction_stays_with_documented_execution_adapters() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = BTreeMap::new();
    for source in [
        "src",
        "src-tauri/src",
        "crates/devflow-core/src",
        "crates/devflow-proxy/src",
        "crates/devflow-terminal/src",
    ] {
        inspect(root, &root.join(source), &mut found);
    }
    let allowed = [
        // Explicit commands, hooks, application processes and sessions.
        ("crates/devflow-core/src/execution.rs", 3),
        ("crates/devflow-core/src/hooks/actions/shell.rs", 2),
        ("crates/devflow-core/src/hooks/executor.rs", 1),
        ("crates/devflow-core/src/processes/mod.rs", 2),
        ("crates/devflow-core/src/detach.rs", 1),
        ("crates/devflow-core/src/companion.rs", 1),
        ("crates/devflow-terminal/src/lib.rs", 1),
        // Explicitly configured executable provider plugins.
        ("crates/devflow-core/src/services/plugin.rs", 1),
    ]
    .into_iter()
    .map(|(path, count)| (path.to_string(), count))
    .collect();
    assert_eq!(
        found, allowed,
        "Unexpected process boundary: use a native API or document an intentional adapter change"
    );
}
