"""能力覆盖层 · v1.11 F6:组合透明性(`zhuye doctor --dump-config`)。

覆盖(对照 gap-report F6「不做内核化,做透明性」建议):
- JSON 快照结构:schema 标记 / active(provider / model_spec)/ providers
  凭据摘要 / routing / hooks / mcp / plugins / sandbox
- 脱敏不变量:任何 api_key 值不出现在输出中,只出 has_api_key 布尔
- TOML 配置生效路径:写 anthropic 段后 dump 应反映 provider/model_spec
"""

from __future__ import annotations

import json

import pytest

from wire import require_binary, run_cli

pytestmark = pytest.mark.usefixtures("sandbox")

SECRET_VALUE = "sk-ant-super-secret-value-0123456789"


def _dump(sandbox) -> dict:
    # env 隔离:runner 门禁恒设 ZHUYE_MODEL=mock(mock 优先级高于 TOML),
    # 不置空则 provider 解析恒为 mock,本组断言假失败。
    proc = run_cli(sandbox, "doctor", "--dump-config", env={"ZHUYE_MODEL": ""}, timeout=60)
    assert proc.returncode == 0, proc.stderr[:400]
    return json.loads(proc.stdout)


def test_dump_config_结构与脱敏(sandbox) -> None:
    """空配置 → schema 标记 + active 全 null + 输出不含任何密钥值。"""
    require_binary()
    dump = _dump(sandbox)
    assert dump["schema"] == "zhuye-dump-config/1", dump.get("schema")
    assert dump["active"]["provider"] is None
    assert dump["active"]["model_spec"] is None
    # 脱敏:占位密钥值绝不能出现在 JSON 快照里。
    assert SECRET_VALUE not in json.dumps(dump)


def test_dump_config_反映生效配置(sandbox) -> None:
    """写 anthropic 段(含 credential 池)→ active/provider/model_spec/
    凭据 label 与 has_api_key 布尔逐字段反映。"""
    require_binary()
    _config_toml(sandbox)
    dump = _dump(sandbox)
    assert dump["active"]["provider"] == "anthropic", dump["active"]
    assert dump["active"]["model_spec"] == "anthropic/claude-x", dump["active"]
    anthropic = dump["providers"]["anthropic"]
    # 段级 api_key 是空串(免 key 端点形态,key 在凭据池里)→ has_api_key
    # 反映段级字段,应为 False;真实 key 在池条目上。
    assert anthropic["has_api_key"] is False
    assert anthropic["model"] == "claude-x"
    labels = [c["label"] for c in anthropic["credentials"]]
    assert labels == ["minimax", "backup"], labels
    by_label = {c["label"]: c for c in anthropic["credentials"]}
    assert by_label["minimax"]["has_api_key"] is True
    # 脱敏:密钥值不出现在任何字段。
    assert SECRET_VALUE not in json.dumps(dump)


def _config_toml(sandbox) -> None:
    # conftest 已预写 [hooks] 段 —— 直接追加,不能重复段头(TOML 重复段
    # 解析失败会让 load_default 静默回退默认配置,active 全 None)。
    toml = (
        '\n[active]\nprovider = "anthropic"\ncredential = "minimax"\n'
        + '\n[anthropic]\napi_key = ""\nmodel = "claude-x"\n'
        + '[[anthropic.credentials]]\nlabel = "minimax"\napi_key = "'
        + SECRET_VALUE
        + '"\n'
        + '[[anthropic.credentials]]\nlabel = "backup"\napi_key = "sk-b"\n'
    )
    cfg = sandbox.home / ".zhuye" / "config.toml"
    cfg.write_text(cfg.read_text() + toml)
