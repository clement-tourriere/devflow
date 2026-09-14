//! One stdout document for JSON commands, including early and partial failures.
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

static JSON: AtomicBool = AtomicBool::new(false);
static DOCUMENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

pub fn configure(json: bool) {
    JSON.store(json, Ordering::Relaxed);
}

pub fn println(args: std::fmt::Arguments<'_>) {
    if !JSON.load(Ordering::Relaxed) {
        std::println!("{args}");
        return;
    }
    let text = args.to_string();
    match serde_json::from_str(&text) {
        Ok(value) => DOCUMENTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(value),
        Err(_) if !text.trim().is_empty() => std::eprintln!("{text}"),
        Err(_) => {}
    }
}

pub fn finish(result: &anyhow::Result<()>) {
    if !JSON.load(Ordering::Relaxed) {
        return;
    }
    let docs = std::mem::take(&mut *DOCUMENTS.lock().unwrap_or_else(|e| e.into_inner()));
    let mut document = match docs.len() {
        0 => json!({"schema_version": 1, "status": "ok"}),
        1 => docs.into_iter().next().unwrap(),
        _ => json!({"schema_version": 1, "results": docs}),
    };
    if let Err(error) = result {
        if !document.is_object() {
            document = json!({"data": document});
        }
        document["status"] = json!("error");
        document["error"] = json!({"message": format!("{error:#}")});
        if let Some(error) =
            error.downcast_ref::<devflow_core::workspace::operation::OperationError>()
        {
            document["operation"] = json!(error.operation);
            document["worktree_path"] = json!(error.operation.worktree_path);
            document["agent_workdir"] = json!(error.operation.worktree_path);
        }
    }
    if let Some(object) = document.as_object_mut() {
        object.entry("schema_version").or_insert(json!(1));
    }
    std::println!(
        "{}",
        serde_json::to_string_pretty(&document).expect("JSON serialization cannot fail")
    );
}
