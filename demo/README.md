# 60 秒演示 / 60-second demo

> [English](README.en.md)

`demo.sh` 演示：启动单二进制服务 → 写入 3 条带 payload 的向量 → 向量检索 →
带过滤的混合检索 → `kill -9` → 重启后 WAL 重放，数据仍可查。

```bash
cargo build --release -p houyi-server
./demo/demo.sh            # 带停顿，适合观看
DEMO_FAST=1 ./demo/demo.sh  # 无停顿，适合冒烟
```

重新生成 README 顶部的 GIF：

```bash
brew install vhs
vhs demo/demo.tape
```

GIF 使用固定的 `/tmp/houyidb-demo` 数据目录，避免录屏暴露个人路径；脚本退出时
会清理该目录。端口可用 `HOUYI_DEMO_URL` / `HOUYI_DEMO_PG` 覆盖。
