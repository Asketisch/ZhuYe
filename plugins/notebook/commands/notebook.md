---
description: 编辑 Jupyter notebook:对 .ipynb 做 cell 级替换/新增/删除。参数:文件与改动意图
---

用户要编辑 Jupyter notebook。先 `read` 目标 .ipynb(cell 结构会以
JSON 形式可见),再用 `notebook_edit` 做 cell 级修改,不要整文件重写。

用户指令:

$ARGUMENTS

规则:

1. 编辑前必须先成功 `read` 该文件(read_before_edit 会拦截未读就写)。
2. `notebook_edit` 参数:`path` / `cell_id`(或 `cell_index`)/
   `new_source` / `mode`(replace | insert | delete);insert 需要
   位置语义,delete 只需定位。
3. 保持 notebook JSON 结构(nbformat 版本、metadata、其余 cell)原样;
   只动目标 cell。
4. 改完建议提示用户在 Jupyter 中重载文件验证输出。
