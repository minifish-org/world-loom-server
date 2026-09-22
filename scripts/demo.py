#!/usr/bin/env python3
"""Exercise MCP build, restart and undo using an isolated temporary world."""

import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    root = Path(__file__).resolve().parents[1]
    subprocess.run(["cargo", "build", "--locked"], cwd=root, check=True)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=root
    ))
    binary = Path(metadata["target_directory"]) / "debug/world-loom-server"
    with tempfile.TemporaryDirectory(prefix="world-loom-demo-") as directory:
        data = Path(directory)
        # Ignore deployment-specific environment variables for this isolated demo.
        env = {k: v for k, v in os.environ.items() if not k.startswith("WORLD_LOOM_")}
        bridge_port = free_port()
        game_port = free_port()
        while game_port == bridge_port:
            game_port = free_port()
        env.update({
            "WORLD_LOOM_GAME_ADDR": f"127.0.0.1:{game_port}",
            "WORLD_LOOM_BRIDGE_ADDR": f"127.0.0.1:{bridge_port}",
            "WORLD_LOOM_DB_PATH": str(data / "world.sqlite3"),
            "WORLD_LOOM_REGION_DIR": str(data / "anvil/region"),
            "WORLD_LOOM_ALLOWED_ORIGINS": "http://localhost:3000",
        })
        endpoint = f"http://127.0.0.1:{bridge_port}/mcp"
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def rpc(method, params=None):
            request = urllib.request.Request(endpoint, data=json.dumps({
                "jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}
            }).encode(), headers={
                "Content-Type": "application/json", "Origin": "http://localhost:3000"
            })
            with opener.open(request, timeout=10) as response:
                body = json.load(response)
            if "error" in body:
                raise RuntimeError(body["error"])
            return body["result"]

        def tool(name, arguments=None):
            result = rpc("tools/call", {"name": name, "arguments": arguments or {}})
            if result.get("isError"):
                raise RuntimeError(result)
            return result["structuredContent"]

        def stop(process):
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:
                    process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    raise RuntimeError("Server did not shut down cleanly")
            if process.returncode != 0:
                raise RuntimeError(f"Server exited with {process.returncode}")

        with (data / "server.log").open("w+") as log:
            process = None
            try:
                def start():
                    child = subprocess.Popen([str(binary)], cwd=data, env=env,
                                             stdout=log, stderr=subprocess.STDOUT)
                    try:
                        deadline = time.monotonic() + 30
                        while time.monotonic() < deadline:
                            if child.poll() is not None:
                                raise RuntimeError("Server exited during startup")
                            try:
                                rpc("initialize")
                                return child
                            except (urllib.error.URLError, TimeoutError):
                                time.sleep(0.2)
                        raise RuntimeError("Server startup timed out")
                    except BaseException:
                        if child.poll() is None:
                            child.terminate()
                            try:
                                child.wait(timeout=5)
                            except subprocess.TimeoutExpired:
                                child.kill()
                                child.wait()
                        raise

                process = start()
                names = {item["name"] for item in rpc("tools/list")["tools"]}
                assert {"apply_build_plan", "undo_build", "get_block"} <= names
                position = {"x": 8, "y": 65, "z": 8}
                original = tool("get_block", position)["block"]
                plan = {"plan": {
                    "schema_version": 1, "idempotency_key": "public-demo",
                    "anchor": position, "replace_mode": "air_only",
                    "operations": [{"op": "set", "at": [0, 0, 0], "block": "stone"}],
                }}
                build = tool("apply_build_plan", plan)
                assert tool("get_block", position)["block"] == "stone"
                assert tool("apply_build_plan", plan)["build_id"] == build["build_id"]
                print("PASS: MCP build and idempotent retry")
                stop(process)
                process = start()
                assert tool("get_block", position)["block"] == "stone"
                print("PASS: world survives server restart")
                tool("undo_build", {"build_id": build["build_id"]})
                assert tool("get_block", position)["block"] == original
                print("PASS: persisted build can be undone after restart")
            except BaseException:
                log.flush()
                log.seek(0)
                print(log.read())
                raise
            finally:
                if process is not None:
                    stop(process)
    print("Demo complete; temporary world removed. No private services were used.")


if __name__ == "__main__":
    main()
