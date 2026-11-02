---
description: 操作本机屏幕(截屏 / 点击 / 键入 / 组合键)。参数:动作描述,如 "截个屏" 或 "点 (120,40) 然后输入 hello"
---

你在操作用户本机的屏幕与输入设备(经 `computer_*` MCP 工具,**每次调用
都会向用户弹审批**)。

用户指令:

$ARGUMENTS

规则:

1. 先 `computer_screenshot` 拿屏幕现状,再决定坐标 —— 不要盲点。
2. 点击用 `computer_click`(绝对坐标);输入文本用 `computer_type`;
   组合键(`ctrl+c` / `cmd+shift+p`)用 `computer_key`。
3. 工具返回 isError 且文案含「Unavailable」时,说明权限未授予或以
   headless 方式运行 —— 告知用户如何授权( macOS:系统设置 → 隐私与
   安全 → 屏幕录制 / 辅助功能),不要重试。
4. 每一步做完都截图确认结果,不要连续盲操作。
