# HouyiDB VectorDBBench 对标协议

> [English](VDBBENCH_PROTOCOL.en.md)

> 目标：用同一套参数、同一台机器、同一流程对比 HouyiDB 与外部基线，结果可
> 复现、可复核。本文只规定**怎么测**与**怎么记录**，不承诺任何具体数值；
> 本仓库不发布第三方系统的对比结果。
>
> EN: a reproducible protocol for comparing HouyiDB with external baselines
> under identical parameters and hardware. This repository publishes the
> protocol only, not third-party comparison results.

## 1. 工具与前提

- 基准工具：`VectorDBBench`（本地工作副本，下称 `VDB_ROOT`）。
- 对标系统：`Qdrant`，同一台机器运行。
- HouyiDB 的 VectorDBBench client 适配器不在本仓库内：请把
  `VDB_ROOT` 指向包含该适配器的工作副本，或按 `--transport http`
  自行接入。
- Python 3.10+，建议独立 venv：

```bash
cd "$VDB_ROOT"
python3 -m venv .venv
source .venv/bin/activate
python -m pip install -U pip
python -m pip install -e ".[test,qdrant]"
```

本仓库的对照脚本默认把 `VDB_ROOT` 解析为与 `houyidb` 同级的
`vectorDBBench-houyidb/`；目录不同时显式覆盖：

```bash
export VDB_ROOT=/path/to/VectorDBBench
```

## 2. 公平性原则

- **同一 case**：两侧使用相同 `case_type`。
- **同一查询参数**：相同 `k`、`m`、`ef_construction`、`ef_search`、
  `num_concurrency`、`concurrency_duration`。
- **同一硬件**：同机运行；不要让两侧共享 CPU 峰值窗口以外的负载。
- **同一流程**：都执行 `load -> optimize -> search_serial ->
  search_concurrent`。
- **冷启动一致**：HouyiDB 侧在空库、无旧 WAL 的状态下开始；Qdrant 侧
  同样使用空 collection。
- **失败可见**：OOM、超时、连接错误必须写入结果，不允许静默重试或丢弃。
- **不混比**：不同 case、不同 `k`、不同并发、不同 transport 的结果不能
  放在同一张对比表里。

## 3. 默认参数

对照脚本内置以下默认值，可用环境变量覆盖；**两侧必须同时使用同一组值**：

| 变量 | 默认 | 说明 |
|------|------|------|
| `VDB_CASE` | `Performance1536D50K` | VectorDBBench case |
| `VDB_K` | `10` | 返回条数 |
| `VDB_M` | `24` | HNSW `m` |
| `VDB_EFC` | `400` | `ef_construction` |
| `VDB_EFS` | `256` | 查询 `ef_search` |
| `VDB_NUM_CONCURRENCY` | `1` | 可逗号分隔，如 `1,8,16` |
| `VDB_CONCURRENCY_DURATION` | `30` | 每档并发持续秒数 |
| `VDB_LOAD_CONCURRENCY` | `0` | 加载并发；`0` = CPU 核数 |
| `VDB_HOUYI_TRANSPORT` | `pgwire` | `pgwire` 或 `http` |
| `HOUYI_PG_PORT` | `15432` | HouyiDB pgwire 端口 |
| `HOUYI_HTTP_PORT` | `17000` | HouyiDB HTTP 端口 |
| `QDRANT_URL` | `http://127.0.0.1:6333` | Qdrant HTTP 地址 |

`HOUYI_HNSW_*` 构图参数（如 `HOUYI_HNSW_MAX_BUILD_BATCH`、
`HOUYI_HNSW_PARALLEL_BATCH_BUILD`）属于服务端调优项，必须在启动
`houyi-server` **之前**设置，并随结果一起记录；两侧对照时应保持恒定。

## 4. 启动两侧服务

HouyiDB（服务端参数需与 bench 的 `m` / `ef_construction` 对齐）：

```bash
./target/release/houyi-server \
  --benchmark-http-addr 127.0.0.1:17000 \
  --pgwire-addr 127.0.0.1:15432 \
  --vector-dim 1536 \
  --index-type hnsw \
  --pgwire-hnsw-m 24 \
  --pgwire-hnsw-efc 400 \
  --wal-path /tmp/houyidb-bench/houyidb.wal \
  --snapshot-interval-secs 0
```

