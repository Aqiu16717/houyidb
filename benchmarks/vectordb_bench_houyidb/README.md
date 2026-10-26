# HouyiDB VectorDBBench 适配器

> [English](README.en.md)

本目录提供一个可公开复现的 HouyiDB 适配器安装包：它把 `houyidb/` 注入本地
VectorDBBench 源码，并注册 `DB.HouyiDB`、三个配置工厂和 `vectordb_bench houyidb`
子命令。本适配器不单独发布为 pip 包，也不复制或打包 VectorDBBench 本身。

## 1. 适用版本

- VectorDBBench `0.1.x` 系列（2026-07 前后的上游 `main` 目录结构）。
- Python `>= 3.11`，VectorDBBench 仍使用 `pydantic < 2`。
- 适配器依赖 VectorDBBench 的 `DB` 枚举、`init_cls` / `config_cls` /
  `case_config_cls` 三个工厂，以及 `vectordb_bench/cli/vectordbbench.py`
  的注册方式。
- `install.py` 会在写入任何文件前检查这些扩展点。上游若改动结构，它会明确
  报错并停止，而不是猜测替换。
- 已验证范围：安装、重复安装（幂等）、注册结果与 Python 语法；未在本文档中
  承诺任何性能数值。

## 2. 安装

先准备 VectorDBBench 工作副本：

```bash
git clone https://github.com/zilliztech/VectorDBBench.git /path/to/VectorDBBench
cd /path/to/VectorDBBench
python3 -m venv .venv
source .venv/bin/activate
python -m pip install -U pip
python -m pip install -e .
python -m pip install "psycopg[binary]>=3.1" requests
```

然后在 HouyiDB 仓库中运行安装脚本：

```bash
cd /path/to/houyidb
python3 benchmarks/vectordb_bench_houyidb/install.py \
  --vdb-root /path/to/VectorDBBench
```

安装脚本会：

1. 复制 `houyidb/` 到
   `$VDB_ROOT/vectordb_bench/backend/clients/houyidb/`；
2. 在 `backend/clients/__init__.py` 插入
   `HouyiDB = "HouyiDB"` 和三个工厂分支；
3. 在 `cli/vectordbbench.py` 插入 import 与 `cli.add_command(HouyiDB)`；
4. 运行 `compileall` 校验安装结果。

重复执行是安全的：已存在的枚举、工厂分支和 CLI 注册会被检测并跳过。安装
失败时，脚本会把原始注册文件与旧适配器保存在
`$VDB_ROOT/.houyidb-install-backup/<时间戳>/`，并打印逐条回滚命令。

验证命令：

```bash
cd /path/to/VectorDBBench
source .venv/bin/activate
vectordb_bench houyidb --help
```

HTTP 传输只需要 `requests`；pgwire 传输还需要 `psycopg[binary]>=3.1`。

## 3. 启动 houyidb-server

在 HouyiDB 仓库中构建并启动服务。以下参数与
[`VDBBENCH_PROTOCOL.md`](../../VDBBENCH_PROTOCOL.md) 的 benchmark 端口一致：

```bash
cargo build --release -p houyi-server

# 清空旧 WAL，保证冷启动；仅用于本地开发目录
rm -rf /tmp/houyidb-bench

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

确认服务可达：

```bash
curl -fsS http://127.0.0.1:17000/v1/health
# {"status":"ok"}
```

说明：

- `--pgwire-hnsw-m` 与 `--pgwire-hnsw-efc` 应与 benchmark 的 `--m` /
  `--ef-construction` 对齐。
- 本适配器不会删除服务端数据。`--drop-old` 只由 VectorDBBench 传入，无法
  通过客户端清空 HouyiDB；每次冷启动对比都应使用新的 WAL 目录和新的服务
  进程。
- 本机访问建议设置 `NO_PROXY=127.0.0.1,localhost`，避免系统代理劫持。
- HNSW 服务端调优环境变量（如 `HOUYI_HNSW_MAX_BUILD_BATCH`）必须在启动
  `houyi-server` 前设置，并写入结果记录。

## 4. 一键运行

`run_bench.sh` 使用协议默认参数：

```bash
export VDB_ROOT=/path/to/VectorDBBench
cd /path/to/houyidb
./benchmarks/vectordb_bench_houyidb/run_bench.sh
```

脚本会：

- 校验 `VDB_ROOT` 与适配器安装状态；
- 在未激活虚拟环境时自动 source `$VDB_ROOT/.venv/bin/activate`；
- 检查 `http://$HOUYI_HOST:$HOUYI_HTTP_PORT/v1/health`；
- 调用 `vectordb_bench houyidb`，默认 case 为 `Performance1536D50K`，
  `k=10`、`m=24`、`ef_construction=400`、`ef_search=256`、
  并发 `1`、持续 `30s`；
