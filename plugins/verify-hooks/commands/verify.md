---
description: 开启校验闭环:改完代码跑测试并修复失败。参数:改动摘要(可选)
---

用户希望本次改动经过校验闭环。使用引擎的两个校验 hook(经
verify-hooks 插件启用):

用户指令:

$ARGUMENTS

规则:

1. 改动完成后主动运行项目测试(默认 `cargo test`;可用
   `[hooks.verification].test_command` 覆盖),不要只依赖 Stop hook
   兜底。
2. Stop 前的 verification hook 会再跑一次校验命令,**失败会否决本轮
   结束**并把失败输出回注 —— 如实修复后重新声明完成。
3. bash 输出出现测试失败特征时,test_runner hook 会注入提醒;按提醒
   定位失败用例,修复后重跑确认。
4. 校验命令长时间不通过时,如实汇报剩余失败,不要谎报完成。
