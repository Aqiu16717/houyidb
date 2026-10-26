//! 冷启动恢复：快照加载 + WAL 重放。
//! Cold-start recovery: snapshot load + WAL replay.

use crate::index_backend::IndexBackend;
use houyi_persistence::encryption::Encryption;
use houyi_persistence::graph_snapshot_file::{
    graph_snapshot_path_for_wal, read_graph_snapshot_with_encryption,
    write_graph_snapshot_atomic_with_encryption,
};
use houyi_persistence::vector_snapshot::{
    decode_vector_snapshot_v1_with_encryption, snapshot_path_for_wal,
};
use houyi_persistence::vector_wal::decode_vector_wal_payload;
use houyi_persistence::WalEntry;
use houyi_vector::{peek_graph_snapshot_dims_metric, DistanceMetric};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 图快照指标（P4.4.0e）：恢复侧 loads/rejects + 写侧 writes/bytes，
/// 主索引与 collection 共用同一组计数（区别在日志字段，指标族不分裂）。
/// EN: Graph snapshot counters shared by the benchmark index and collections.
#[derive(Debug, Default)]
pub struct GraphSnapshotMetrics {
    /// 启动以来写出的快照文件数。
    /// EN: Snapshot files written since startup.
    pub writes_total: AtomicU64,
    /// 启动以来成功恢复的快照数。
    /// EN: Snapshots successfully restored since startup.
    pub loads_total: AtomicU64,
    /// 被丢弃的快照数（信封损坏 / 头非法 / 恢复失败）。
    /// EN: Snapshots discarded (corrupt envelope / bad header / restore failure).
    pub rejects_total: AtomicU64,
    /// 写出的 payload 字节数（不含信封头）。
    /// EN: Payload bytes written (envelope header excluded).
    pub bytes_written_total: AtomicU64,
}

impl GraphSnapshotMetrics {
    pub fn inc_writes(&self, payload_bytes: usize) {
        self.writes_total.fetch_add(1, Ordering::Relaxed);
        self.bytes_written_total
            .fetch_add(payload_bytes as u64, Ordering::Relaxed);
    }

    pub fn inc_loads(&self) {
        self.loads_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_rejects(&self) {
        self.rejects_total.fetch_add(1, Ordering::Relaxed);
    }
}

/// 加载 benchmark 主索引的 HNSW 图快照（P4.4.0）。存在且恢复成功 → `Some(checkpoint_lsn)`，
/// 调用方以此为 `min_lsn_exclusive` 跳过已覆盖的 WAL 记录；不存在/损坏/恢复失败/非 Hnsw
/// 后端一律 `Ok(None)`（warn 后由调用方回退 JSON 快照或全量重放）。
///
/// **例外（P4.7.3）**：无密钥入口遇到加密快照时返回 `Err`（缺密钥致命）——
/// 这是「快照不得让冷启动失败」的唯一例外，且必须如此：静默回退会丢掉加密
/// 历史。生产接线用 `_with_encryption` 变体传入 keyring。
/// EN: everything falls back to `Ok(None)` **except** an encrypted snapshot seen
/// through the keyless entry point, which errors out — silently falling back
/// would drop encrypted history.
pub async fn apply_graph_snapshot_if_present(
    wal_path: &Path,
    index: &IndexBackend,
    metrics: &GraphSnapshotMetrics,
) -> std::io::Result<Option<u64>> {
    apply_graph_snapshot_if_present_with_encryption(wal_path, index, metrics, None).await
}

/// 带解密的图快照加载（P4.7.3）。缺密钥是**致命错误**：把它当作「损坏→重放」
/// 会在加密 WAL 上继续失败，或更糟——在仅明文 WAL 的节点上静默丢掉整段历史。
/// EN: decryption-aware load. A missing key is fatal — treating it as
/// corruption either fails later on the encrypted WAL or, worse, silently
/// drops history on a node whose WAL happens to be plaintext.
pub async fn apply_graph_snapshot_if_present_with_encryption(
    wal_path: &Path,
    index: &IndexBackend,
    metrics: &GraphSnapshotMetrics,
    encryption: Option<&Encryption>,
) -> std::io::Result<Option<u64>> {
    let path = graph_snapshot_path_for_wal(wal_path);
    let snap = match read_graph_snapshot_with_encryption(&path, encryption) {
        Ok(None) => return Ok(None),
        Ok(Some(s)) => s,
        Err(e) if e.is_missing_key() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            ));
        }
        Err(e) => {
            metrics.inc_rejects();
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "[houyi-recovery] graph snapshot discarded (corrupt envelope) — falling back"
            );
            return Ok(None);
        }
    };
    match index.restore_graph_snapshot(&snap.payload).await {
        None => {
            // 非 HNSW 后端 = 功能不适用，非损坏：既非 load 也非 reject，不计数。
            // EN: Non-HNSW backend = not applicable, not corrupt: neither loaded nor rejected,
            // EN: uncounted.
            tracing::debug!(
                path = %path.display(),
                "[houyi-recovery] graph snapshot ignored — backend is not HNSW"
            );
            Ok(None)
        }
        Some(Err(e)) => {
            metrics.inc_rejects();
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "[houyi-recovery] graph snapshot discarded (restore failed) — falling back"
            );
            Ok(None)
        }
        Some(Ok(())) => {
            metrics.inc_loads();
            tracing::info!(
                path = %path.display(),
                checkpoint_lsn = snap.checkpoint_lsn,
                "houyi-vector: loaded graph snapshot"
            );
            Ok(Some(snap.checkpoint_lsn))
        }
    }
}

/// collection 图快照目录：`<collections.wal 所在目录>/snapshots/`。
/// EN: Per-collection snapshot dir: `snapshots/` next to collections.wal.
pub fn collection_snapshots_dir(collections_wal_path: &Path) -> PathBuf {
    collections_wal_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("snapshots")
}

/// 恢复全部 collection 的图快照（collections WAL 重放之前调用），返回
/// `collection 名 → checkpoint_lsn`。单个快照损坏/恢复失败只弃用该 collection 的快照
/// （全量重放它），不影响其他 collection。非 Hnsw 后端的 collection 安全跳过。
///
/// 缺密钥是**整体致命错误**（P4.7.3）：不存在「无密钥包装器」——曾经有一个
/// `unwrap_or_default()` 的便利包装器，会把缺密钥静默转成空 checkpoint 表，
/// 这正是设计要禁止的丢历史路径（OCR 批次三十五 HIGH）。
/// EN: Restore every collection's graph snapshot. A missing key fails the
/// whole startup; there is deliberately no infallible keyless wrapper — it
/// would turn the fatal case into a silent full replay.
pub async fn apply_collection_graph_snapshots_with_encryption(
    manager: &crate::collection::CollectionManager,
    collections_wal_path: &Path,
    metrics: &GraphSnapshotMetrics,
    encryption: Option<&Encryption>,
) -> std::io::Result<HashMap<String, u64>> {
    let dir = collection_snapshots_dir(collections_wal_path);
    let mut checkpoints = HashMap::new();
    let entries = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(checkpoints), // 无快照目录：全新部署，正常路径。
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        // 文件名即 collection 名（loader 只认固定后缀，无路径注入面）。
        // EN: The file name is the collection name (loader accepts only fixed suffixes;
        // EN: no path injection surface).
        let Some(name) = file_name
            .to_str()
            .and_then(|s| s.strip_suffix(".graph_snapshot.bin"))
        else {
            continue;
        };
        let path = entry.path();
        let snap = match read_graph_snapshot_with_encryption(&path, encryption) {
            Ok(Some(s)) => s,
            Ok(None) => continue,
            Err(e) if e.is_missing_key() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                ));
            }
            Err(e) => {
                metrics.inc_rejects();
                tracing::warn!(
                    collection = %name,
                    error = %e,
                    "[houyi-recovery] collection graph snapshot discarded (corrupt) — full replay"
                );
                continue;
            }
        };
        // 建 collection 需要 dims：只读快照头即可，不做全量解码。
        // EN: dims come from a header-only peek (no full decode).
        let dims = match peek_graph_snapshot_dims_metric(&snap.payload) {
            Ok((dims, _metric)) => dims,
            Err(e) => {
                metrics.inc_rejects();
                tracing::warn!(
                    collection = %name,
                    error = %e,
                    "[houyi-recovery] collection graph snapshot discarded (bad header) — full replay"
                );
                continue;
            }
        };
        let (col, _) = match manager.get_or_create_flagged(name, Some(dims)) {
            Ok(v) => v,
            Err(e) => {
                metrics.inc_rejects();
                tracing::warn!(
                    collection = %name,
                    error = %e,
                    "[houyi-recovery] collection graph snapshot discarded (create failed) — full replay"
                );
                continue;
            }
        };
        match col.backend.restore_graph_snapshot(&snap.payload).await {
            Some(Ok(())) => {
                metrics.inc_loads();
                tracing::info!(
                    collection = %name,
                    checkpoint_lsn = snap.checkpoint_lsn,
                    "houyi-vector: loaded collection graph snapshot"
                );
                checkpoints.insert(name.to_string(), snap.checkpoint_lsn);
            }
            Some(Err(e)) => {
                metrics.inc_rejects();
                tracing::warn!(
                    collection = %name,
                    error = %e,
                    "[houyi-recovery] collection graph snapshot discarded (restore failed) — full replay"
                );
            }
            None => {
                tracing::debug!(
                    collection = %name,
                    "[houyi-recovery] graph snapshot ignored — backend is not HNSW"
                );
            }
        }
    }
    Ok(checkpoints)
}

/// 全量图快照写出器（P4.4.0e）：周期/阈值/关停三通道复用同一例程。
/// EN: One snapshot routine shared by the periodic, WAL-threshold, and
/// shutdown triggers.
///
/// checkpoint 取点（正确性核心，两处不对称）：
/// EN: Checkpoint provenance — deliberately asymmetric:
///
/// - 主索引用 applied sidecar：sidecar 只在 insert_batch 返回后推进，是唯一被
///   证明满足「lsn ≤ checkpoint ⇒ 已入图（drain 后）」的水位线；与既有 JSON
///   快照、WAL 截断同语义。drain 后取 wal max_lsn 会被在途写击穿——并发写
///   append 完 WAL（lsn ≤ max）却尚未 insert_batch，该向量不进图却被跳过
///   重放，重启即丢。
/// - collections 无 sidecar：checkpoint = collections.wal next_lsn-1，随后逐
///   collection write_lock 过一遍（写锁覆盖「WAL append → insert_batch」复合
///   写，锁传递完成后所有 lsn ≤ checkpoint 的写必然已入待构图队列），再逐
///   collection drain。反向竞态（图中向量 lsn > checkpoint）无害——重放幂等
///   补插。
///
/// 并发纪律：调用方（周期任务/关停）用 tokio Mutex 互斥本例程；快照期间持图
/// 读锁编码，client 写不受阻（走 WAL + 热缓冲）。
pub struct GraphSnapshotWriter {
    pub index: IndexBackend,
    pub wal_path: PathBuf,
    pub applied_sidecar: PathBuf,
    pub collections: std::sync::Arc<crate::collection::CollectionManager>,
    pub collections_wal: std::sync::Arc<crate::types::WalHandle>,
    pub collections_wal_path: PathBuf,
    pub metrics: std::sync::Arc<GraphSnapshotMetrics>,
    /// 静态加密句柄（P4.7.3）：Some 时快照写成加密 v2 信封。
    /// EN: when set, snapshots are written as encrypted v2 envelopes.
    pub encryption: Option<std::sync::Arc<Encryption>>,
}

