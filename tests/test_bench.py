"""性能脚本必须测量实际事件,缺事件和进程失败不能伪装成功。"""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class BenchTest(unittest.TestCase):
    def run_bench(self, source):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary = root / "zhuye"
            binary.write_text("#!/usr/bin/env python3\n" + source)
            binary.chmod(0o755)
            env = dict(os.environ, ZHUYE_BIN=str(binary), BENCH_RUNS="1", ZHUYE_BENCH_DIR=str(root / "report"))
            result = subprocess.run(["python3", str(ROOT / "scripts/bench.py")], env=env,
                                    text=True, capture_output=True, timeout=20)
            report = root / "report/summary.json"
            data = json.loads(report.read_text()) if report.exists() else None
            return result, data

    def test_有效事件采样(self):
        result, data = self.run_bench(
            'import json,sys,time\n'
            'if "--version" not in sys.argv:\n'
            ' print(json.dumps({"msg":{"type":"turn_started"}}),flush=True)\n'
            ' time.sleep(.05)\n'
            ' print(json.dumps({"msg":{"type":"agent_message_delta","delta":"a"}}),flush=True)\n'
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        samples = data["samples"]
        self.assertGreater(samples["first_text_s"][0], samples["first_event_s"][0] + .03)
        self.assertGreater(samples["peak_rss_kb"][0], 0)

    def test_缺少文本事件失败(self):
        result, data = self.run_bench(
            'import json,sys\n'
            'if "--version" not in sys.argv: print(json.dumps({"msg":{"type":"turn_started"}}))\n'
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIsNone(data)
