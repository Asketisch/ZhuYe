# ZhuYe 引擎 · 剩余工作清单

> 基线:`develop` @ 93fda39。todolist.md F01-F70 共 70 个功能单元的引擎侧核心能力已全部落地
> (308/312 勾选),本文件记录**诚实的延后项与验证欠账**,按优先级排列。

## 一、需要真机/真环境的验证(代码已落地,未在真实环境验证)

- [ ] **F24 Windows 沙箱真机验证**:windows_ffi(AppContainer / Job Object / ACL / ToolHelp)
      已通过双 target(`x86_64-pc-windows-msvc` / `aarch64-pc-windows-msvc`)编译零错误,
      真机集成测试以 `--ignored` 标注(93fda39)——需在真实 Windows 机器上运行验证。
- [ ] **F40 computer-use 真机手动验证**:a11y 树寻址 / 跨平台后端(macOS AX / Win UIA /
      Linux X11·Wayland)的单测为快照绑定 + 后验断言;真机 real feature 需手动验。

## 二、环境受限项

- [ ] **zhuye-py 本机编译**:本机缺 Python 3.9 framework,全量测试以 `--exclude zhuye-py`
      跑绿;需在标准 Python 环境复验绑定编译。
- [ ] **live / 实模型门禁**:真 LLM(F20 LLM judge 真模型判分)、真网络类任务未跑;
      按 AGENTS.md 口径需 API key 手动触发(`./tests/run_all.sh live`)。

## 三、全量选集重跑

- [ ] **nightly / release 选集**最后一批提交后未在本机完整重跑
      (fault / fuzz / soak / 性能基线 `scripts/bench.sh` / 打包签名发布链);
      开发期间各门类专项均已跑过。

## 四、长期约束(非欠账,持续遵守)

- Protocol v0 已冻结:后续只增不改既有变体。
- 下游 `zhuye-agent/` submodule 只读;改核心回本仓库,再由下游升级指针。