impl GraphSnapshotWriter {
    /// 写主索引 + 全部 collection 的图快照；任何单点失败只 warn 跳过，
    /// 快照永远不得让主流程失败（与恢复侧「快照不得阻塞冷启动」对称）。
    /// EN: Best-effort — individual failures warn and skip; a snapshot must
    /// never fail the caller.
    pub async fn write_all(&self) {
        self.write_main_index().await;
        self.write_collections().await;
    }

    async fn write_main_index(&self) {
        // 先取 checkpoint（sidecar 落后于 insert 完成点，安全），再 drain——
        // drain 把 lsn ≤ checkpoint 的队列全部冲进图。
        // EN: Take the checkpoint first (sidecar lags the insert completion point; safe),
        // EN: then drain — drain flushes all queued records with lsn <= checkpoint.
        let checkpoint = match houyi_persistence::wal_sidecar::read_applied_lsn(
            &self.applied_sidecar,
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "graph snapshot: read applied_lsn sidecar failed, skipping main-index snapshot");
                return;
            }
        };
        drain_pending_builder(&self.index).await;
        let Some(payload) = self.index.encode_graph_snapshot().await else {
            return; // 非 HNSW 后端：无图可快照（功能不适用，非故障）。
        };
        let path = graph_snapshot_path_for_wal(&self.wal_path);
        let bytes = payload.len();
        let encryption = self.encryption.clone();
        // payload 可达数百 MB，同步 fs 写移出 runtime worker；tmp+rename
        // 原子，任务被 abort 时阻塞写仍在后台完成，不产生撕裂文件。
        // EN: The fs write is moved off the runtime; an aborted task lets the
        // blocking write finish in the background (atomic rename, no tears).
        match tokio::task::spawn_blocking(move || {
            write_graph_snapshot_atomic_with_encryption(
                &path,
                checkpoint,
                &payload,
                encryption.as_deref(),
            )
        })
        .await
        {
            Ok(Ok(())) => {
                self.metrics.inc_writes(bytes);
                tracing::info!(
                    checkpoint_lsn = checkpoint,
                    payload_bytes = bytes,
                    "houyi-vector: graph snapshot written"
                );
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "graph snapshot write failed");
            }
            Err(e) => {
                tracing::error!(error = %e, "graph snapshot write task join failed");
            }
        }
    }

    async fn write_collections(&self) {
        let names = self.collections.list();
        if names.is_empty() {
            return;
        }
        // 先取 checkpoint（共享 WAL 的全局 LSN），再锁传递 + drain——顺序即
        // 正确性（见结构体文档）。next_lsn 在 Async 模式下要穿有界通道等写
        // 线程应答（阻塞 recv）——直接调会把 runtime worker 钉在写线程健康
        // 上；移 spawn_blocking + 5s 上限，写线程挂死时跳过本轮（best-effort
        // 纪律；超时后阻塞任务仍钉在线程上，与 total_wal_bytes 同一取舍）。
        // EN: checkpoint first, then lock-pass + drain. The next_lsn read is
        // moved off the runtime and bounded at 5s — a wedged WAL writer
        // skips this round instead of pinning a runtime worker.
        let wal = std::sync::Arc::clone(&self.collections_wal);
        let read = tokio::task::spawn_blocking(move || wal.next_lsn());
        let checkpoint = match tokio::time::timeout(std::time::Duration::from_secs(5), read).await {
            Ok(Ok(v)) => v.saturating_sub(1),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "graph snapshot: collections next_lsn read task failed, skipping collections snapshot");
                return;
            }
            Err(_) => {
                tracing::warn!(
                    "graph snapshot: collections next_lsn read timed out, skipping collections snapshot"
                );
                return;
            }
        };
        let dir = collection_snapshots_dir(&self.collections_wal_path);
        if let Err(e) = fs::create_dir_all(&dir) {
            tracing::error!(path = %dir.display(), error = %e, "graph snapshot: create snapshots dir failed");
            return;
        }
        for name in names {
            let Some(col) = self.collections.get(&name) else {
                continue; // list→get 间隙被 drop：跳过即可，下轮快照覆盖。
            };
            // 锁传递：等所有 checkpoint 前 append 的写完成 insert_batch。
            // EN: Lock hand-off: wait for all writes appended before the checkpoint to
            // EN: finish insert_batch.
            drop(col.write_lock.lock().await);
            drain_pending_builder(&col.backend).await;
            let Some(payload) = col.backend.encode_graph_snapshot().await else {
                continue; // 非 HNSW 后端。
            };
            let path = dir.join(format!("{name}.graph_snapshot.bin"));
            let bytes = payload.len();
            let encryption = self.encryption.clone();
            // 同主索引：大 payload 同步写移出 runtime worker。
            // EN: Same as the main index: large synchronous payload writes move off the
            // EN: runtime worker.
            match tokio::task::spawn_blocking(move || {
                write_graph_snapshot_atomic_with_encryption(
                    &path,
                    checkpoint,
                    &payload,
                    encryption.as_deref(),
                )
            })
            .await
            {
                Ok(Ok(())) => {
                    self.metrics.inc_writes(bytes);
                    tracing::info!(
                        collection = %name,
                        checkpoint_lsn = checkpoint,
                        payload_bytes = bytes,
                        "houyi-vector: collection graph snapshot written"
                    );
                }
                Ok(Err(e)) => {
                    tracing::error!(collection = %name, error = %e, "collection graph snapshot write failed");
                }
                Err(e) => {
                    tracing::error!(collection = %name, error = %e, "collection graph snapshot write task join failed");
                }
            }
        }
    }
}

pub async fn apply_vector_snapshot_if_present(
    wal_path: &Path,
    index: &IndexBackend,
    expected_dim: usize,
    expected_metric: DistanceMetric,
) -> std::io::Result<Option<u64>> {
    apply_vector_snapshot_if_present_with_encryption(
        wal_path,
        index,
        expected_dim,
        expected_metric,
        None,
    )
    .await
}

/// 带解密的旧 JSON 快照恢复（P4.7.3）。缺密钥是致命错误。
/// EN: decrypting load of the legacy JSON snapshot; a missing key is fatal.
pub async fn apply_vector_snapshot_if_present_with_encryption(
    wal_path: &Path,
    index: &IndexBackend,
    expected_dim: usize,
    expected_metric: DistanceMetric,
    encryption: Option<&Encryption>,
) -> std::io::Result<Option<u64>> {
    let path = snapshot_path_for_wal(wal_path);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)?;
    let snap = decode_vector_snapshot_v1_with_encryption(&bytes, encryption)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if snap.dim != expected_dim {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "vector snapshot dim {} != server vector_dim {}",
                snap.dim, expected_dim
            ),
        ));
    }
    let file_metric = DistanceMetric::parse_str(&snap.metric).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("vector snapshot: unknown metric {:?}", snap.metric),
        )
    })?;
    if file_metric != expected_metric {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "vector snapshot metric {:?} != index metric {:?}",
                file_metric, expected_metric
            ),
        ));
    }
    const CHUNK: usize = 8192;
    let mut skipped_non_finite = 0usize;
    // 毒行明细限流器跨所有 chunk 共享（WHY 见 PoisonedRowLog）。
    // EN: One rate limiter shared across all chunks (see PoisonedRowLog for rationale).
    let mut poison_log = PoisonedRowLog::new();
    for chunk in snap.items.chunks(CHUNK) {
        let batch: Vec<(u64, Vec<f32>)> =
            chunk.iter().map(|it| (it.id, it.values.clone())).collect();
        // 历史毒数据容忍（WHY 见 partition_finite_batch 文档）：跳过而非中止冷启动。
        // EN: Tolerate legacy poisoned rows — skip them instead of aborting cold start.
        let (batch, skipped) =
            partition_finite_batch(batch, ReplaySource::Snapshot, &mut poison_log);
        skipped_non_finite += skipped;
        if batch.is_empty() {
            continue;
        }
        index
            .insert_batch(batch)
            .await
            .map_err(std::io::Error::other)?;
    }
    drain_pending_builder(index).await;
    // `vectors` 报告实际载入行数（= 总条目 − 跳过毒行），让「潜在数据丢失」事件自描述，
    // 无需读者自行相减；`total_items` 保留快照原始条目数。
    // EN: `vectors` is the count actually loaded (total − skipped) so a data-loss event
    // is self-describing; `total_items` keeps the snapshot's raw item count.
    let loaded = snap.items.len() - skipped_non_finite;
    // `suppressed_detail` > 0 表示有毒行明细被限流折叠（前 16 行之后只计数，不打印）。
    // EN: `suppressed_detail` > 0 means per-row detail was rate-limited (counted, not
    // printed beyond the first 16 rows).
    tracing::info!(
        checkpoint_lsn = snap.checkpoint_lsn,
        vectors = loaded,
        total_items = snap.items.len(),
        skipped_non_finite,
        suppressed_detail = poison_log.suppressed(),
        "houyi-vector: loaded vector snapshot"
    );
    Ok(Some(snap.checkpoint_lsn))
}

