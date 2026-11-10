# zhuye-py

烛夜 Agent 的 Python 绑定(Pyo3 + maturin)。

## 构建

```bash
cd crates/runtime/zhuye-py
maturin develop
```

## 示例

```bash
python examples/basic_builder.py
```

完整离线验证会构建并安装 wheel,再执行真实 Python 多轮调用:

```bash
./tests/run_all.sh native-python
```