`--snapshot-interval-secs 0` 关闭周期图快照，避免后台快照影响写阶段；
如果需要验证快照路径，请单独做一组注明参数的运行。

Qdrant（原生二进制或容器均可）：

```bash
# 启动 Qdrant 后确认可达
curl -fsS http://127.0.0.1:6333/collections
```

本机访问请设置 `NO_PROXY=127.0.0.1,localhost`，避免系统代理劫持请求。

## 5. 一键运行

本仓库公开的适配器在 `benchmarks/vectordb_bench_houyidb/`，默认参数与本文一致：

```bash
# 1) 把适配器装进本地 VectorDBBench 工作副本
python3 benchmarks/vectordb_bench_houyidb/install.py --vdb-root "$VDB_ROOT"

# 2) 启动 houyi-server（空 WAL），然后运行 benchmark
VDB_ROOT="$VDB_ROOT" ./benchmarks/vectordb_bench_houyidb/run_bench.sh

# 3) 如需对比两份结果 JSON（可带 recall/ndcg 门禁）
python3 scripts/vdbbench_compare_results.py <reference.json> <houyidb.json> \
  --min-recall 0.95
```

脚本在跑分前会检查 HouyiDB `GET /v1/health`；外部基线请按你自己的客户端启动并
确认可达。

不可达时立即失败并打印目标地址，避免空跑。

## 6. 接口路径标注

每条结果都必须注明数据面接口，禁止把不同路径的数字混在一栏：

| transport | 数据面 | 说明 |
|-----------|--------|------|
| `benchmark-http` | HTTP `/v1/vectors/*` | JSON 编解码，控制面与数据面同端口 |
| `pgwire` | PostgreSQL 协议 | 批量写入走 Extended Query；查询按服务端实现走文本/二进制参数 |
| `pgwire-binary` | PostgreSQL 二进制参数 | 仅在适配器明确使用二进制 binding 时标注 |
| `adbc` | Arrow 批量传输 | 尚未提供正式适配器；有结果时必须单独列出 |

跨接口对比时要说明：是否存在额外 JSON 编解码、文本还是二进制参数、
是否使用批量写入/查询优化。仅基于 benchmark HTTP 的结论不得表述为
“HouyiDB 的最终协议性能”。

## 7. 结果记录规范

每次运行至少保存：

- VectorDBBench 原始 `result_*.json`（不裁剪）；
- 一份摘要表：case、recall@k、ndcg@k、p50/p95/p99、
  `qps`、`load_duration`、`insert_duration`、`optimize_duration`；
- 关键参数：`m` / `ef_construction` / `ef_search` / 并发 / 持续时间；
- 服务端与构建信息：HouyiDB 版本或 commit、Qdrant 版本、
  `transport`、`HNSW_*` 环境变量、CPU/内存/文件系统；
- 失败与异常：OOM、超时、重试次数、日志片段。

指标口径：

- `load_duration` = 插入 + 优化；单看 `insert_duration` 会被异步构图转移
  工作量，不能直接与旧版对比；
- `recall` / `ndcg` 以 VectorDBBench 输出的 ground truth 为准；
- streaming case 的阶段时间戳是“相对写入开始的检索启动时刻”，不是该阶段
  的检索耗时；写速度看 `insert_duration`。

## 8. HNSW 图层密度与构图路径

- `HOUYI_HNSW_LAYER0_MULT`：level-0 连接数乘数。默认 `2`（召回优先）；
  `1` 提供更高 QPS、召回略降。两种模式不可在同一份旧图快照上直接切换，
  切换前应确认快照/WAL 完整或接受一次全量重放。
- `HOUYI_HNSW_PLAN_APPLY`：合图默认“读锁规划 + 短写锁提交”，构图期间
  检索可并发读图；设为 `0` 回退旧的整图持锁路径。对照测试必须在两侧
  使用相同开关，并在结果中记录。
- 任何 HNSW 调优都要在同一套 `VDB_*` 参数下重跑完整
  `load -> optimize -> search`，不能只比较单点微基准。

## 9. 复现与回归

- 本仓库自测：`./scripts/perf_smoke_fast.sh` 与 `benchmarks/vectordb_bench_houyidb/`；
- 保留失败运行的原始 JSON 与日志，修复合入后同参数复跑；
- 结果报告建议同时给出绝对数值与相对比值，并注明机器规格与版本。