/// 冷启动重放向量 WAL；跳过非 JSON 或旧版裸 `bootstrap` 条目。
/// `min_lsn_exclusive`：若 `Some(c)`，则跳过 `lsn <= c` 的记录（快照已覆盖）。
/// Replay vector WAL on cold start; skip non-JSON or legacy raw `bootstrap` records.
pub async fn replay_vector_wal_into_index(
    entries: &[WalEntry],
    index: &IndexBackend,
    expected_dim: usize,
    min_lsn_exclusive: Option<u64>,
) -> std::io::Result<usize> {
    let mut applied = 0usize;
    let mut skipped_non_finite = 0usize;
    // 整批向量全部为毒行的 WAL 记录数：这些记录未向索引应用任何行，不得计入 applied。
    // EN: WAL records whose entire payload was poisoned — no row applied, must not be
    // counted in `applied` (otherwise the caller's "applied N batch(es)" log overstates
    // real recovery work and hides fully-lost records).
    let mut fully_skipped_records = 0usize;
    // 毒行明细限流器跨全部 WAL 记录共享（WHY 见 PoisonedRowLog）。
    // EN: One rate limiter shared across every WAL record (see PoisonedRowLog).
    let mut poison_log = PoisonedRowLog::new();
    for e in entries {
        if let Some(c) = min_lsn_exclusive {
            if e.lsn <= c {
                continue;
            }
        }
        match decode_vector_wal_payload(&e.payload) {
            Ok(None) => {}
            Ok(Some(batch)) => {
                if batch.dim != expected_dim {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "WAL record dim {} != server vector_dim {}",
                            batch.dim, expected_dim
                        ),
                    ));
                }
                // 历史毒数据容忍：当前 JSON 编码不可能产出非有限行（NaN 序列化为 null，
                // 解码即失败），但手工损坏/旧版本/未来编码可能携带；HiiL0 现在会硬拒，
                // 不跳过则整条记录让冷启动必死。WHY 同快照路径。
                // EN: Tolerate poisoned rows (hand-corrupted/legacy/future encoders);
                // HiiL0 now hard-rejects them, so propagating would brick cold start.
                let (vectors, skipped) = partition_finite_batch(
                    batch.vectors,
                    ReplaySource::Wal(e.lsn),
                    &mut poison_log,
                );
                skipped_non_finite += skipped;
                if vectors.is_empty() {
                    fully_skipped_records += 1;
                } else {
                    index
                        .insert_batch(vectors)
                        .await
                        .map_err(std::io::Error::other)?;
                    applied += 1;
                }
            }
            Err(err) => {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, err));
            }
        }
    }
    if skipped_non_finite > 0 {
        // `suppressed_detail` > 0：超过前 16 行样例后毒行明细被限流折叠，只在此汇总。
        // EN: When suppressed_detail > 0, per-row detail beyond the first 16 samples was
        // rate-limited and surfaces only here in the aggregate.
        tracing::warn!(
            skipped_non_finite,
            fully_skipped_records,
            batches_applied = applied,
            suppressed_detail = poison_log.suppressed(),
            "[houyi-recovery] WAL replay skipped poisoned vector(s) with non-finite components (NaN/±Inf)"
        );
    }
    Ok(applied)
}

/// Collections WAL 重放统计。
/// EN: Replay counters for the collections WAL.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CollectionsReplayStats {
    /// 应用的 collection_create 记录数（幂等：已存在的 collection 不计）。
    /// EN: Applied collection_create records (idempotent — existing collections don't count).
    pub creates: usize,
    /// 应用的 upsert 批次数。
    /// EN: Applied upsert batches.
    pub upsert_batches: usize,
    /// 因图快照覆盖（LSN ≤ checkpoint）而跳过向量侧的 upsert 批次数（标量行仍重建）。
    /// EN: Upsert batches whose vector side was skipped as snapshot-covered
    /// (scalar rows were still rebuilt).
    pub upsert_batches_skipped_snapshot: usize,
    /// 重写入索引的向量行数（不含毒行）。
    /// EN: Vector rows re-applied (poisoned rows excluded).
    pub upsert_rows: usize,
    /// 重建的标量行数。
    /// EN: Rebuilt scalar rows.
    pub scalar_rows: usize,
    /// 应用的删除批次数。
    /// EN: Applied delete batches.
    pub delete_batches: usize,
    /// 因图快照覆盖而跳过后端 delete 的批次数（标量墓碑仍重建）。
    /// EN: Delete batches skipped on the backend side as snapshot-covered
    /// (scalar tombstones were still rebuilt).
    pub delete_batches_skipped_snapshot: usize,
    /// 重建的墓碑数。
    /// EN: Rebuilt tombstones.
    pub tombstones: usize,
}

/// 冷启动重放 collections WAL：按 LSN 顺序重建全部 Collection（向量 + 标量页 + 墓碑）。
/// Replay the collections WAL on cold start: rebuild every collection in LSN order
/// (vectors + scalar pages + tombstones).
///
/// 语义与写入侧严格对偶：
/// - `Create` → `get_or_create`（幂等；index_type 以 Manager 装配默认值为准——v1 全部
///   Collection 共享服务端默认后端，记录里的 index_type 仅作向前兼容留档）；
/// - `UpsertBatch` → 向量进索引 + label/payload 经 [`crate::scalar_json::build_collection_scalar_row`]
///   重建标量行（与写入侧同一函数，逐字节语义等价）；裸向量不建行（同写入侧约定）；
/// - `Delete` → 墓碑 + 向量删除。目标 collection 缺失的 Delete（如 WAL 被手工截断）
///   跳过不炸——重放必须对不完整历史保持韧性。
///
/// EN: Strict dual of the write path. `Create` is idempotent; v1 collections all share
/// the server default backend, so the recorded index_type is kept only for forward
/// compatibility. `UpsertBatch` re-inserts vectors and rebuilds scalar rows through
/// the same `build_collection_scalar_row` used by writes; bare vectors get no row.
/// `Delete` replays as tombstone + vector delete; a Delete for a missing collection
/// (e.g. hand-truncated WAL) is skipped, not fatal — replay must tolerate
/// incomplete history.
/// `graph_checkpoints`：collection 名 → 图快照 checkpoint_lsn（collections.wal LSN 空间）。
/// LSN ≤ checkpoint 的记录跳过后端向量插入/删除（图快照已含，含墓碑），但标量行与
/// 标量墓碑仍全量重建——标量 PAX 页是纯内存 O(N) 结构，重建便宜，由此省掉标量序列化。
/// EN: `graph_checkpoints` maps collection name → snapshot checkpoint_lsn (in the
/// collections.wal LSN space). Records with LSN ≤ checkpoint skip backend vector
/// insert/delete (the graph snapshot already carries them, tombstones included)
/// but still rebuild scalar rows/tombstones — scalar PAX pages are in-memory and
/// O(N) to rebuild, which avoids serializing scalars into the snapshot.
pub async fn replay_collections_wal_into_manager(
    entries: &[WalEntry],
    manager: &crate::collection::CollectionManager,
    graph_checkpoints: &HashMap<String, u64>,
) -> std::io::Result<CollectionsReplayStats> {
    use houyi_persistence::collection_wal::decode_collection_wal_payload;

    let mut stats = CollectionsReplayStats::default();
    let mut skipped_non_finite = 0usize;
    let mut poison_log = PoisonedRowLog::new();
    for e in entries {
        let record = match decode_collection_wal_payload(&e.payload) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(err) => {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, err));
            }
        };
        apply_collection_wal_record(
            manager,
            e.lsn,
            record,
            |name| graph_checkpoints.get(name).copied(),
            &mut stats,
            &mut skipped_non_finite,
            &mut poison_log,
        )
        .await?;
    }
    // 重放写入只进待构图队列；排空后重启前写入的向量立即可检索。
    // EN: Replayed inserts only enqueue; drain so pre-restart vectors are searchable
    // immediately after boot.
    for name in manager.list() {
        if let Some(col) = manager.get(&name) {
            drain_pending_builder(&col.backend).await;
        }
    }
    if skipped_non_finite > 0 {
        tracing::warn!(
            skipped_non_finite,
            suppressed_detail = poison_log.suppressed(),
            "[houyi-recovery] collections WAL replay skipped poisoned vector(s) \
             with non-finite components"
        );
    }
    Ok(stats)
}

/// 应用单条 collections WAL 记录——冷启动重放与复制 follower 实时 apply 同源，
/// 杜绝语义分叉。
/// Apply a single collections WAL record — shared by cold replay and the
/// replication follower's live apply, so the two paths can never drift.
///
/// `checkpoint_of`：collection 名 → 图快照 checkpoint_lsn（冷重放传映射查询；
/// follower 实时 apply 传 `|_| None`——实时记录必在任一 checkpoint 之后）。
/// EN: `checkpoint_of` maps collection name → snapshot checkpoint_lsn (cold
/// replay passes the map lookup; the live follower passes `|_| None` since
/// streamed records always postdate any checkpoint).
pub(crate) async fn apply_collection_wal_record(
    manager: &crate::collection::CollectionManager,
    lsn: u64,
    record: houyi_persistence::collection_wal::CollectionWalRecord,
    checkpoint_of: impl Fn(&str) -> Option<u64>,
    stats: &mut CollectionsReplayStats,
    skipped_non_finite: &mut usize,
    poison_log: &mut PoisonedRowLog,
) -> std::io::Result<()> {
    use houyi_persistence::collection_wal::CollectionWalRecord;
    match record {
        CollectionWalRecord::Create { name, dims, .. } => {
            let (_, created) = manager
                .get_or_create_flagged(&name, Some(dims))
                .map_err(std::io::Error::other)?;
            if created {
                stats.creates += 1;
            }
        }
        CollectionWalRecord::UpsertBatch { name, dim, items } => {
            let (col, _) = manager
                .get_or_create_flagged(&name, Some(dim))
                .map_err(std::io::Error::other)?;
            if col.dims != dim {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "collections WAL record dim {dim} != collection '{name}' dims {}",
                        col.dims
                    ),
                ));
            }
            let covered = checkpoint_of(&name).is_some_and(|c| lsn <= c);
            if covered {
                // 图快照已含该批向量（及当时墓碑）：跳过后端插入，仅重建标量行。
                // EN: The snapshot already carries these vectors — skip the
                // backend insert, rebuild scalar rows only.
                stats.upsert_batches_skipped_snapshot += 1;
            } else {
                let batch: Vec<(u64, Vec<f32>)> =
                    items.iter().map(|it| (it.id, it.values.clone())).collect();
                // 毒行容忍与主索引重放同纪律（HiiL0 硬拒非有限分量，不跳过则冷启动必死）。
                // EN: Same poisoned-row tolerance as the main-index replay.
                let (vectors, skipped) =
                    partition_finite_batch(batch, ReplaySource::Wal(lsn), poison_log);
                *skipped_non_finite += skipped;
                if !vectors.is_empty() {
                    col.backend
                        .insert_batch(vectors)
                        .await
                        .map_err(std::io::Error::other)?;
                    stats.upsert_batches += 1;
                }
                stats.upsert_rows += items.len() - skipped;
            }
            let mut scalars = col
                .scalars
                .lock()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            for it in &items {
                let row = crate::scalar_json::build_collection_scalar_row(
                    it.label.as_deref(),
                    it.payload.as_ref(),
                );
                if !row.is_empty() {
                    scalars.append_row(it.id, row);
                    stats.scalar_rows += 1;
                }
            }
        }
        CollectionWalRecord::Delete { name, ids } => {
            let Some(col) = manager.get(&name) else {
                tracing::warn!(
                    collection = %name,
                    ids = ids.len(),
                    "[houyi-recovery] collections WAL delete for unknown collection \
                     skipped — truncated history?"
                );
                return Ok(());
            };
            {
                let mut scalars = col
                    .scalars
                    .lock()
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                for id in &ids {
                    scalars.tombstone(*id);
                }
            }
            let covered = checkpoint_of(&name).is_some_and(|c| lsn <= c);
            if covered {
                // 墓碑已在图快照内：跳过后端 delete，标量墓碑已在上方重建。
                // EN: The snapshot already carries the tombstone — skip the
                // backend delete (scalar tombstone rebuilt above).
                stats.delete_batches_skipped_snapshot += 1;
                stats.tombstones += ids.len();
                return Ok(());
            }
            for id in &ids {
                // 后端 delete 对不存在 id 返回 Ok(false)，重放天然幂等；
                // 不支持 delete 的后端 Err 与写入侧同口径（写入侧映射 400，
                // 重放侧说明 WAL 与该后端组合从未产生过 delete 记录——传播为启动错误）。
                // EN: backend delete is idempotent for missing ids; an Err here means
                // the backend never supported delete, so no such WAL record could
                // have been produced — propagate as a startup error.
                col.backend
                    .delete(*id)
                    .await
                    .map_err(std::io::Error::other)?;
            }
            stats.delete_batches += 1;
            stats.tombstones += ids.len();
        }
        CollectionWalRecord::SetHaLevel { name, ha_level } => {
            // 元数据记录无 checkpoint 分界（图快照不覆盖 ha_level——它只活在
            // 内存与 WAL 里），照常应用。解码侧已把字符串定型为枚举（P4.6.3），
            // 此处不再有可失败的 parse。
            // EN: Metadata records ignore snapshot checkpoints; the decoder
            // already validated the level into the enum (P4.6.3).
            let Some(col) = manager.get(&name) else {
                // 目标 collection 不在 = 历史被截断或记录顺序异常；与 Delete
                // 同纪律告警跳过（fail-fast 会让一条畸形元数据卡死整个启动）。
                // EN: Missing target collection = truncated history or bad record order;
                // EN: warn and skip like Delete (fail-fast would let one malformed metadata
                // EN: record block startup).
                tracing::warn!(
                    collection = %name,
                    ha_level = %ha_level,
                    "[houyi-recovery] set_ha_level for unknown collection skipped — \
                     truncated history?"
                );
                return Ok(());
            };
            col.set_ha_level(ha_level);
        }
    }
    Ok(())
}

