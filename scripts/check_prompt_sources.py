#!/usr/bin/env python3
"""对比提示词来源的固定版本和上游分支，输出供维护者审核的报告。"""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from pathlib import Path
from urllib.parse import quote

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "crates/abilities/zhuye-prompt/prompts/sources.json"


def gh_api(endpoint: str, *, raw: bool = False) -> bytes:
    command = ["gh", "api"]
    if raw:
        command.extend(["-H", "Accept: application/vnd.github.raw+json"])
    command.append(endpoint)
    return subprocess.check_output(command)


def tree(repository: str, revision: str) -> dict[str, str]:
    data = json.loads(gh_api(f"repos/{repository}/git/trees/{revision}?recursive=1"))
    if data.get("truncated"):
        raise RuntimeError(f"上游文件树被截断: {repository}@{revision}")
    return {item["path"]: item["sha"] for item in data["tree"] if item["type"] == "blob"}


def main() -> int:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    collection = manifest["collection"]
    repo, pinned, branch = (collection[k] for k in ("repository", "revision", "branch"))
    old, new = tree(repo, pinned), tree(repo, branch)
    prefixes = {entry["path"].split("/", 1)[0] + "/" for entry in manifest["profiles"]}
    print(f"# 提示词来源检查\n\n来源 {repo}，基准 {pinned}，分支 {branch}。\n")
    for entry in manifest["profiles"]:
        path = entry["path"]
        content = gh_api(f"repos/{repo}/contents/{quote(path)}?ref={pinned}", raw=True)
        digest = hashlib.sha256(content).hexdigest()
        if digest != entry["sha256"]:
            raise RuntimeError(f"固定来源摘要不符: {path}")
    changed = sorted(path for path in old.keys() | new.keys()
                     if path.startswith(tuple(prefixes)) and old.get(path) != new.get(path))
    if changed:
        for path in changed:
            status = "新增" if path not in old else "删除或移动" if path not in new else "变更"
            print(f"- {status} `{path}`")
    else:
        print("跟踪目录没有变化。")
    for entry in manifest["additional_sources"]:
        repository, revision, source_branch, path = (
            entry[k] for k in ("repository", "revision", "branch", "path")
        )
        content = gh_api(f"repos/{repository}/contents/{quote(path)}?ref={revision}", raw=True)
        if hashlib.sha256(content).hexdigest() != entry["sha256"]:
            raise RuntimeError(f"固定来源摘要不符: {repository}/{path}")
        before, after = tree(repository, revision), tree(repository, source_branch)
        status = "已变化或移动" if before.get(path) != after.get(path) else "无变化"
        print(f"- {repository} `{path}` {status}")
    check_tools(manifest)
    return 0


def check_tools(manifest: dict) -> None:
    """工具×模型两级目录(tools 段)的来源校验。

    实体变体(file + source)按独立的 `tools_collection` 基准核对 sha256,
    并报告基准 → 分支的漂移;回引叶项(profile_ref)不在此核对 —— 其
    来源已由 profiles / additional_sources 覆盖。
    """
    collection = manifest.get("tools_collection")
    tools = manifest.get("tools")
    if not collection or not tools:
        return
    repo, pinned, branch = (collection[k] for k in ("repository", "revision", "branch"))
    old, new = tree(repo, pinned), tree(repo, branch)
    print(f"\n## 工具变体目录\n\n基准 {pinned}，分支 {branch}。\n")
    sources = [
        (tool["id"], variant)
        for tool in tools
        for variant in tool.get("variants", [])
        if "source" in variant
    ]
    for tool_id, variant in sources:
        path = variant["source"]["path"]
        content = gh_api(f"repos/{repo}/contents/{quote(path)}?ref={pinned}", raw=True)
        digest = hashlib.sha256(content).hexdigest()
        if digest != variant["source"]["sha256"]:
            raise RuntimeError(f"固定来源摘要不符: {tool_id}/{variant['id']} → {path}")
    prefixes = {variant["source"]["path"].rsplit("/", 1)[0] + "/"
                for _, variant in sources}
    changed = sorted(path for path in old.keys() | new.keys()
                     if path.startswith(tuple(prefixes)) and old.get(path) != new.get(path))
    if changed:
        for path in changed:
            status = "新增" if path not in old else "删除或移动" if path not in new else "变更"
            print(f"- {status} `{path}`")
    else:
        print("工具变体来源目录没有变化。")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, subprocess.CalledProcessError, OSError) as error:
        print(f"提示词来源检查失败: {error}", file=sys.stderr)
        raise SystemExit(1) from error
