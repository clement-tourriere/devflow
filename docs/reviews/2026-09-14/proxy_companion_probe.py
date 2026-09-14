#!/usr/bin/env python3
"""Check the actual companion on disposable ports without host integration."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[3]
BINARY = ROOT / "target/debug/devflow-proxy"

def ports():
    sockets = [socket.socket() for _ in range(6)]
    try:
        for sock in sockets:
            sock.bind(("127.0.0.1", 0))
        return [sock.getsockname()[1] for sock in sockets]
    finally:
        for sock in sockets:
            sock.close()

with tempfile.TemporaryDirectory(prefix=".devflow-companion-", dir=ROOT) as directory:
    root = Path(directory)
    env = {k: v for k, v in os.environ.items() if not k.startswith("DEVFLOW_")}
    env.update(DEVFLOW_PROXY_DIR=str(root / "proxy"), DEVFLOW_CONFIG_DIR=str(root / "config"), DEVFLOW_PROCESS_STATE_DIR=str(root / "processes"))
    chosen = ports()
    def command(offset):
        return [str(BINARY), "--non-interactive", "proxy", "start", "--https-port", str(chosen[offset]), "--http-port", str(chosen[offset+1]), "--api-port", str(chosen[offset+2]), "--domain-suffix", "localhost", "--no-auto-network", "--no-mdns"]
    with (root / "log").open("w+") as log:
        first = subprocess.Popen(command(0), env=env, cwd=root, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 15
            status = None
            while time.monotonic() < deadline:
                if first.poll() is not None:
                    log.seek(0)
                    raise RuntimeError(log.read())
                try:
                    with urllib.request.urlopen(f"http://127.0.0.1:{chosen[2]}/api/status", timeout=0.3) as response:
                        status = json.load(response)
                    if status.get("running"):
                        break
                except OSError:
                    time.sleep(0.1)
            assert status and status["running"], "companion never became ready"
            with urllib.request.urlopen(f"http://127.0.0.1:{chosen[2]}/api/targets") as response:
                targets = json.load(response)
            assert isinstance(targets, list)
            second = subprocess.run(command(3), env=env, cwd=root, capture_output=True, text=True, timeout=10)
            assert second.returncode != 0
            assert "owns the runtime" in second.stderr, second.stderr
            result = {"ready":True,"api_targets_is_array":True,"second_owner_rejected":True,"host_integration":False}
        finally:
            first.send_signal(signal.SIGINT) if first.poll() is None else None
            try:
                first.wait(timeout=10)
            except subprocess.TimeoutExpired:
                first.kill()
                first.wait()
        assert first.returncode == 0, first.returncode
        result["clean_shutdown"] = True
    Path(__file__).with_name("proxy-companion-results.json").write_text(json.dumps(result, indent=2)+"\n")
    print(json.dumps(result))
