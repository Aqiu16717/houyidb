# HouyiDB SLO 与错误预算

> [English](SLO.en.md)

> 版本：2026-10-21（初版）。目标值在首批生产数据/压测基线后按季度校准；
> 任何放宽必须有变更记录与容量评估。
> EN: initial SLOs. Calibrate quarterly or after the first production baseline.

## 1. 服务边界

| 面 | 入口 | 说明 |
|----|------|------|
| 数据面（HTTP） | `/v1/collections/{name}/...` | upsert/search/delete/hybrid |
| 数据面（pgwire） | SQL Simple/Extended | 与 HTTP 共享同一 CollectionManager |
| 管理面 | `/v1/admin/*`、`/metrics` | 不计入数据面可用性 |
| 探针 | `/v1/health`、`/readyz` | 仅用于可用性判定，不计入请求成功率 |

## 2. SLI 与目标

### 2.1 可用性（Availability）

- **SLI**：成功请求数 / 有效请求数。成功 =非 5xx 且未超时；4xx（鉴权/配额/
  客户端错误）不计入分母，但 401/403/429 的**突增**走安全/容量告警。
- **目标**：**99.9% / 自然月**（允许停机 43m 12s）。
- **数据源**：入口负载均衡/Blackbox 探针的请求计数与状态码；或后续在
  HTTP/pgwire 中间件补充的请求计数器。

### 2.2 延迟（Latency）

| 操作 | SLI | 目标 |
|------|-----|------|
| 向量检索 | p99 端到端 | ≤ 100ms（k ≤ 10，≤ 1M 向量，HNSW） |
| upsert（async/semi） | p99 端到端 | ≤ 200ms |
| upsert（full_sync） | p99 端到端 | ≤ 2× `replication_sync_timeout_ms`（默认 2s → 4s 上限） |
| pgwire 简单查询 | p99 | 与 HTTP 同口径 |

延迟只统计成功请求；超时/5xx 计入可用性错误预算。目标以部署侧观测与
`VDBBENCH_PROTOCOL.md` 的自测数据为准。

### 2.3 复制与持久性（RPO / RTO）

| HA 级别 | RPO 目标 | 说明 |
|---------|----------|------|
| full_sync | **0** | 已 ACK 的写必须在 follower 持久态；由 full_sync ACK 语义与故障演练断言 |
| semi_sync | 0（等待超时则降级） | 超时降级 async，窗口见 `sync_timeouts_total` |
| async | 有界（≤ 1s） | 受 WAL flush 策略约束；不是持久性承诺 |

- **RTO 目标**：单节点重启恢复 ≤ 60s（有图快照，1M 向量级）；手动 promote
  ≤ 30s。按部署指南做重启/promote 演练并记录实测。
- 复制滞后 SLI：`houyidb_replication_lag_entries` p95 ≤ 1000 且 5 分钟均值
  ≤ 100；超过走[复制滞后 Runbook](runbooks/replication-lag.md)。

## 3. 错误预算策略

- 月度预算 = 0.1% × 当月分钟数；按可用性 SLI 消耗。
- **Burn rate 告警**：
  - 快燃：1 小时窗口 burn rate > 14.4（2% 预算/小时）→ **page**；
  - 慢燃：6 小时窗口 burn rate > 6（5% 预算/6 小时）→ 工单。
- 预算耗尽（剩余 < 10%）：
  1. 冻结非紧急的功能合并，优先可靠性修复；
  2. 复盘最近变更与事故；
  3. 恢复发布需 DBA/负责人双确认。
- 预算充足也不豁免安全/正确性门禁（SLO 不是降低质量的理由）。

## 4. 与告警/演练的接线

- Prometheus 规则：`deploy/prometheus/houyidb-alerts.yml`（runbook_url 指向
  `docs/runbooks/`）。
- RTO/MTTR：每次演练保留记录，用于季度校准。
- 事故分级：P0（数据丢失/不可用 > 30m/安全事件）、P1（可用性受影响）、
  P2（单节点/非核心）；P0/P1 必须复盘并更新 SLO 或 Runbook。

## 5. 待补测量（诚实的当前缺口）

- HTTP 请求总数/延迟直方图目前依赖外部探针或网关指标；后续可在中间件补充
  `houyidb_http_requests_total{status}` 与延迟直方图（不在本 SLO 初版范围内）。
- RTO 目标需要至少 3 次演练数据（不同数据量档位）后才能定为承诺值。
