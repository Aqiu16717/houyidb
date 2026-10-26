# HII: Hybrid Intelligent Index — Design Document

> [中文](HII_DESIGN.md)

> A multi-tier LSM graph index for heterogeneous storage (memory / GPU VRAM / NVMe SSD).
> Each of the three tiers has a distinct role; background compaction reconciles write and query efficiency, and atomic swap enables zero-downtime updates.

---

## 1. Architecture Overview

```
                        ┌──────────────────────────────┐
                        │         SearchRouter          │
                        │   根据 recall_target 决定     │
                        │   查询发往哪些层              │
                        └──────────┬───────────────────┘
                                   │
              ┌────────────────────┼────────────────────┐
              │                    │                    │
    ┌─────────▼─────────┐ ┌───────▼──────────┐ ┌──────▼──────────┐
    │  L0: 内存增量层    │ │  L1: GPU 主索引   │ │  L2: 磁盘持久层  │
    │  Memtable + 小图   │ │  CAGRA Graph      │ │  DiskANN Graph   │
    │  M=4, 实时可见     │ │  高吞吐, 亚毫秒   │ │  高召回, 兜底    │
    └─────────┬─────────┘ └───────┬──────────┘ └──────┬──────────┘
              │                    │                    │
              └────────────────────┼────────────────────┘
                                   │
                        ┌──────────▼──────────┐
                        │    结果归并 + Top-K   │
                        │    精确距离重排序      │
                        └──────────────────────┘
```

### Tier Responsibilities

| Tier | Storage medium | Structure | Responsibility |
|----|---------|------|------|
| **L0** | Memory | B-Tree + simplified HNSW (M=4) | Absorbs real-time writes, provides millisecond-level visibility, and serves queries for the newest data |
| **L1** | GPU VRAM (or memory) | CAGRA graph (or in-memory HNSW) | Primary query tier, 90%+ of traffic, high throughput with sub-millisecond latency |
| **L2** | NVMe SSD | Vamana/DiskANN graph | High-recall fallback, full persistence, supports datasets larger than memory |

### Query Flow: Three-Way Merge

```
1. 查询同时发往 L0 + L1（+ L2 可选）
2. 各层独立检索，返回 Top-K 结果
3. 归并去重，精确距离重排序，截取最终 Top-K
4. recall_target 控制路由:
   - 0.95: 仅 L0 + L1
   - 0.99: L0 + L1 + L2
```

---

## 2. L0 — In-Memory Incremental Tier

### Structure

```
L0Segment:
  ├── memtable: BTreeMap<VectorId, Vec<f32>>   # 可写，吸收写入
  ├── small_graph: HnswGraph(M=4, ef_c=64)     # 轻量小图，毫秒级检索
  └── wal: 预写日志                              # 防崩溃恢复
```

### Write Path

```
Insert(id, vector):
  1. 追加 WAL 记录（持久化保证）
  2. memtable.insert(id, vector)
  3. small_graph.insert(id, vector)           # M=4 插入极快
  4. 立即可见（毫秒级）
```

### Freeze and Switch

```
当 L0 memtable 达到阈值 (默认 100K 条):
  1. 当前 L0 标记为 Frozen，不可再写
  2. 新建 Active L0 吸收后续写入
  3. Frozen L0 加入 L0→L1 合并队列
  4. 查询同时搜索 Active L0 + Frozen L0
```

---

## 3. L1 — Primary Index Tier (GPU Accelerated)

### GPU Path: CAGRA

```
L1Graph (CAGRA):
  ├── 驻留 GPU 显存
  ├── 由稳定的批量数据构建（只读）
  ├── 查询：GPU 并行，亚毫秒延迟
  └── 重建：GPU 全量重建千万级向量，需数秒
```

### CPU Fallback Path: Standard HNSW

In environments without a GPU (e.g., an Apple M1 development machine), L1 degrades to an optimized in-memory HNSW:

```
L1Graph (CPU fallback):
  ├── HnswGraph(M=24, ef_c=400)               # 复用现有实现
  ├── 驻留内存
  └── 通过 feature flag 切换: simd-cagra / portable
```

### L0 → L1 Compaction (Mini-Compaction)

```
触发条件: Frozen L0 累积 ≥ 100K 条

流程:
  1. 取出所有 Frozen L0 段
  2. 与 L1 现有数据合并，去重（last-wins）
  3. GPU: 全量重建 CAGRA 图（数秒）
     CPU: 增量更新 HNSW 图
  4. Atomic swap: L1 指针原子切换为新图
  5. 查询流量无缝迁移，零停机
```

---

## 4. L2 — Disk Persistence and High-Recall Tier

### Structure

