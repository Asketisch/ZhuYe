#!/usr/bin/env python3
"""采集隔离 mock 会话的启动、首事件、首文本、耗时与峰值内存。"""
import json
import os
from pathlib import Path
import platform
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parent.parent
BIN = Path(os.environ.get("ZHUYE_BIN", ROOT / "target/release/zhuye"))
RUNS = int(os.environ.get("BENCH_RUNS", "5"))
if RUNS < 1 or not BIN.is_file():
    sys.exit("性能测试需要可执行产物及大于零的 BENCH_RUNS")
results = {k: [] for k in ("cold_start_s", "first_event_s", "first_text_s", "single_turn_s", "peak_rss_kb")}
with tempfile.TemporaryDirectory() as tmp:
    home = Path(tmp)
    (home / ".zhuye").mkdir()
    (home / ".zhuye/config.toml").write_text("[hooks]\nenabled = []\n")
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("ZHUYE_", "OPENAI_", "ANTHROPIC_", "GEMINI_", "OLLAMA_", "BRAVE_"))}
    env.update(HOME=str(home), ZHUYE_HOME=str(home / ".zhuye"), ZHUYE_MODEL="mock", OPENAI_API_KEY="sk-test")
    env.pop("ZHUYE_MOCK_SCRIPT", None)
    for _ in range(RUNS):
        start = time.monotonic()
        subprocess.run([str(BIN), "--version"], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=60, env=env)
        results["cold_start_s"].append(time.monotonic() - start)
        time_file = home / "time.txt"
        time_args = ["/usr/bin/time", "-l"] if sys.platform == "darwin" else ["/usr/bin/time", "-v"]
        start = time.monotonic()
        with time_file.open("w") as err:
            proc = subprocess.Popen([*time_args, str(BIN), "exec", "noop"], stdout=subprocess.PIPE, stderr=err, text=True, env=env, cwd=home, start_new_session=True)
            timer = threading.Timer(60, lambda: os.killpg(proc.pid, signal.SIGKILL))
            timer.start()
            first = delta = None
            assert proc.stdout is not None
            try:
                for line in proc.stdout:
                    event = json.loads(line)
                    msg = event.get("msg", {})
                    if first is None:
                        first = time.monotonic() - start
                    if delta is None and msg.get("type") == "agent_message_delta" and msg.get("delta"):
                        delta = time.monotonic() - start
                rc = proc.wait(timeout=60)
            finally:
                timer.cancel()
                if proc.poll() is None:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait()
            if rc != 0 or first is None or delta is None:
                sys.exit("性能测试执行失败或缺少首事件/文本:" + time_file.read_text())
        results["first_event_s"].append(first)
        results["first_text_s"].append(delta)
        results["single_turn_s"].append(time.monotonic() - start)
        lines = time_file.read_text().splitlines()
        if sys.platform == "darwin":
            rss = next((int(line.split()[0]) // 1024 for line in lines if "maximum resident set size" in line), None)
        else:
            rss = next((int(line.rsplit(":", 1)[1]) for line in lines if "Maximum resident set size" in line), None)
        if not rss:
            sys.exit("性能测试未取得峰值内存")
        results["peak_rss_kb"].append(rss)
out = Path(os.environ.get("ZHUYE_BENCH_DIR", ROOT / "target/bench"))
out.mkdir(parents=True, exist_ok=True)
report = {"platform": platform.platform(), "runs": RUNS, "samples": results}
(out / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
summary = "\n".join(f"{key}: min={min(vals):.4f} mean={statistics.mean(vals):.4f} max={max(vals):.4f} n={len(vals)}" for key, vals in results.items())
(out / "summary.txt").write_text(summary + "\n")
print(summary)
