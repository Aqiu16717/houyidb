# Changelog

> [English](CHANGELOG.en.md)

## v0.2.0-beta.5 (2026-10-25)

### Added
- **向量化接入（路线 A）**：`docs/EMBEDDING_GUIDE.md`（Ollama / OpenAI /
  TEI / mock 配方）、`examples/text_ingest.py`（切块→自动入库→检索）、
  `scripts/embed_smoke.sh`（链路自检）、`scripts/mock_embedding_server.py`
  （无语义 mock 上游）。试用手册新增 §2.6；发布包原生包含 docs/examples/scripts。
- 试用手册支持 Apple Silicon / Intel 双平台预编译包与向量化章节。

### Changed
- `async-trait 0.1.89 → 0.1.92`：Rust 1.99 新增 `clippy::double_must_use`，
  旧版宏展开触发 `-D warnings`；升级后 1.99 clippy 全绿。
- **CI/发布修复**：5 个失效的 action 固定 SHA 替换为有效 v-tag commit；
  Release 的 `macos-x86_64` 改为在 `macos-latest` 交叉编译（`macos-13` 退役）；
  CI Docker build 与 Release 流水线恢复。

### Fixed
- 向量化示例/脚本修正：忽略系统代理、非 UTF-8/缺失文件友好报错、
  chunk/overlap/batch/k 参数校验、smoke 响应解析守卫。
- 依赖安全：`cmov 0.5.4`、`vitest ≥4.1.11`（TS SDK 开发依赖）、
  `tokio` workspace 下限 `1.54.1`。

## v0.2.0-beta.4 (2026-10-25)

### Security
- **依赖加固**：`tokio 1.52.0 → 1.54.1`（RUSTSEC-2026-0386 `AsyncFd` I/O
  safety，unsound 警告）；`ctutils 0.4.2 → 0.4.5`（RUSTSEC-2026-0385
  非常量时间掩码；链路为 dev 依赖
  `tokio-postgres → postgres-protocol → digest → ctutils`）。
  `cargo audit` 复核：0 漏洞 / 0 警告；`cargo test --all` 660 项全绿。

## v0.2.0-beta.3 (2026-10-25)

### Added
- **标签过滤连续向量缓存**：命中向量按标签惰性拷贝为连续 `Vec<f32>`（Cosine
  预先单位化），过滤查询退化为顺序 SIMD 扫描 + top-k，写后失效。Cohere 1M
  1% 过滤实测 ef128 **1365 QPS**、ef256 **1386 QPS**，recall 1.0。
- **HNSW 实验旋钮**（默认关闭）：`HOUYI_HNSW_KEEP_PRUNED=1`（keepPruned
  connections，成熟召回 .996→.9976）、`HOUYI_HNSW_RERANK_EXPAND=N`（查询侧
  邻居扩展）、`HOUYI_HNSW_COMMIT_CHUNK=N`（串行合图短锁提交）。
- `/metrics` 构图日志新增 `graph`（图内节点数），用于诊断 main 与图的覆盖差。

### Changed
- **HNSW 合图默认改为「读锁规划 + 短写锁提交」**：`run_async_builder_once`
  的 ef_construction 规划移出全局写锁（读锁内并行完成），仅连边提交持短写锁
  并按 `parallel_chunk_size × parallel_plan_group_chunks` 分组。Streaming
  实测 50% 阶段 1000 条串行查询 104s → **3.9s**、p99 1.72s → **21ms**；
  80/100% 检查点 206s/211s → **164s/201s**；成熟 recall 不变（1× .9876、
  2× .996），写后 QPS 1× **1355/1353**。提交窗口用独立 gate 与 `remove`
  互斥（不占墓碑锁，避免批量写入排队）；`HOUYI_HNSW_PLAN_APPLY=0` 可回退
  旧路径。同日 Cohere 1M 批量加载总墙钟 +11%（1055s → 1170s，recall/QPS
  同档；对加载时间敏感时用回退开关）。详见
  `docs/benchmarks/houyidb-performance-2026-10.md`。
- **HNSW 默认连接密度保持 2×M**：1× 会把旧 2× 图快照判为不兼容并回退重放；
  若 WAL 已被 checkpoint 截断，存在数据丢失窗口。因此代码默认仍为 2×（兼容
  旧快照），高 QPS 场景显式设 `HOUYI_HNSW_LAYER0_MULT=1`（VDB campaign
  默认 1×）。同机实测 1× 的 OpenAI/Cohere QPS 提升约 35–60%，Cohere
  recall 0.9971 → 0.9883。

## v0.2.0-beta.2 (2026-10-21)

### Added
- **正确性验证（P4.5）**: proptest 属性化、cargo-fuzz、shuttle/loom、检索 oracle、
  crash chaos、turmoil 确定性网络仿真、Stateright 模型检查、Jepsen 式线性化、soak 哨兵。