/// 毒行来源标签：仅在真正跳过毒行时才经 [`fmt::Display`] 格式化成字符串。WHY: 干净
/// 重放是大循环热路径（可能有数十万条 WAL 记录），不能为每条记录预先 `format!` 一个
/// 永远不会被打印的来源串。
/// EN: Poisoned-row source label — formatted via Display only when a row is actually
/// skipped, so a clean large replay loop pays no per-record String allocation.
#[derive(Clone, Copy)]
enum ReplaySource {
    /// 向量快照。
    /// EN: vector snapshot.
    Snapshot,
    /// 向量 WAL 记录，携带其 LSN。
    /// EN: a vector WAL record with its LSN.
    Wal(u64),
}

impl fmt::Display for ReplaySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot => f.write_str("vector snapshot"),
            Self::Wal(lsn) => write!(f, "WAL lsn={lsn}"),
        }
    }
}

/// 毒行诊断限流器：一次完整重放（WAL 全部记录或快照全部分片）共享一个实例。
/// EN: Rate limiter for poisoned-row diagnostics — one instance is shared by a whole
/// replay pass (all WAL records, or all snapshot chunks).
///
/// WHY（为什么需要限流）：毒行可能以百万计——整段历史 WAL/快照都可能由无校验版本写入。
/// 旧实现逐行 `eprintln!`，每行一次 `write(2)` 的**无界 stderr I/O** 会主导冷启动耗时、
/// 刷爆磁盘日志，而前几条之后的明细不含任何新增诊断价值（来源只有快照或若干 WAL lsn）。
/// 因此前 [`Self::DETAILED_LIMIT`] 条毒行打全量明细（来源 + id，足够定位），其余只累加
/// `suppressed`，在重放汇总行里一次性报出，百万级毒行冷启动时防止无界 stderr I/O。
/// EN: A poisoned legacy dataset may contain millions of bad rows; the old per-row
/// eprintln! was unbounded stderr I/O (one write(2) per row) that could dominate cold
/// start and flood logs, while detail beyond the first rows adds no diagnostic value.
/// The first DETAILED_LIMIT rows log full source+id detail; the rest are counted and
/// reported once in the replay summary.
pub(crate) struct PoisonedRowLog {
    /// 已打印全量明细的毒行数。
    /// Rows already emitted with full detail.
    detailed_logged: usize,
    /// 被折叠、未打印明细的毒行数（并入汇总）。
    /// Rows whose detail was suppressed (folded into the summary).
    suppressed: usize,
}

impl PoisonedRowLog {
    /// 全量明细样例上限：16 行足以覆盖来源（快照/WAL lsn）定位需求。
    /// Detailed-sample cap: 16 rows are enough to attribute the source.
    const DETAILED_LIMIT: usize = 16;

    pub(crate) fn new() -> Self {
        Self {
            detailed_logged: 0,
            suppressed: 0,
        }
    }

    /// 记录一条被跳过的毒行：前 DETAILED_LIMIT 条打全量明细，之后只累加折叠计数。
    /// Record one skipped poisoned row: full detail for the first DETAILED_LIMIT,
    /// then a suppressed count only.
    fn record_skip(&mut self, id: u64, source: ReplaySource) {
        if self.detailed_logged < Self::DETAILED_LIMIT {
            self.detailed_logged += 1;
            tracing::warn!(
                source = %source,
                id,
                "[houyi-recovery] skip poisoned vector (non-finite NaN/±Inf component)"
            );
        } else {
            self.suppressed += 1;
        }
    }

    fn suppressed(&self) -> usize {
        self.suppressed
    }
}

/// 从重放批次中剔除含 NaN/±Inf 分量的行，返回 `(保留批次, 跳过条数)`；毒行诊断经
/// `poison_log` 限流（前 16 行全量，其余折叠进重放汇总）。
/// EN: Split a replay batch into finite rows (kept) and non-finite rows (skipped),
/// returning `(kept_batch, skipped_count)`; per-row diagnostics are rate-limited through
/// `poison_log` (first 16 rows in full, the rest folded into the replay summary).
///
/// WHY（为什么重放要容忍毒行，而写入侧却 fail-fast）：写入边界现在拒绝非有限分量，
/// 但磁盘历史数据可能已含毒行——无校验版本写入、JSON 数字 `1e40` 被 f32 解成 Inf、
/// 手工损坏文件或未来的二进制编码。[`HiiL0`](houyi_vector::hii::HiiL0) 等后端的
/// `insert_batch` 现在对非有限分量返回 `Err`；若重放原样传播该错误，进程冷启动必死、
/// 无法自愈。毒行无法参与任何有意义的距离计算，「跳过 + 计数 + 限流诊断」是可用性
/// 与正确性的最佳折中。注意维度不一致仍然硬失败（那是配置级错误，跳过等于静默丢数据）。
/// EN: Writes fail fast at the boundary, but legacy on-disk data may already contain
/// NaN/±Inf rows (older builds, JSON `1e40` → f32 Inf, hand-corrupted files, future
/// binary encoders). Propagating the library error would make cold start permanently
/// abort; poisoned rows cannot contribute meaningful distances anyway, so skip + count +
/// rate-limited logging. Dim mismatch stays a hard error — config-level corruption.
fn partition_finite_batch(
    batch: Vec<(u64, Vec<f32>)>,
    source: ReplaySource,
    poison_log: &mut PoisonedRowLog,
) -> (Vec<(u64, Vec<f32>)>, usize) {
    let mut skipped = 0usize;
    let kept = batch
        .into_iter()
        .filter(|(id, vec)| {
            if vec.iter().all(|x| x.is_finite()) {
                true
            } else {
                skipped += 1;
                poison_log.record_skip(*id, source);
                false
            }
        })
        .collect();
    (kept, skipped)
}

