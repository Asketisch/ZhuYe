# zhuye-py

<p align="center">  [中文](README.md) · **English**  </p>

Python bindings for the ZhuYe Agent (PyO3 + maturin).

## Build

```bash
cd crates/runtime/zhuye-py
maturin develop
```

## Example

```bash
python examples/basic_builder.py
```

Full offline verification builds and installs the wheel, then performs
real multi-turn Python calls:

```bash
./tests/run_all.sh native-python
```