- 结束后打印最新结果 JSON 路径。

可用环境变量覆盖默认值：

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `VDB_ROOT` | 必填 | VectorDBBench 源码目录 |
| `HOUYI_HOST` | `127.0.0.1` | HouyiDB 地址 |
| `HOUYI_PG_PORT` | `15432` | pgwire 端口 |
| `HOUYI_HTTP_PORT` | `17000` | HTTP 端口 |
| `VDB_HOUYI_TRANSPORT` | `pgwire` | `pgwire` 或 `http` |
| `VDB_CASE` | `Performance1536D50K` | VectorDBBench case |
| `VDB_K` | `10` | 返回条数 |
| `VDB_M` | `24` | HNSW `m` |
| `VDB_EFC` | `400` | `ef_construction` |
| `VDB_EFS` | `256` | `ef_search` |
| `VDB_NUM_CONCURRENCY` | `1` | 并发检索数 |
| `VDB_CONCURRENCY_DURATION` | `30` | 每档并发持续秒数 |
| `VDB_LOAD_CONCURRENCY` | `0` | 加载并发；`0` 表示 CPU 核数 |
| `VDB_METRIC` | `COSINE` | `L2`、`COSINE` 或 `IP` |
| `HOUYI_TIMEOUT_SEC` | `600` | 单请求超时秒数 |
| `HOUYI_USE_TLS` | `0` | 设为 `1` 时 HTTP 使用 HTTPS |
| `HOUYI_API_KEY` | 空 | 可选认证 token |
| `TASK_LABEL` | `houyidb-<时间戳>` | 结果任务标签 |

脚本会把额外命令行参数原样追加给 `vectordb_bench`，例如切换 case：

```bash
./benchmarks/vectordb_bench_houyidb/run_bench.sh --case-type Performance1536D50K
```

## 5. 直接使用 CLI

不经过脚本时，可使用完整命令：

```bash
source /path/to/VectorDBBench/.venv/bin/activate
export NO_PROXY=127.0.0.1,localhost

vectordb_bench houyidb \
  --host 127.0.0.1 \
  --port 15432 \
  --http-port 17000 \
  --transport pgwire \
  --case-type Performance1536D50K \
  --k 10 \
  --m 24 \
  --ef-construction 400 \
  --ef-search 256 \
  --metric-type COSINE \
  --load-concurrency 0 \
  --num-concurrency 1 \
  --concurrency-duration 30 \
  --task-label "houyidb-$(date +%Y%m%d-%H%M%S)"
```