/// 重放后尽量排空构图队列（有界循环，避免死循环）。
/// Drain builder queue after replay (bounded loop).
pub async fn drain_pending_builder(index: &IndexBackend) {
    const MAX: usize = 1_000_000;
    let mut i = 0usize;
    while index.pending_build_count().await > 0 && i < MAX {
        let _ = index.run_async_builder_once().await;
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyi_persistence::vector_snapshot::{
        encode_vector_snapshot_v1, snapshot_path_for_wal, write_vector_snapshot_atomic,
    };
    use std::path::PathBuf;

    fn unique_wal_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "houyi-recovery-{tag}-{}-{nanos}.wal",
            std::process::id()
        ))
    }

    /// 测试用加密句柄（单 key `k1`）。 / EN: test encryption handle (single key).
    fn test_encryption(byte: u8) -> std::sync::Arc<Encryption> {
        let hex = format!("{byte:02x}").repeat(houyi_persistence::encryption::KEY_LEN);
        let provider =
            houyi_persistence::encryption::LocalKeyProvider::parse(&format!("k1={hex}"), None)
                .unwrap();
        std::sync::Arc::new(Encryption::from_provider(std::sync::Arc::new(provider)).unwrap())
    }

    /// 回归（OCR HIGH）：含非有限行的历史快照不得让冷启动中止——毒行跳过、合法行恢复。
    /// JSON `1e40` 解 f32 即 +Inf，模拟旧版本/手工损坏的快照（serde_json 不视为非法数字）。
    /// EN: A legacy snapshot carrying a non-finite row (JSON `1e40` → f32 +Inf) must not
    /// abort cold start: the poisoned row is skipped and valid rows still load.
    #[tokio::test]
    async fn snapshot_replay_skips_non_finite_rows_without_abort() {
        let wal_path = unique_wal_path("snap");
        let json = br#"{
            "v": 1, "dim": 4, "metric": "cosine", "checkpoint_lsn": 9,
            "items": [
                {"id": 1, "values": [1e40, 0.0, 0.0, 0.0]},
                {"id": 2, "values": [1.0, 0.0, 0.0, 0.0]}
            ]
        }"#;
        write_vector_snapshot_atomic(&wal_path, json).unwrap();

        let index = IndexBackend::new_hii_l0(4);
        let cp = apply_vector_snapshot_if_present(&wal_path, &index, 4, DistanceMetric::Cosine)
            .await
            .expect("poisoned legacy snapshot must not abort cold start");
        assert_eq!(cp, Some(9));
        assert_eq!(
            index.indexed_count().await,
            1,
            "poisoned row skipped, good row loaded"
        );
        let hits = index
            .search_scored_with_ef_override(&[1.0, 0.0, 0.0, 0.0], 1, None, None)
            .await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 2);
        assert!(hits[0].1.abs() < 1e-6, "good row must remain searchable");

        let _ = std::fs::remove_file(snapshot_path_for_wal(&wal_path));
        let _ = std::fs::remove_file(wal_path);
    }

    /// 回归（OCR）：整条记录全为毒行时不得计入 `applied`（否则调用方的
    /// 「applied N batch(es)」日志虚报恢复工作量、掩盖整批丢失）；混合记录里的合法行
    /// 仍正常应用。当前 JSON 编码器产不出毒行（NaN→null 解码即失败），用 `1e40`
    /// 手工构造 JSON 模拟旧版本/手工损坏（serde_json 接受，解 f32 为 +Inf）。
    /// EN: A fully-poisoned record must not be counted as applied (the caller's
    /// "applied N batch(es)" log would overstate recovery and hide the total loss);
    /// finite rows of mixed records still apply. Hand-crafted JSON with `1e40`
    /// (→ f32::Inf) simulates a legacy/corrupted record the current encoder cannot write.
    #[tokio::test]
    async fn wal_replay_fully_poisoned_record_not_counted_as_applied() {
        use houyi_persistence::vector_wal::encode_vector_upsert_batch;
        use houyi_persistence::WalEntry;

        let poisoned_only = br#"{"v":1,"op":"vector_upsert_batch","dim":4,
            "items":[{"id":1,"values":[1e40,0.0,0.0,0.0]}]}"#
            .to_vec();
        // 混合记录同样手工构造：编码器会把 Inf 序列化为 null（强类型解码即失败），
        // 产不出「毒行 + 合法行」的批次；1e40 解 f32 为 +Inf。
        // The mixed record is hand-crafted too: the encoder serializes Inf to null
        // (typed decode fails), so a poison+good batch cannot be produced by the encoder;
        // JSON 1e40 decodes to f32 +Inf.
        let mixed_payload = br#"{"v":1,"op":"vector_upsert_batch","dim":4,"items":[
            {"id":3,"values":[1e40,0.0,0.0,0.0]},
            {"id":4,"values":[0.0,1.0,0.0,0.0]}]}"#
            .to_vec();
        let entries = vec![
            WalEntry {
                lsn: 1,
                payload: poisoned_only,
            },
            WalEntry {
                lsn: 2,
                payload: mixed_payload,
            },
            WalEntry {
                lsn: 3,
                payload: encode_vector_upsert_batch(4, &[(2_u64, vec![1.0, 0.0, 0.0, 0.0])])
                    .unwrap(),
            },
        ];

        let index = IndexBackend::new_hii_l0(4);
        let applied = replay_vector_wal_into_index(&entries, &index, 4, None)
            .await
            .expect("poisoned records must not abort replay");
        assert_eq!(
            applied, 2,
            "only the mixed (1 good row) and clean records apply"
        );
        assert_eq!(
            index.indexed_count().await,
            2,
            "poison ids 1/3 absent, good ids 2/4 present"
        );
        let hits = index
            .search_scored_with_ef_override(&[1.0, 0.0, 0.0, 0.0], 1, None, None)
            .await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 2);
    }

    /// 毒行过滤单元测试：NaN/+Inf/-Inf 行全部剔除并计数，合法行原样保留。
    /// EN: Non-finite rows (NaN/±Inf) are dropped and counted; finite rows survive.
    #[test]
    fn partition_finite_batch_drops_poisoned_rows() {
        let batch = vec![
            (1_u64, vec![f32::NAN, 0.0, 0.0, 0.0]),
            (2_u64, vec![f32::INFINITY, 0.0, 0.0, 0.0]),
            (3_u64, vec![f32::NEG_INFINITY, 0.0, 0.0, 0.0]),
            (4_u64, vec![1.0, 0.0, 0.0, 0.0]),
        ];
        let mut poison_log = PoisonedRowLog::new();
        let (kept, skipped) =
            partition_finite_batch(batch, ReplaySource::Snapshot, &mut poison_log);
        assert_eq!(skipped, 3);
        assert_eq!(kept, vec![(4_u64, vec![1.0, 0.0, 0.0, 0.0])]);
        // 3 行未超明细上限，无折叠。
        // Only 3 rows — below the detail cap, nothing suppressed.
        assert_eq!(poison_log.suppressed(), 0);
    }

    /// 回归（OCR）：毒行明细必须限流——前 16 行打全量明细，其余折叠计数。百万级毒行
    /// 冷启动时旧实现逐行 eprintln! 构成无界 stderr I/O（每行一次 write(2)）。
    /// EN: Per-row detail is rate-limited: the first 16 rows log in full, the rest are
    /// folded into a suppressed count; otherwise millions of poisoned rows mean unbounded
    /// stderr I/O (one write(2) per row) during cold start.
    #[test]
    fn partition_finite_batch_rate_limits_poison_row_detail() {
        let batch: Vec<(u64, Vec<f32>)> = (0..20)
            .map(|id| (id as u64, vec![f32::NAN, 0.0, 0.0, 0.0]))
            .collect();
        let mut poison_log = PoisonedRowLog::new();
        let (kept, skipped) = partition_finite_batch(batch, ReplaySource::Wal(7), &mut poison_log);
        assert_eq!(skipped, 20, "all 20 poisoned rows counted");
        assert!(kept.is_empty());
        assert_eq!(
            poison_log.suppressed(),
            20 - PoisonedRowLog::DETAILED_LIMIT,
            "rows beyond the first 16 must be suppressed from per-row detail"
        );
    }

    // ── P4.4.0 图快照恢复 ─────────────────────────────────
    // EN: P4.4.0 graph snapshot recovery.

    use houyi_persistence::graph_snapshot_file::{
        graph_snapshot_path_for_wal, read_graph_snapshot, write_graph_snapshot_atomic,
    };
    use std::collections::HashMap;

    fn vec4(seed: u64) -> Vec<f32> {
        let mut v = vec![0.0_f32; 4];
        v[(seed % 4) as usize] = 1.0;
        v[3] += seed as f32 / 1000.0;
        v
    }

    async fn build_hnsw_snapshot_payload(n: u64) -> (IndexBackend, Vec<u8>) {
        let index = IndexBackend::new_hnsw(4);
        index
            .insert_batch((1..=n).map(|id| (id, vec4(id))).collect())
            .await
            .expect("insert_batch");
        for _ in 0..1000 {
            if index.run_async_builder_once().await.flushed == 0 {
                break;
            }
        }
        let payload = index.encode_graph_snapshot().await.expect("hnsw encode");
        (index, payload)
    }

    /// 核心路径：图快照加载后，LSN ≤ checkpoint 的 WAL 记录跳过重放，
    /// 之后的记录正常应用。
    /// EN: Core path — after a graph snapshot load, WAL records with
    /// LSN ≤ checkpoint are skipped; later records apply normally.
    #[tokio::test]
    async fn graph_snapshot_loads_and_replay_skips_covered_lsn() {
        let wal_path = unique_wal_path("graph-load");
        let (_src, payload) = build_hnsw_snapshot_payload(4).await;
        write_graph_snapshot_atomic(&graph_snapshot_path_for_wal(&wal_path), 2, &payload).unwrap();

        let index = IndexBackend::new_hnsw(4);
        let cp =
            apply_graph_snapshot_if_present(&wal_path, &index, &GraphSnapshotMetrics::default())
                .await
                .expect("graph snapshot load");
        assert_eq!(cp, Some(2));
        assert_eq!(
            index.indexed_count().await,
            4,
            "graph restored from snapshot"
        );

        // lsn=1（覆盖）跳过；lsn=3（新 id 9）应用。
        // EN: lsn=1 (overwrite) skipped; lsn=3 (new id 9) applied.
        let entries = vec![
            WalEntry {
                lsn: 1,
                payload: houyi_persistence::vector_wal::encode_vector_upsert_batch(
                    4,
                    &[(1, vec4(1)), (2, vec4(2))],
                )
                .unwrap(),
            },
            WalEntry {
                lsn: 3,
                payload: houyi_persistence::vector_wal::encode_vector_upsert_batch(
                    4,
                    &[(9, vec4(9))],
                )
                .unwrap(),
            },
        ];
        let applied = replay_vector_wal_into_index(&entries, &index, 4, cp)
            .await
            .unwrap();
        assert_eq!(applied, 1, "only the post-checkpoint record applies");
        // 重放的 insert_batch 只入待构图队列，排空后才计入 indexed_count（同 main 装配顺序）。
        // EN: Replayed insert_batch only queues for graph building; indexed_count is
        // EN: updated after draining (same assembly order as main).
        drain_pending_builder(&index).await;
        assert_eq!(index.indexed_count().await, 5);

        let _ = std::fs::remove_file(graph_snapshot_path_for_wal(&wal_path));
    }

    /// P4.7.3：加密图快照——有密钥正常恢复；缺密钥必须返回 Err（启动失败），
    /// 不得伪装成「损坏→回退重放」。
    /// EN: P4.7.3: encrypted graph snapshot — restores normally with a key; missing
    /// EN: key must return Err (startup failure), never masquerade as fallback replay.
    #[tokio::test]
    async fn encrypted_graph_snapshot_requires_the_key() {
        let wal_path = unique_wal_path("graph-enc");
        let snap_path = graph_snapshot_path_for_wal(&wal_path);
        let (_src, payload) = build_hnsw_snapshot_payload(4).await;
        let enc = test_encryption(5);
        write_graph_snapshot_atomic_with_encryption(&snap_path, 2, &payload, Some(enc.as_ref()))
            .unwrap();

        let index = IndexBackend::new_hnsw(4);
        let err = apply_graph_snapshot_if_present_with_encryption(
            &wal_path,
            &index,
            &GraphSnapshotMetrics::default(),
            None,
        )
        .await
        .expect_err("missing key must be fatal");
        assert!(
            err.to_string().contains("missing encryption key"),
            "got: {err}"
        );
        assert_eq!(index.indexed_count().await, 0, "nothing restored");

        let cp = apply_graph_snapshot_if_present_with_encryption(
            &wal_path,
            &index,
            &GraphSnapshotMetrics::default(),
            Some(enc.as_ref()),
        )
        .await
        .unwrap();
        assert_eq!(cp, Some(2));
        assert_eq!(index.indexed_count().await, 4);
        let _ = std::fs::remove_file(&snap_path);
    }

    /// P4.7.3：collection 快照同理——缺密钥整体启动失败。
    /// EN: P4.7.3: same for collection snapshots — a missing key fails the whole startup.
    #[tokio::test]
    async fn encrypted_collection_snapshot_requires_the_key() {
        let wal_path = unique_wal_path("coll-enc");
        let dir = collection_snapshots_dir(&wal_path);
        std::fs::create_dir_all(&dir).unwrap();
        let (_src, payload) = build_hnsw_snapshot_payload(4).await;
        let enc = test_encryption(6);
        write_graph_snapshot_atomic_with_encryption(
            &dir.join("docs.graph_snapshot.bin"),
            2,
            &payload,
            Some(enc.as_ref()),
        )
        .unwrap();

        let manager = crate::collection::CollectionManager::new(4, "hnsw");
        let err = apply_collection_graph_snapshots_with_encryption(
            &manager,
            &wal_path,
            &GraphSnapshotMetrics::default(),
            None,
        )
        .await
        .expect_err("missing key must be fatal");
        assert!(
            err.to_string().contains("missing encryption key"),
            "got: {err}"
        );

        let cps = apply_collection_graph_snapshots_with_encryption(
            &manager,
            &wal_path,
            &GraphSnapshotMetrics::default(),
            Some(enc.as_ref()),
        )
        .await
        .unwrap();
        assert_eq!(cps.get("docs"), Some(&2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 图快照优先于旧 JSON 快照：两者并存时图快照生效，JSON 不加载。
    /// EN: Graph snapshot takes precedence over the legacy JSON snapshot.
    #[tokio::test]
    async fn graph_snapshot_takes_precedence_over_json_snapshot() {
        let wal_path = unique_wal_path("graph-precedence");
        let (_src, payload) = build_hnsw_snapshot_payload(4).await;
        write_graph_snapshot_atomic(&graph_snapshot_path_for_wal(&wal_path), 2, &payload).unwrap();
        // JSON 快照：checkpoint=9 且含一个图快照没有的 id 42——若 JSON 被误加载即可检出。
        // EN: JSON snapshot: checkpoint=9 and contains id 42 absent from the graph
        // EN: snapshot — detectable if JSON is ever loaded by mistake.
        let json = encode_vector_snapshot_v1(
            4,
            "cosine",
            9,
            vec![houyi_persistence::vector_wal::VectorItem {
                id: 42,
                values: vec4(42),
            }],
        )
        .unwrap();
        write_vector_snapshot_atomic(&wal_path, &json).unwrap();

        let index = IndexBackend::new_hnsw(4);
        let cp =
            apply_graph_snapshot_if_present(&wal_path, &index, &GraphSnapshotMetrics::default())
                .await
                .unwrap();
        assert_eq!(cp, Some(2), "graph snapshot wins");
        assert_eq!(index.indexed_count().await, 4);
        let hits = index.search(&vec4(42), 100, None).await;
        assert!(!hits.contains(&42), "JSON snapshot must not be loaded");

        let _ = std::fs::remove_file(graph_snapshot_path_for_wal(&wal_path));
        let _ = std::fs::remove_file(snapshot_path_for_wal(&wal_path));
    }

    /// 损坏的图快照：弃用并干净回退——JSON 快照路径（旧版回退）照常工作。
    /// EN: A corrupt graph snapshot is discarded; the legacy JSON fallback
    /// path still works.
    #[tokio::test]
    async fn corrupt_graph_snapshot_falls_back_to_json_cleanly() {
        let wal_path = unique_wal_path("graph-corrupt");
        std::fs::write(graph_snapshot_path_for_wal(&wal_path), b"garbage").unwrap();
        let json = encode_vector_snapshot_v1(
            4,
            "cosine",
            9,
            vec![houyi_persistence::vector_wal::VectorItem {
                id: 42,
                values: vec4(42),
            }],
        )
        .unwrap();
        write_vector_snapshot_atomic(&wal_path, &json).unwrap();

        let index = IndexBackend::new_hnsw(4);
        let cp =
            apply_graph_snapshot_if_present(&wal_path, &index, &GraphSnapshotMetrics::default())
                .await
                .unwrap();
        assert_eq!(cp, None, "corrupt graph snapshot discarded");
        let cp = apply_vector_snapshot_if_present(&wal_path, &index, 4, DistanceMetric::Cosine)
            .await
            .unwrap();
        assert_eq!(cp, Some(9), "JSON fallback still loads");
        assert_eq!(index.indexed_count().await, 1);

        let _ = std::fs::remove_file(graph_snapshot_path_for_wal(&wal_path));
        let _ = std::fs::remove_file(snapshot_path_for_wal(&wal_path));
    }

    /// 非 Hnsw 后端：图快照存在也不适用，返回 None（调用方走 JSON/全量重放）。
    /// EN: Non-Hnsw backends ignore graph snapshots.
    #[tokio::test]
    async fn graph_snapshot_ignored_for_non_hnsw_backend() {
        let wal_path = unique_wal_path("graph-nonhnsw");
        let (_src, payload) = build_hnsw_snapshot_payload(4).await;
        write_graph_snapshot_atomic(&graph_snapshot_path_for_wal(&wal_path), 2, &payload).unwrap();

        let index = IndexBackend::new_flat(4);
        let cp =
            apply_graph_snapshot_if_present(&wal_path, &index, &GraphSnapshotMetrics::default())
                .await
                .unwrap();
        assert_eq!(cp, None);

        let _ = std::fs::remove_file(graph_snapshot_path_for_wal(&wal_path));
    }

    /// Collection 图快照 + LSN 分界重放（P4.4.0 v1 关键简化）：
    /// LSN ≤ checkpoint 的记录跳过后端向量插入/删除，但标量行与标量墓碑
    /// 仍全量重建（标量 PAX 页纯内存，O(N) 便宜，省掉标量序列化）。
    /// EN: Per-collection graph snapshot + LSN-boundary replay: covered
    /// records skip vector insert/delete but still rebuild scalar rows and
    /// scalar tombstones.
    #[tokio::test]
    async fn collections_graph_snapshot_skips_vectors_but_rebuilds_scalars() {
        let dir = std::env::temp_dir().join(format!(
            "houyi-colsnap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let snaps_dir = dir.join("snapshots");
        std::fs::create_dir_all(&snaps_dir).unwrap();
        let collections_wal_path = dir.join("collections.wal");

        // 源 collection：id 1/2 入图（这就是快照覆盖的内容，checkpoint=2）。
        // EN: Source collection: ids 1/2 enter the graph (the snapshot covers this, checkpoint=2).
        let src_manager = crate::collection::CollectionManager::new(4, "hnsw");
        let src_col = src_manager.get_or_create("docs", Some(4)).unwrap();
        src_col
            .backend
            .insert_batch(vec![(1, vec4(1)), (2, vec4(2))])
            .await
            .unwrap();
        for _ in 0..1000 {
            if src_col.backend.run_async_builder_once().await.flushed == 0 {
                break;
            }
        }
        let payload = src_col.backend.encode_graph_snapshot().await.unwrap();
        write_graph_snapshot_atomic(&snaps_dir.join("docs.graph_snapshot.bin"), 2, &payload)
            .unwrap();

        // 目标 manager：快照恢复（collections.wal 重放之前）。
        // EN: Target manager: snapshot restore (before collections.wal replay).
        let manager = crate::collection::CollectionManager::new(4, "hnsw");
        let checkpoints = apply_collection_graph_snapshots_with_encryption(
            &manager,
            &collections_wal_path,
            &GraphSnapshotMetrics::default(),
            None,
        )
        .await
        .expect("plaintext snapshot restore");
        assert_eq!(checkpoints.get("docs"), Some(&2));
        {
            let col = manager.get("docs").expect("restored from snapshot");
            assert_eq!(
                col.backend.indexed_count().await,
                2,
                "graph restored without WAL replay"
            );
        }

        // WAL：create@1（幂等）、upsert@2（覆盖：跳向量、建标量行）、
        // upsert@3（新 id 3：全应用）、delete@4（id 1：全应用）。
        // EN: WAL: create@1 (idempotent), upsert@2 (overwrite: skip vector, build scalar
        // EN: row), upsert@3 (new id 3: fully applied), delete@4 (id 1: fully applied).
        let entries = vec![
            WalEntry {
                lsn: 1,
                payload: encode_collection_create("docs", 4, "hnsw").unwrap(),
            },
            WalEntry {
                lsn: 2,
                payload: encode_collection_upsert_batch(
                    "docs",
                    4,
                    &[
                        wal_item(1, vec4(1), Some("doc-a"), None),
                        wal_item(2, vec4(2), Some("doc-b"), None),
                    ],
                )
                .unwrap(),
            },
            WalEntry {
                lsn: 3,
                payload: encode_collection_upsert_batch(
                    "docs",
                    4,
                    &[wal_item(3, vec4(3), Some("doc-c"), None)],
                )
                .unwrap(),
            },
            WalEntry {
                lsn: 4,
                payload: encode_collection_delete("docs", &[1]).unwrap(),
            },
        ];
        let stats = replay_collections_wal_into_manager(&entries, &manager, &checkpoints)
            .await
            .unwrap();
        assert_eq!(stats.creates, 0, "collection already created by snapshot");
        assert_eq!(
            stats.upsert_batches, 1,
            "only post-checkpoint upsert applies"
        );
        assert_eq!(
            stats.upsert_batches_skipped_snapshot, 1,
            "covered upsert skipped on the vector side"
        );
        assert_eq!(stats.scalar_rows, 3, "scalar rows rebuilt for ALL upserts");
        assert_eq!(stats.delete_batches, 1);
        assert_eq!(stats.tombstones, 1);

        let col = manager.get("docs").unwrap();
        // id 1：快照恢复后被 delete@4 删除；id 2（快照）与 id 3（重放）可检索。
        // EN: id 1: deleted by delete@4 after restore; id 2 (snapshot) and id 3 (replay)
        // EN: remain searchable.
        let hits = col.backend.search(&vec4(2), 10, None).await;
        assert!(hits.contains(&2), "snapshot id searchable: {hits:?}");
        assert!(!hits.contains(&1), "deleted id absent: {hits:?}");
        let hits = col.backend.search(&vec4(3), 10, None).await;
        assert!(hits.contains(&3), "replayed id searchable: {hits:?}");
        // 标量行：三个 id 都有行；id 1 被墓碑隐藏。
        // EN: Scalar rows: all three ids have rows; id 1 is hidden by the tombstone.
        let payloads = col.scalars.lock().unwrap().payloads_for(&[1, 2, 3]);
        assert!(!payloads.contains_key(&1), "tombstoned payload hidden");
        assert!(payloads.contains_key(&2));
        assert!(payloads.contains_key(&3));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 覆盖的 delete（LSN ≤ checkpoint）：墓碑已在快照内，跳过后端 delete，
    /// 但标量墓碑仍重建。
    /// EN: A covered delete skips the backend delete (the snapshot already
    /// carries the tombstone) but still rebuilds the scalar tombstone.
    #[tokio::test]
    async fn collections_covered_delete_skips_backend_but_rebuilds_scalar_tombstone() {
        let dir = std::env::temp_dir().join(format!(
            "houyi-colsnap-del-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let snaps_dir = dir.join("snapshots");
        std::fs::create_dir_all(&snaps_dir).unwrap();
        let collections_wal_path = dir.join("collections.wal");

        // 源：id 1/2 入图后删除 id 1（墓碑随快照），checkpoint=3。
        // EN: Source: ids 1/2 enter the graph then id 1 is deleted (tombstone in
        // EN: snapshot), checkpoint=3.
        let src_manager = crate::collection::CollectionManager::new(4, "hnsw");
        let src_col = src_manager.get_or_create("docs", Some(4)).unwrap();
        src_col
            .backend
            .insert_batch(vec![(1, vec4(1)), (2, vec4(2))])
            .await
            .unwrap();
        for _ in 0..1000 {
            if src_col.backend.run_async_builder_once().await.flushed == 0 {
                break;
            }
        }
        assert!(src_col.backend.delete(1).await.unwrap());
        let payload = src_col.backend.encode_graph_snapshot().await.unwrap();
        write_graph_snapshot_atomic(&snaps_dir.join("docs.graph_snapshot.bin"), 3, &payload)
            .unwrap();

        let manager = crate::collection::CollectionManager::new(4, "hnsw");
        let checkpoints = apply_collection_graph_snapshots_with_encryption(
            &manager,
            &collections_wal_path,
            &GraphSnapshotMetrics::default(),
            None,
        )
        .await
        .expect("plaintext snapshot restore");
        assert_eq!(checkpoints.get("docs"), Some(&3));

        // WAL：upsert@2（覆盖）、delete@3（覆盖：跳后端、建标量墓碑）。
        // EN: WAL: upsert@2 (overwrite), delete@3 (overwrite: skip backend, build scalar
        // EN: tombstone).
        let entries = vec![
            WalEntry {
                lsn: 2,
                payload: encode_collection_upsert_batch(
                    "docs",
                    4,
                    &[
                        wal_item(1, vec4(1), Some("doc-a"), None),
                        wal_item(2, vec4(2), Some("doc-b"), None),
                    ],
                )
                .unwrap(),
            },
            WalEntry {
                lsn: 3,
                payload: encode_collection_delete("docs", &[1]).unwrap(),
            },
        ];
        let stats = replay_collections_wal_into_manager(&entries, &manager, &checkpoints)
            .await
            .unwrap();
        assert_eq!(stats.delete_batches, 0, "covered delete skips backend");
        assert_eq!(stats.delete_batches_skipped_snapshot, 1);
        assert_eq!(stats.tombstones, 1, "scalar tombstone still rebuilt");

        let col = manager.get("docs").unwrap();
        // 快照内墓碑保留：id 1 不可检索，且后端 delete 未被重放语义复活。
        // EN: Snapshot tombstones persist: id 1 is unsearchable, and the backend delete
        // EN: is not revived by replay semantics.
        let hits = col.backend.search(&vec4(1), 10, None).await;
        assert!(!hits.contains(&1), "snapshot tombstone holds: {hits:?}");
        assert!(hits.contains(&2));
        let payloads = col.scalars.lock().unwrap().payloads_for(&[1, 2]);
        assert!(!payloads.contains_key(&1));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 损坏的 collection 快照：该 collection 弃用快照全量重放；其他 collection
    /// 的快照不受影响。非 Hnsw 后端（flat）同样安全跳过。
    /// EN: A corrupt collection snapshot falls back to full replay for that
    /// collection only; other snapshots are unaffected. Non-Hnsw backends
    /// skip snapshots safely too.
    #[tokio::test]
    async fn collections_corrupt_snapshot_falls_back_per_collection() {
        let dir = std::env::temp_dir().join(format!(
            "houyi-colsnap-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let snaps_dir = dir.join("snapshots");
        std::fs::create_dir_all(&snaps_dir).unwrap();
        let collections_wal_path = dir.join("collections.wal");

        // docs：损坏快照；imgs：正常快照（id 7 入图，checkpoint=5）。
        // EN: docs: corrupt snapshot; imgs: valid snapshot (id 7 enters the graph, checkpoint=5).
        std::fs::write(snaps_dir.join("docs.graph_snapshot.bin"), b"garbage").unwrap();
        let src_manager = crate::collection::CollectionManager::new(4, "hnsw");
        let src_col = src_manager.get_or_create("imgs", Some(4)).unwrap();
        src_col
            .backend
            .insert_batch(vec![(7, vec4(7))])
            .await
            .unwrap();
        for _ in 0..1000 {
            if src_col.backend.run_async_builder_once().await.flushed == 0 {
                break;
            }
        }
        let payload = src_col.backend.encode_graph_snapshot().await.unwrap();
        write_graph_snapshot_atomic(&snaps_dir.join("imgs.graph_snapshot.bin"), 5, &payload)
            .unwrap();

        let manager = crate::collection::CollectionManager::new(4, "hnsw");
        let checkpoints = apply_collection_graph_snapshots_with_encryption(
            &manager,
            &collections_wal_path,
            &GraphSnapshotMetrics::default(),
            None,
        )
        .await
        .expect("plaintext snapshot restore");
        assert_eq!(checkpoints.get("imgs"), Some(&5));
        assert!(
            !checkpoints.contains_key("docs"),
            "corrupt snapshot discarded"
        );

        // docs 全量重放（无 checkpoint）：upsert@1 全应用。
        // EN: docs replay fully (no checkpoint): upsert@1 fully applied.
        let entries = vec![WalEntry {
            lsn: 1,
            payload: encode_collection_upsert_batch(
                "docs",
                4,
                &[wal_item(1, vec4(1), Some("doc-a"), None)],
            )
            .unwrap(),
        }];
        let stats = replay_collections_wal_into_manager(&entries, &manager, &checkpoints)
            .await
            .unwrap();
        assert_eq!(
            stats.upsert_batches, 1,
            "full replay for corrupt-snapshot collection"
        );
        let col = manager.get("docs").unwrap();
        let hits = col.backend.search(&vec4(1), 10, None).await;
        assert!(hits.contains(&1));

        // flat 后端 manager：快照恢复安全跳过（不 panic、无 checkpoint）。
        // EN: flat-backend manager: snapshot restore skips safely (no panic, no checkpoint).
        let flat_manager = crate::collection::CollectionManager::new(4, "flat");
        let flat_checkpoints = apply_collection_graph_snapshots_with_encryption(
            &flat_manager,
            &collections_wal_path,
            &GraphSnapshotMetrics::default(),
            None,
        )
        .await
        .expect("plaintext snapshot restore");
        assert!(
            flat_checkpoints.is_empty(),
            "non-hnsw backend skips snapshots"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── P4.4.0e 快照写出器 ─────────────────────────────────
    // EN: P4.4.0e snapshot writer.

    fn unique_snap_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "houyi-graphsnap-writer-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 核心路径：write_all 同时写主索引（checkpoint=applied sidecar）与
    /// collection（checkpoint=collections.wal next_lsn-1）快照，指标计数
    /// 同步推进；写出的文件可被恢复侧原样加载（主索引 checkpoint 穿透）。
    /// EN: Core path — write_all snapshots the main index (checkpoint from
    /// the applied sidecar) and every collection (checkpoint from
    /// collections.wal), bumps metrics, and the files load back cleanly.
    #[tokio::test]
    async fn graph_snapshot_writer_writes_main_and_collections() {
        let dir = unique_snap_dir("main-and-collections");
        let wal_path = dir.join("bench.wal");
        let applied_sidecar = houyi_persistence::wal_sidecar::sidecar_path_for_wal(&wal_path);
        houyi_persistence::wal_sidecar::write_applied_lsn_atomic(&applied_sidecar, 7).unwrap();
        let collections_wal_path = dir.join("collections.wal");
        let (collections_wal, _entries) =
            crate::types::WalHandle::open(&collections_wal_path, crate::types::WalWriteMode::Sync)
                .unwrap();

        let index = IndexBackend::new_hnsw(4);
        index
            .insert_batch(vec![(1, vec4(1)), (2, vec4(2))])
            .await
            .unwrap();
        let manager = crate::collection::CollectionManager::new(4, "hnsw");
        let col = manager.get_or_create("docs", Some(4)).unwrap();
        col.backend
            .insert_batch(vec![(10, vec4(10)), (11, vec4(11))])
            .await
            .unwrap();

        let metrics = std::sync::Arc::new(GraphSnapshotMetrics::default());
        let writer = GraphSnapshotWriter {
            index: index.clone(),
            wal_path: wal_path.clone(),
            applied_sidecar: applied_sidecar.clone(),
            collections: std::sync::Arc::new(manager),
            collections_wal: std::sync::Arc::new(collections_wal),
            collections_wal_path: collections_wal_path.clone(),
            metrics: std::sync::Arc::clone(&metrics),
            encryption: None,
        };
        writer.write_all().await;

        assert_eq!(
            metrics
                .writes_total
                .load(std::sync::atomic::Ordering::Relaxed),
            2,
            "main index + one collection"
        );
        assert!(
            metrics
                .bytes_written_total
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
        );

        // 主索引快照：checkpoint 来自 sidecar（7），可被恢复侧加载并穿透。
        // EN: Main-index snapshot: checkpoint comes from the sidecar (7); the restore side
        // EN: can load it.
        let snap = read_graph_snapshot(&graph_snapshot_path_for_wal(&wal_path))
            .unwrap()
            .expect("main snapshot written");
        assert_eq!(snap.checkpoint_lsn, 7, "checkpoint = applied sidecar");
        let fresh = IndexBackend::new_hnsw(4);
        let load_metrics = GraphSnapshotMetrics::default();
        let cp = apply_graph_snapshot_if_present(&wal_path, &fresh, &load_metrics)
            .await
            .unwrap();
        assert_eq!(cp, Some(7));
        assert_eq!(fresh.indexed_count().await, 2);
        assert_eq!(
            load_metrics
                .loads_total
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        // collection 快照：collections.wal 无写入 → next_lsn-1 = 0。
        // EN: Collection snapshot: no collections.wal writes -> next_lsn-1 = 0.
        let col_snap = read_graph_snapshot(
            &collection_snapshots_dir(&collections_wal_path).join("docs.graph_snapshot.bin"),
        )
        .unwrap()
        .expect("collection snapshot written");
        assert_eq!(col_snap.checkpoint_lsn, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非 HNSW 后端：主索引与 collection 均安全跳过（无文件、无计数）。
    /// EN: Non-HNSW backends are skipped cleanly — no files, no counters.
    #[tokio::test]
    async fn graph_snapshot_writer_skips_non_hnsw_backends() {
        let dir = unique_snap_dir("non-hnsw");
        let wal_path = dir.join("bench.wal");
        let applied_sidecar = houyi_persistence::wal_sidecar::sidecar_path_for_wal(&wal_path);
        houyi_persistence::wal_sidecar::write_applied_lsn_atomic(&applied_sidecar, 3).unwrap();
        let collections_wal_path = dir.join("collections.wal");
        let (collections_wal, _entries) =
            crate::types::WalHandle::open(&collections_wal_path, crate::types::WalWriteMode::Sync)
                .unwrap();

        let index = IndexBackend::new_flat(4);
        index.insert_batch(vec![(1, vec4(1))]).await.unwrap();
        let manager = crate::collection::CollectionManager::new(4, "flat");
        let col = manager.get_or_create("docs", Some(4)).unwrap();
        col.backend.insert_batch(vec![(1, vec4(1))]).await.unwrap();

        let metrics = std::sync::Arc::new(GraphSnapshotMetrics::default());
        let writer = GraphSnapshotWriter {
            index,
            wal_path: wal_path.clone(),
            applied_sidecar,
            collections: std::sync::Arc::new(manager),
            collections_wal: std::sync::Arc::new(collections_wal),
            collections_wal_path: collections_wal_path.clone(),
            metrics: std::sync::Arc::clone(&metrics),
            encryption: None,
        };
        writer.write_all().await;

        assert_eq!(
            metrics
                .writes_total
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(graph_snapshot_path_for_wal(&wal_path).metadata().is_err());
        assert!(!collection_snapshots_dir(&collections_wal_path)
            .join("docs.graph_snapshot.bin")
            .exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// sidecar 真实读故障：跳主索引快照但 collection 照常写（单点失败不扩散）。
    /// EN: A genuine sidecar read failure skips only the main-index snapshot;
    /// collections still get theirs.
    #[tokio::test]
    async fn graph_snapshot_writer_sidecar_failure_skips_only_main() {
        let dir = unique_snap_dir("sidecar-failure");
        let wal_path = dir.join("bench.wal");
        // 侧车路径指向一个「目录」（read 必失败），制造真实读故障。
        // EN: The sidecar path points at a directory (read must fail), creating a real
        // EN: read fault.
        let applied_sidecar = dir.join("sidecar-as-dir");
        std::fs::create_dir_all(&applied_sidecar).unwrap();
        let collections_wal_path = dir.join("collections.wal");
        let (collections_wal, _entries) =
            crate::types::WalHandle::open(&collections_wal_path, crate::types::WalWriteMode::Sync)
                .unwrap();

        let index = IndexBackend::new_hnsw(4);
        index.insert_batch(vec![(1, vec4(1))]).await.unwrap();
        let manager = crate::collection::CollectionManager::new(4, "hnsw");
        let col = manager.get_or_create("docs", Some(4)).unwrap();
        col.backend.insert_batch(vec![(1, vec4(1))]).await.unwrap();

        let metrics = std::sync::Arc::new(GraphSnapshotMetrics::default());
        let writer = GraphSnapshotWriter {
            index,
            wal_path: wal_path.clone(),
            applied_sidecar,
            collections: std::sync::Arc::new(manager),
            collections_wal: std::sync::Arc::new(collections_wal),
            collections_wal_path: collections_wal_path.clone(),
            metrics: std::sync::Arc::clone(&metrics),
            encryption: None,
        };
        writer.write_all().await;

        assert!(graph_snapshot_path_for_wal(&wal_path).metadata().is_err());
        assert!(
            collection_snapshots_dir(&collections_wal_path)
                .join("docs.graph_snapshot.bin")
                .exists(),
            "collection snapshot survives main-index failure"
        );
        assert_eq!(
            metrics
                .writes_total
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── collections WAL 重放 ─────────────────────────────────
    // EN: collections WAL replay.

    use houyi_persistence::collection_wal::{
        encode_collection_create, encode_collection_delete, encode_collection_upsert_batch,
        CollectionWalItem,
    };
    use houyi_persistence::{FileWal, Wal};

    fn wal_item(
        id: u64,
        values: Vec<f32>,
        label: Option<&str>,
        payload: Option<serde_json::Value>,
    ) -> CollectionWalItem {
        CollectionWalItem {
            id,
            values,
            label: label.map(str::to_string),
            payload: payload.and_then(|v| v.as_object().cloned()),
        }
    }

    /// 核心回归：collections WAL 重启恢复——向量、标量行（label+payload）、墓碑全部
    /// 按写入语义重建。这是「数据库宕机重启不丢 collection 数据」的兜底测试。
    /// EN: Core regression — a cold restart replays the collections WAL and rebuilds
    /// vectors, scalar rows (label + payload), and tombstones with write-path
    /// semantics. This is the backstop for "collections survive a crash".
    #[tokio::test]
    async fn collections_wal_replay_restores_vectors_scalars_and_tombstones() {
        let path = unique_wal_path("collections-roundtrip");
        let _ = std::fs::remove_file(&path);
        {
            let (mut wal, _) = FileWal::open(&path).unwrap();
            wal.append(encode_collection_create("docs", 4, "flat").unwrap());
            let items = vec![
                wal_item(
                    1,
                    vec![1.0, 0.0, 0.0, 0.0],
                    Some("doc-a"),
                    Some(serde_json::json!({"tag": "p0", "score": 0.5})),
                ),
                wal_item(2, vec![0.0, 1.0, 0.0, 0.0], None, None),
            ];
            wal.append(encode_collection_upsert_batch("docs", 4, &items).unwrap());
            wal.append(encode_collection_delete("docs", &[2]).unwrap());
        }
        let (_wal, entries) = FileWal::open(&path).unwrap();
        let manager = crate::collection::CollectionManager::new(4, "flat");
        let stats = replay_collections_wal_into_manager(&entries, &manager, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(stats.creates, 1);
        assert_eq!(stats.upsert_batches, 1);
        assert_eq!(stats.upsert_rows, 2);
        assert_eq!(stats.scalar_rows, 1, "bare vector must not build a row");
        assert_eq!(stats.delete_batches, 1);
        assert_eq!(stats.tombstones, 1);

        let col = manager.get("docs").expect("collection restored");
        assert_eq!(col.dims, 4);
        // 向量：id 1 可检索，id 2 已删。
        // Vectors: id 1 searchable, id 2 deleted.
        let hits = col.backend.search(&[1.0, 0.0, 0.0, 0.0], 4, None).await;
        assert!(hits.contains(&1), "id 1 must be searchable: {hits:?}");
        assert!(!hits.contains(&2), "id 2 must stay deleted: {hits:?}");
        // 标量行：label + payload 原样回读；墓碑 id 缺席。
        // Scalar rows: label + payload read back; tombstoned id absent.
        let payloads = col.scalars.lock().unwrap().payloads_for(&[1, 2]);
        let row = payloads.get(&1).expect("payload for id 1");
        assert_eq!(
            row.get("label"),
            Some(&houyi_core::ScalarValue::Text("doc-a".to_string()))
        );
        assert_eq!(
            row.get("tag"),
            Some(&houyi_core::ScalarValue::Text("p0".to_string()))
        );
        assert!(!payloads.contains_key(&2), "tombstoned id must be absent");
        // 过滤：tag=p0 命中 id 1（混合检索的结构化过滤在重启后依然可用）。
        // Filters: tag=p0 matches id 1 — structured filtering works after restart.
        let matched = col.scalars.lock().unwrap().matching_ids(
            "tag",
            &houyi_core::ScalarPredicate::Eq(houyi_core::ScalarValue::Text("p0".to_string())),
        );
        assert_eq!(matched, vec![1]);
        let _ = std::fs::remove_file(&path);
    }

    /// 韧性：缺 create 记录的 upsert 自动补建 collection；指向未知 collection 的
    /// delete（WAL 被截断的历史）跳过而非中止冷启动。
    /// EN: Resilience — an upsert without a preceding create auto-creates the
    /// collection; a delete for an unknown collection (truncated history) is
    /// skipped, never fatal to cold start.
    #[tokio::test]
    async fn collections_wal_replay_tolerates_missing_create_and_unknown_delete() {
        let path = unique_wal_path("collections-resilience");
        let _ = std::fs::remove_file(&path);
        {
            let (mut wal, _) = FileWal::open(&path).unwrap();
            // 无 create，直接 upsert（手工截断/旧版写入的历史）。
            // EN: No create, direct upsert (history from manual truncation/older writers).
            let items = vec![wal_item(7, vec![1.0, 0.0, 0.0, 0.0], None, None)];
            wal.append(encode_collection_upsert_batch("orphan", 4, &items).unwrap());
            // 未知 collection 的 delete。
            // EN: Delete of an unknown collection.
            wal.append(encode_collection_delete("ghost", &[1, 2, 3]).unwrap());
            // 混入主 WAL 的记录也不得炸掉重放（防御：文件被运维错放）。
            // EN: Records mixed into the main WAL must not break replay (defensive:
            // EN: operator misfiled the file).
            wal.append(
                houyi_persistence::vector_wal::encode_vector_upsert_batch(4, &[(1, vec![0.0; 4])])
                    .unwrap(),
            );
        }
        let (_wal, entries) = FileWal::open(&path).unwrap();
        let manager = crate::collection::CollectionManager::new(4, "flat");
        let stats = replay_collections_wal_into_manager(&entries, &manager, &HashMap::new())
            .await
            .expect("incomplete history must not abort replay");
        assert_eq!(stats.creates, 0, "no explicit create records");
        assert_eq!(stats.upsert_batches, 1);
        assert!(manager.get("orphan").is_some(), "upsert auto-created");
        assert!(manager.get("ghost").is_none(), "unknown delete skipped");
        let _ = std::fs::remove_file(&path);
    }

    /// last-wins：同 id 先 upsert 后 delete 再 upsert，重放按 LSN 顺序收敛到最终态
    /// （向量在、墓碑被复活语义清除、标量行取最新）。
    /// EN: Last-wins — upsert → delete → upsert of the same id converges in LSN
    /// order (vector present, tombstone cleared by resurrection, latest row wins).
    #[tokio::test]
    async fn collections_wal_replay_last_wins_across_ops() {
        let path = unique_wal_path("collections-lastwins");
        let _ = std::fs::remove_file(&path);
        {
            let (mut wal, _) = FileWal::open(&path).unwrap();
            let v1 = vec![wal_item(
                1,
                vec![1.0, 0.0, 0.0, 0.0],
                Some("old"),
                Some(serde_json::json!({"tag": "p0"})),
            )];
            wal.append(encode_collection_upsert_batch("docs", 4, &v1).unwrap());
            wal.append(encode_collection_delete("docs", &[1]).unwrap());
            let v2 = vec![wal_item(
                1,
                vec![1.0, 0.0, 0.0, 0.0],
                Some("new"),
                Some(serde_json::json!({"tag": "p1"})),
            )];
            wal.append(encode_collection_upsert_batch("docs", 4, &v2).unwrap());
        }
        let (_wal, entries) = FileWal::open(&path).unwrap();
        let manager = crate::collection::CollectionManager::new(4, "flat");
        replay_collections_wal_into_manager(&entries, &manager, &HashMap::new())
            .await
            .unwrap();
        let col = manager.get("docs").unwrap();
        let hits = col.backend.search(&[1.0, 0.0, 0.0, 0.0], 1, None).await;
        assert_eq!(hits, vec![1], "resurrected id must be searchable");
        let payloads = col.scalars.lock().unwrap().payloads_for(&[1]);
        let row = payloads.get(&1).expect("resurrected payload");
        assert_eq!(
            row.get("label"),
            Some(&houyi_core::ScalarValue::Text("new".to_string())),
            "latest row wins"
        );
        assert_eq!(
            row.get("tag"),
            Some(&houyi_core::ScalarValue::Text("p1".to_string()))
        );
        let _ = std::fs::remove_file(&path);
    }
}
