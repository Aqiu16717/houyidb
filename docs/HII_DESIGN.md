# HII: Hybrid Intelligent Index — 设计文档

> [English](HII_DESIGN.en.md)

> 面向异构存储（内存 / GPU 显存 / NVMe SSD）的多层 LSM 图索引。
> 三层各司其职，后台合并调和写入与查询效率，原子切换实现零停机更新。

---

## 1. 架构总览

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

### 三层职责

| 层 | 存储介质 | 结构 | 职责 |
|----|---------|------|------|
| **L0** | 内存 | B-Tree + 简化 HNSW (M=4) | 吸收实时写入，毫秒级可见，承载最新数据查询 |
| **L1** | GPU 显存 (或内存) | CAGRA 图 (或内存 HNSW) | 主力查询层，90%+ 流量，高吞吐亚毫秒延迟 |
| **L2** | NVMe SSD | Vamana/DiskANN 图 | 高召回兜底，全量持久化，支持超内存数据集 |

### 查询流程：三路归并

```
1. 查询同时发往 L0 + L1（+ L2 可选）
2. 各层独立检索，返回 Top-K 结果
3. 归并去重，精确距离重排序，截取最终 Top-K
4. recall_target 控制路由:
   - 0.95: 仅 L0 + L1
   - 0.99: L0 + L1 + L2
```

---

## 2. L0 — 内存增量层

### 结构

```
L0Segment:
  ├── memtable: BTreeMap<VectorId, Vec<f32>>   # 可写，吸收写入
  ├── small_graph: HnswGraph(M=4, ef_c=64)     # 轻量小图，毫秒级检索
  └── wal: 预写日志                              # 防崩溃恢复
```

### 写入路径

```
Insert(id, vector):
  1. 追加 WAL 记录（持久化保证）
  2. memtable.insert(id, vector)
  3. small_graph.insert(id, vector)           # M=4 插入极快
  4. 立即可见（毫秒级）
```

### 冻结与切换

```
当 L0 memtable 达到阈值 (默认 100K 条):
  1. 当前 L0 标记为 Frozen，不可再写
  2. 新建 Active L0 吸收后续写入
  3. Frozen L0 加入 L0→L1 合并队列
  4. 查询同时搜索 Active L0 + Frozen L0
```

---

## 3. L1 — 主索引层 (GPU 加速)

### GPU 路径：CAGRA

```
L1Graph (CAGRA):
  ├── 驻留 GPU 显存
  ├── 由稳定的批量数据构建（只读）
  ├── 查询：GPU 并行，亚毫秒延迟
  └── 重建：GPU 全量重建千万级向量，需数秒
```

### CPU 回退路径：标准 HNSW

在没有 GPU 的环境中（如 Apple M1 开发机），L1 退化为优化的内存 HNSW：

```
L1Graph (CPU fallback):
  ├── HnswGraph(M=24, ef_c=400)               # 复用现有实现
  ├── 驻留内存
  └── 通过 feature flag 切换: simd-cagra / portable
```

### L0 → L1 合并 (Mini-Compaction)

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

## 4. L2 — 磁盘持久化与高召回层

### 结构

```
L2Graph (DiskANN/Vamana):
  ├── 存储在 NVMe SSD
  ├── 索引全量压缩数据（PQ 压缩，16-32x）
  ├── 图结构内存缓存 + 向量数据按需从 SSD 读取
  └── 支持超内存规模（TB 级）
```

### 查询触发

- `recall_target ≥ 0.99` 时启用
- 仅在高召回场景下付出 SSD 随机读代价
- 结果与 L0/L1 归并

### L1 → L2 合并 (Base Compaction)

```
触发条件: L1 重建次数 ≥ N（默认 10 次）

流程:
  1. L1 全量数据 + 现有 L2 数据合并
  2. 异步重建 DiskANN 图（分钟级，不阻塞查询）
  3. 原子替换 L2 图指针
```

---

## 5. 后台合并流水线

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

**关键特性**：
- 所有合并**后台异步**执行，不阻塞前台读写
- 原子指针交换实现**零停机**切换
- WAL 保证 L0 数据**不丢失**

---

## 6. 性能预估

| 场景 | 当前 (纯 HNSW) | HII L0+L1 | HII L0+L1+L2 |
|------|:---:|:---:|:---:|
| 单条写入延迟 | 需进 builder 队列 | **毫秒级** (L0 直写) | 同左 |
| 写入可见性 | 秒级 | **实时** | 实时 |
| 50K 构建 | ~40s | L1 重建 ~2-5s | - |
| 1M 构建 | ~450s | GPU 重建 ~3-10s | - |
| QPS (recall 0.95) | 200 | **500-1000** (GPU) | - |
| QPS (recall 0.99) | 200 | - | **300-500** |
| P99 延迟 | 6.5ms | **<1ms** (GPU) | **3-5ms** |
| 最大数据规模 | 内存限制 | 内存限制 | **NVMe 限制 (TB 级)** |
| 在线更新 | 需重建 | **实时 L0 + 后台合并** | 同左 |

---

## 7. 分阶段实施

### 阶段 1：L0 增量层 + L0→内存 L1 合并（3-4 周）

- L0 Memtable + 简化 HNSW (M=4) + WAL
- L0 冻结机制（Active / Frozen 切换）
- L0 → L1 Mini-Compaction（合并 + 重建 L1 HNSW）
- 原子指针切换
- 查询路由：同时搜索 Active L0 + Frozen L0 + L1
- **里程碑**：实时写入可见 + 零停机更新

### 阶段 2：GPU L1 加速（2-3 周，需 GPU 硬件）

- 集成 CAGRA 图 (NVIDIA cuVS / RAPIDS)
- GPU 全量重建流水线
- `simd-cagra` feature flag
- CPU 回退路径保持兼容

### 阶段 3：L2 磁盘持久层（2-3 周）

- DiskANN/Vamana 图实现
- PQ 向量压缩（16-32x）
- NVMe SSD 按需读取
- L1 → L2 Base Compaction
- `recall_target` 参数控制 L2 启用

### 阶段 4：Learned Router + 自适应（1-2 周）

- 根据 recall_target 自动选择层
- 查询特性学习（热点/冷门）
- 预测性 L2 预取

---

## 8. 与现有代码的集成

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

`HybridIntelligentIndex` 实现 `VectorIndex` trait，对 `houyi-server` 透明。现有 `HotBufferIndex` 保留作为轻量模式。
