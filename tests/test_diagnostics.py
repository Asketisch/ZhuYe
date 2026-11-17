"""验证功能层失败证据能保存且不会包含已知凭据。"""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class DiagnosticsTest(unittest.TestCase):
    def test_失败时保存并遮盖凭据(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            suite = root / "suite"
            suite.mkdir()
            for name in ("conftest.py", "wire.py"):
                shutil.copy2(ROOT / "tests/functional" / name, suite / name)
            (suite / "test_failure.py").write_text(
                'def test_failure(sandbox):\n'
                '    (sandbox.cwd / "serve-one.jsonl").write_text("HOST_SECRET_VALUE")\n'
                '    (sandbox.cwd / "serve-two.stderr").write_text("api_key = \\\"local-secret\\\"")\n'
                '    assert False\n'
            )
            output = root / "evidence"
            env = dict(os.environ, ZHUYE_TEST_ARTIFACTS=str(output), OPENAI_API_KEY="HOST_SECRET_VALUE")
            result = subprocess.run([sys.executable, "-m", "pytest", str(suite), "-q", "-p", "no:cacheprovider"],
                                    env=env, capture_output=True, text=True, timeout=30)
            self.assertNotEqual(result.returncode, 0)
            files = list(output.rglob("*"))
            content = "\n".join(path.read_text() for path in files if path.is_file())
            self.assertTrue(any(path.name == "identity.json" for path in files), result.stdout)
            self.assertNotIn("HOST_SECRET_VALUE", content)
            self.assertNotIn("local-secret", content)
            self.assertIn("已隐藏", content)