```
L2Graph (DiskANN/Vamana):
  ├── 存储在 NVMe SSD
  ├── 索引全量压缩数据（PQ 压缩，16-32x）
  ├── 图结构内存缓存 + 向量数据按需从 SSD 读取
  └── 支持超内存规模（TB 级）
```

### Query Triggering

- Enabled when `recall_target ≥ 0.99`
- Pays the cost of SSD random reads only in high-recall scenarios
- Results are merged with L0/L1

### L1 → L2 Compaction (Base Compaction)

```
触发条件: L1 重建次数 ≥ N（默认 10 次）

流程:
  1. L1 全量数据 + 现有 L2 数据合并
  2. 异步重建 DiskANN 图（分钟级，不阻塞查询）
  3. 原子替换 L2 图指针
```

---

## 5. Background Compaction Pipeline

```
                    ┌──────────┐
                    │  Active  │  持续吸收写入
                    │   L0     │
                    └────┬─────┘
                         │ 达到阈值
                    ┌────▼─────┐
                    │  Frozen  │  冻结，不可写
                    │   L0     │
                    └────┬─────┘
                         │ Mini-Compaction
                    ┌────▼─────┐
                    │    L1    │  全量重建 CAGRA/HNSW
                    │  主索引  │  Atomic swap 切换
                    └────┬─────┘
                         │ 累积 N 次
                    ┌────▼─────┐
                    │    L2    │  异步重建 DiskANN
                    │  持久层  │  Atomic swap 切换
                    └──────────┘
```

**Key properties**:
- All compactions run **asynchronously in the background** and do not block foreground reads or writes
- Atomic pointer swaps provide **zero-downtime** switching
- WAL guarantees that L0 data **is not lost**

---

## 6. Performance Estimates

| Scenario | Current (pure HNSW) | HII L0+L1 | HII L0+L1+L2 |
|------|:---:|:---:|:---:|
| Single-write latency | Must enter the builder queue | **Millisecond-level** (L0 direct write) | Same as left |
| Write visibility | Seconds | **Real-time** | Real-time |
| 50K build | ~40s | L1 rebuild ~2-5s | - |
| 1M build | ~450s | GPU rebuild ~3-10s | - |
| QPS (recall 0.95) | 200 | **500-1000** (GPU) | - |
| QPS (recall 0.99) | 200 | - | **300-500** |
| P99 latency | 6.5ms | **<1ms** (GPU) | **3-5ms** |
| Maximum data scale | Memory-limited | Memory-limited | **NVMe-limited (TB scale)** |
| Online updates | Requires rebuild | **Real-time L0 + background compaction** | Same as left |

---

## 7. Phased Implementation

### Phase 1: L0 Incremental Tier + L0→In-Memory L1 Compaction (3-4 weeks)

- L0 Memtable + simplified HNSW (M=4) + WAL
- L0 freeze mechanism (Active / Frozen switch)
- L0 → L1 Mini-Compaction (merge + rebuild L1 HNSW)
- Atomic pointer swap
- Query routing: search Active L0 + Frozen L0 + L1 concurrently
- **Milestone**: real-time write visibility + zero-downtime updates

### Phase 2: GPU L1 Acceleration (2-3 weeks, requires GPU hardware)

- Integrate the CAGRA graph (NVIDIA cuVS / RAPIDS)
- GPU full-rebuild pipeline
- `simd-cagra` feature flag
- CPU fallback path remains compatible

### Phase 3: L2 Disk Persistence Tier (2-3 weeks)

- DiskANN/Vamana graph implementation
- PQ vector compression (16-32x)
- On-demand reads from NVMe SSD
- L1 → L2 Base Compaction
- `recall_target` parameter controls L2 activation

### Phase 4: Learned Router + Adaptivity (1-2 weeks)

- Automatically select tiers based on recall_target
- Learn query characteristics (hot/cold)
- Predictive L2 prefetching

---

## 8. Integration with Existing Code

```
houyi-vector/src/
├── lib.rs                      # HotBufferIndex（保留作为回退）
├── hnsw_graph.rs               # HnswGraph（保留，L0 小图 + L1 CPU 回退）
├── hii/
│   ├── mod.rs                  # HybridIntelligentIndex 主结构
│   ├── l0.rs                   # L0Memtable + Active/Frozen 切换 + WAL
│   ├── l1.rs                   # L1Graph: CAGRA (GPU) 或 HNSW (CPU)
│   ├── l2.rs                   # L2Graph: DiskANN + PQ 压缩
│   ├── compaction.rs           # Mini/Base Compaction 流水线
│   └── router.rs               # SearchRouter: recall_target 路由
├── distance.rs                 # 复用
└── hnsw_tuning.rs              # 复用
```

`HybridIntelligentIndex` implements the `VectorIndex` trait and is transparent to `houyi-server`. The existing `HotBufferIndex` is retained as a lightweight mode.