适配器参数：

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--host` | 必填 | HouyiDB 主机 |
| `--port` | `15432` | pgwire 端口 |
| `--http-port` | `17000` | HTTP 端口；`--transport http` 时同时是数据端口 |
| `--transport` | `pgwire` | `pgwire` 或 `http` |
| `--use-tls` | 关闭 | HTTP 使用 HTTPS |
| `--api-key` | 空 | 认证 token，也可用 `HOUYI_API_KEY` |
| `--timeout-sec` | `600` | 单请求超时 |
| `--metric-type` | `COSINE` | `L2`、`COSINE` 或 `IP` |
| `--m` | `24` | HNSW `m` |
| `--ef-construction` | `400` | HNSW `ef_construction` |
| `--ef-search` | `256` | HNSW `ef_search` |

其余参数（`--case-type`、`--k`、`--num-concurrency`、
`--concurrency-duration`、`--task-label` 等）来自 VectorDBBench 公共 CLI。

`mock` 传输仅保留用于离线适配器自测，不会被 `run_bench.sh` 使用，也不得
用于发布任何性能或召回数字。

## 6. Transport 与接口路径

| transport | 写入 | 检索 | 结果接口标注 |
|-----------|------|------|--------------|
| `pgwire` | PostgreSQL extended query + `BYTEA` 二进制批量写入 | `BYTEA` 查询参数；label 过滤自动回退 HTTP | `pgwire-binary` |
| `http` | `POST /v1/vectors/upsert` JSON | `POST /v1/vectors/search` JSON | `benchmark-http` |

两种传输都使用 HTTP 控制面：

- `GET /v1/health`：健康检查；
- `POST /v1/optimize`：触发冻结与索引排空；
- `GET /v1/index/status`：轮询 `pending_count` 直到为 0。

不同 transport 的 QPS、延迟和 recall 不应混在同一张对比表；结果必须注明
数据面路径。完整口径见
[`VDBBENCH_PROTOCOL.md`](../../VDBBENCH_PROTOCOL.md)。

认证：

- HTTP：`--api-key` / `HOUYI_API_KEY` 以 `Authorization: Bearer <token>` 发送；
- pgwire：token 作为连接密码发送。pgwire 密码通道为明文协议，只应在
  localhost 或受信网络使用；
- 建议通过环境变量传递 token，避免出现在 shell 历史和进程参数中。

## 7. 结果记录

VectorDBBench 原始结果写入：

```text
$VDB_ROOT/vectordb_bench/results/HouyiDB/result_*.json
```

每次运行至少保存：

- 原始 `result_*.json`，不裁剪；
- `task_label`、case、`k`、`m`、`ef_construction`、`ef_search`、并发与持续
  时间；
- transport、HouyiDB commit/版本、`houyi-server` 完整启动参数、相关
  `HOUYI_HNSW_*` 环境变量；
- `recall`、`ndcg`、QPS、p50/p95/p99、`load_duration`、
  `insert_duration`、`optimize_duration`；
- 失败信息：OOM、超时、连接失败、重试与日志片段。

`load_duration` 包含插入与构建排空；异步构图下不能只看 `insert_duration`。
失败运行同样应保留原始 JSON 和日志。

## 8. 故障排查

- **`No such command 'houyidb'` 或 `Unknown DB: HouyiDB`**：适配器未安装或注册
  文件被覆盖，重新运行 `install.py`。
- **`transport=pgwire requires psycopg`**：在 VectorDBBench 的虚拟环境中安装
  `psycopg[binary]>=3.1`。
- **健康检查失败**：确认 `--http-port` 指向
  `--benchmark-http-addr`，而不是 pgwire 端口；检查 `NO_PROXY`。
- **结果为空或 recall 异常**：确认未复用旧 WAL、服务端 `--vector-dim` 与
  case 维度一致，并确认 `--pgwire-hnsw-m` / `--pgwire-hnsw-efc` 与 CLI
  参数一致。
- **安装中止**：按脚本打印的备份路径恢复两个注册文件，并重新确认
  VectorDBBench 版本。

## 9. 卸载

安装脚本不提供自动卸载。干净的 VectorDBBench 工作副本可执行：

```bash
rm -rf "$VDB_ROOT/vectordb_bench/backend/clients/houyidb"
git -C "$VDB_ROOT" checkout -- \
  vectordb_bench/backend/clients/__init__.py \
  vectordb_bench/cli/vectordbbench.py
```

若该工作副本还有其他未提交修改，请勿直接 `git checkout`，应手动删除
`DB.HouyiDB` 枚举、三个工厂分支和 CLI 注册。

## 10. 来源与许可证

本适配器由 HouyiDB 项目维护，按 Apache-2.0 发布，完整文本见
[`LICENSE`](../../LICENSE)。它针对 VectorDBBench 的公开客户端与 CLI 扩展点
编写；VectorDBBench 是 Zilliz 的独立项目，按其 MIT 许可证使用。本目录只
安装适配器，不重新分发 VectorDBBench。
