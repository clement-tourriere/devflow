#!/usr/bin/python3
"""Disposable CLI probes for the second architecture review; no real cloud calls."""
import argparse
import contextlib
import hashlib
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading

ROOT = Path(__file__).resolve().parents[3]
BIN = ROOT / "target/debug/devflow"
RESULTS = {}


@contextlib.contextmanager
def fixture(extra=None, branch="main"):
    with tempfile.TemporaryDirectory(prefix=".devflow-architecture-", dir=ROOT) as tmp:
        base = Path(tmp)
        repo = base / "repo"
        repo.mkdir()
        env = {k: v for k, v in os.environ.items() if not k.startswith("DEVFLOW_")}
        env.update(DEVFLOW_CONFIG_DIR=str(base / "config"),
                   DEVFLOW_PROCESS_STATE_DIR=str(base / "processes"),
                   GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
        def git(*args):
            subprocess.run(["git", *args], cwd=repo, env=env, check=True, capture_output=True)
        git("init", "-b", branch)
        git("config", "user.name", "Architecture probe")
        git("config", "user.email", "probe@example.invalid")
        config = {"name": "architecture-probe", "git": {"main_workspace": branch},
                  "worktree": {"path_template": "../worktrees/{workspace}",
                               "copy_files": [], "copy_ai_configs": False}}
        config.update(extra or {})
        (repo / ".devflow.yml").write_text(json.dumps(config))
        capture = repo / "capture-argv"
        capture.write_text('#!/usr/bin/python3\nimport json,sys\nfrom pathlib import Path\nPath("execution-ran").write_text("yes")\nprint(json.dumps(sys.argv[1:]))\n')
        capture.chmod(0o755)
        git("add", ".")
        git("-c", "commit.gpgsign=false", "commit", "-m", "probe fixture")
        def cli(*args, json_mode=True):
            flags = ["--non-interactive"] + (["--json"] if json_mode else [])
            out = subprocess.run([str(BIN), *flags, *args], cwd=repo, env=env,
                                 capture_output=True, text=True, timeout=30)
            result = {"exit_code": out.returncode, "stderr": out.stderr}
            if json_mode:
                result["document"] = json.loads(out.stdout)
            else:
                result["stdout"] = out.stdout
            return result
        yield base, repo, cli


with fixture() as (_, _, cli):
    RESULTS["build_features"] = cli("capabilities")["document"].get("build_features")
    RESULTS["argument_boundaries"] = cli("switch", "-c", "argv", "--no-services",
        "--no-processes", "--no-verify", "-x", "./capture-argv", "--", "hello world")

with fixture({"services": [{"name": "cache", "service_type": "generic",
                             "generic": {"image": "redis:7-alpine"}}]}) as (_, _, cli):
    RESULTS["reset_absent_generic_workspace"] = cli("service", "reset", "never-created")

with fixture({"hooks": {"pre-switch": {"approval-needed": "printf UNAPPROVED"}}}) as (base, _, cli):
    json_result = cli("switch", "-c", "json-task", "--no-services", "--no-processes",
                      "-x", "./capture-argv")
    human_result = cli("switch", "-c", "human-task", "--no-services", "--no-processes",
                       "-x", "./capture-argv", json_mode=False)
    inventory = cli("list")["document"]
    paths = {w["name"]: w.get("worktree_path") for w in inventory["workspaces"]}
    RESULTS["readiness_depends_on_output_mode"] = {
        "json": json_result, "human": human_result,
        "json_executed": bool(paths.get("json-task")) and (Path(paths["json-task"]) / "execution-ran").exists(),
        "human_executed": bool(paths.get("human-task")) and (Path(paths["human-task"]) / "execution-ran").exists(),
    }


class CloudStub(http.server.BaseHTTPRequestHandler):
    requests = []
    def log_message(self, *_args):
        pass
    def reply(self, value):
        body = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        self.requests.append({"method": "GET", "path": self.path})
        self.reply({"branches": [
            {"id": "br-default", "name": "trunk", "created_at": "2026-09-01T00:00:00Z"},
            {"id": "br-scratch", "name": "scratch", "created_at": "2026-09-02T00:00:00Z"},
        ]})
    def do_DELETE(self):
        self.requests.append({"method": "DELETE", "path": self.path})
        self.reply({})


if RESULTS["build_features"].get("neon"):
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), CloudStub)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with fixture({"services": [{"name": "db", "type": "neon", "neon": {
            "api_key": "fixture-only", "project_id": "fake",
            "base_url": f"http://127.0.0.1:{server.server_port}"}}]}, branch="trunk") as (_, _, cli):
            RESULTS["cleanup_configured_default"] = cli("service", "cleanup", "--max-count", "0")
            RESULTS["cleanup_configured_default"]["requests"] = CloudStub.requests
    finally:
        server.shutdown()
        server.server_close()

RESULTS["binary_sha256"] = hashlib.sha256(BIN.read_bytes()).hexdigest()
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--output", type=Path, default=Path(__file__).with_name("architecture-probe-results.json"))
output = parser.parse_args().output
output.write_text(json.dumps(RESULTS, indent=2) + "\n")
print(json.dumps(RESULTS, indent=2))