- **复制硬化（P4.6）**: 协议 v5（日志前缀指纹 + 认证 token）、promote/关停互斥、
  HaLevel 枚举、FileWal 毒化、apply 原地重试、DNS/握手独立超时、SDK 集成 hermetic 化。
- **审计日志（P4.7.0）**: SHA-256 哈希链、启动校验、写失败 poison、审计校验端点。
- **RBAC（P4.7.1）**: 配置式主体 admin/writer/reader + collection 作用域。
- **租户配额（P4.7.2）**: 每主体 collection 数/在途/写速率限制（HTTP 429、pgwire 54000）。
- **静态加密（P4.7.3）**: AES-256-GCM；WAL v3、图快照 v2、旧 JSON 快照加密；
  本地 keyring + KeyProvider、密钥轮换、复制跨密钥指纹一致。
- **供应链与运维（P4.8）**: CI cargo audit/deny + CycloneDX SBOM；告警 Runbook 与
  Prometheus 规则；SLO/错误预算；RTO/MTTR 实测脚本；Helm chart
  （StatefulSet/PVC/Secret）；psql/psycopg2/pgbench 外部客户端一致性；
  移除未维护的 hwloc 原生依赖（NUMA 改读 sysfs）。
- **pgwire 兼容**: 接受标准客户端的 BEGIN/COMMIT/ROLLBACK/SET 空操作
  （libpq 系驱动隐式发送），修复 psycopg2 无法连接的问题。

### Changed
- **pgwire 授权**：Simple Query 与 Extended Execute 共用拒绝链；配额拒绝
  （54000）与越权拒绝（42501）指标分账。
- **依赖升级**：ureq 3 / rustls-pki-types（移除 rustls 0.21 与未维护的
  rustls-pemfile）；anyhow、crossbeam-epoch、tokio-postgres、rustls-webpki 安全升级。
- **TLS PEM 解析**迁移到 rustls-pki-types；Docker/CI 不再需要 libhwloc。

### Fixed
- 复制分歧（同 LSN 不同内容）不再被幂等跳过错失；缺密钥启动 fail-closed。
- pgwire SELECT 隐式建集合绕过配额；DELETE 未知 collection 误报 429。
- 代码审查批次三十二至三十五共 24 项（含 3 HIGH）。

## v0.2.0-beta (2026-05-13)

### Added
- **多 Collection 支持**: `CollectionManager` 管理独立维度/索引的 Collection，HTTP + pgwire 双协议路由
- **四种索引后端**: `--index-type hnsw|clustered|flat-clustered|flat`
- **ClusteredHnsw**: K-Means 分区 + 并行子图构建，加载快 73%（50K）/ 55%（1M）
- **FlatClustered**: K-Means 分区 + SIMD 暴力检索，QPS 231（50K 最高）
- **自适应 nprobe**: 查询时动态计算 (k × 2500 / cluster_size)，小 k 不浪费算力
- **Product Quantization**: 全局共享码本 + SDC 查表检索，128D 压缩 32x（16B/向量）
- **PqFlatClustered + mmap 重排**: 内存 15MB + 磁盘 488MB，SIFT1M recall 0.937, 延迟 3.9ms
- **houyi-mobile**: 独立 C ABI 库，支持 iOS/Android 原生集成
- **houyi-mobile MobileDB**: 多 Collection 管理，与服务器 API 一脉相承
- **SIFT1M 对标 MicroNN**: 延迟 3.9ms, recall 0.937, RAM 15MB
- **RegionPool 无锁化**: CAS 替代 Mutex
- **NEON SIMD 补全**: 4 累加器展开，Cosine/L2/点积全路径
- **统一配置系统**: houyi-core/config.rs 集中管理

### Changed
- **IndexBackend 枚举**: 抽象 HNSW/ClusteredHnsw/FlatClustered/Flat 四种后端
- **main.rs 模块拆分**: 6339 → 3040 行（types.rs / tests.rs 独立）
- **向量数据共享 Arc**: main_index 与 HnswGraph 同一份数据，消除三重存储
- **PAX 页清理**: 移除装饰性 ptr/layout，向量列改为列式连续存储
- **optimize() 实现**: HnswGraph.optimize_compact() 启发式邻居重选

### Fixed
- 配置散落：41 处 env::var 集中到 21 处
- houyi-core 零 Mutex（CAS 替代）
- SQL 解析器升级 (sqlparser 0.62 AST)

### Known Issues
- 15 个 pgwire 字节级测试因 IndexBackend 重构出现 1 字节偏差（不影响功能）
- ClusterGraph 自动分裂未完成（占位代码）
- PQ 未接入 --index-type
- simd-avx512 仅骨架

---

## v0.1.0-alpha (2026-04-24)

### Added
- 超融合单体内存向量数据库骨架
- HNSW 图引擎 + 双速写入路径
- pgwire + HTTP 双协议
- WAL 持久化 + 冷启动恢复
- PAX 页抽象 + Region 池
