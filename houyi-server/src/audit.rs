//! 不可篡改审计日志（P4.7.0）。
//! Tamper-evident audit log (P4.7.0).
//!
//! 设计 / Design:
//! - **哈希链**：每条记录 `hash = SHA-256(seq‖ts‖actor‖action‖resource‖
//!   outcome‖prev_hash)`，`prev_hash` 指向上一条的 `hash`（首条为 64 个 0）。
//!   任何一条被改写，其后所有记录校验失败——篡改可被发现，而不是靠文件权限
//!   自证。EN: each record commits to the previous hash; any rewrite breaks
//!   the chain from that point on.
//! - **只追加**：JSONL，`O_APPEND` 打开，每条写完 `fsync`（审计事件频率 =
//!   写/管理/认证失败，不在检索热路径）。EN: append-only JSONL, fsync per
//!   record; audit events are writes/admin/auth failures, never the read hot path.
//! - **启动即校验**：开启审计时先验证既有链（含轮转段），链断则拒绝启动
//!   （fail-closed）——带着被篡改的审计历史运行比停机更危险。
//!   EN: verify the existing chain (including rotated segments) on startup and
//!   refuse to start when it is broken.
//! - **写失败关闭**：写审计失败时，成功的 HTTP 响应被改写为 500（写可能已
//!   发生但无审计证据，财务/合规场景必须显式失败而不是静默通过）。
//!   EN: if the audit write fails, a successful response is replaced by 500 —
//!   the write may have happened without evidence, which must not look like success.
//! - **不记录原始凭据**：actor 是 token 的 SHA-256 指纹前 16 位（`token:…`）
//!   或 `anonymous`，原始 Bearer/X-Api-Key 永不落盘。
//!   EN: actors are token fingerprints, never raw credentials.
//!
//! 边界 / Limits: v1 记录 HTTP 面的写/管理/认证失败；复制会话内部事件与
//! pgwire 审计在后续批次接入。轮转段只增不删（留存/法律保留策略未实现）。
//! EN: v1 covers the HTTP surface; replication-session/pgwire events and
//! retention policy are follow-ups.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 链首哨兵（64 个 0）。
/// EN: Genesis sentinel (64 zeros).
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// 审计记录（落盘形态，含 hash）。字段顺序参与哈希，不得重排。
/// On-disk record; field order is part of the hash contract.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditRecord {
    seq: u64,
    ts_unix_nanos: u64,
    actor: String,
    action: String,
    resource: String,
    outcome: String,
    prev_hash: String,
    hash: String,
}

/// 哈希覆盖的主体（不含 hash 自身）。
/// EN: Hash coverage (self-hash excluded).
#[derive(Debug, Serialize)]
struct AuditBody<'a> {
    seq: u64,
    ts_unix_nanos: u64,
    actor: &'a str,
    action: &'a str,
    resource: &'a str,
    outcome: &'a str,
    prev_hash: &'a str,
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn hash_body(body: &AuditBody<'_>) -> io::Result<String> {
    let bytes = serde_json::to_vec(body).map_err(io::Error::other)?;
    Ok(sha256_hex(&bytes))
}

fn record_hash(rec: &AuditRecord) -> io::Result<String> {
    hash_body(&AuditBody {
        seq: rec.seq,
        ts_unix_nanos: rec.ts_unix_nanos,
        actor: &rec.actor,
        action: &rec.action,
        resource: &rec.resource,
        outcome: &rec.outcome,
        prev_hash: &rec.prev_hash,
    })
}

#[derive(Debug)]
struct AuditState {
    file: File,
    seq: u64,
    last_hash: String,
    bytes: u64,
    /// 首次追加失败后锁存：失败可能已在文件里留下半行或「完整但未 fsync」
    /// 的记录，继续追加会让整条链永久不可校验，而调用方仍看到成功。
    /// 中毒后所有 record 立即失败（HTTP 中间件据此返回 500），直到运维介入。
    /// EN: latch after the first append failure — a partial/unfsynced line makes
    /// the chain unverifiable; later appends must fail fast (fail-closed).
    poisoned: Option<String>,
}

#[derive(Debug, Default)]
struct AuditCounters {
    entries: AtomicU64,
    write_errors: AtomicU64,
}

/// 审计日志句柄（进程内单例，随 HTTP state 共享）。
/// Audit log handle shared through the HTTP state.
#[derive(Debug)]
pub struct AuditLog {
    path: PathBuf,
    include_reads: bool,
    max_bytes: u64,
    state: Arc<Mutex<AuditState>>,
    counters: Arc<AuditCounters>,
}

/// 校验报告（`/v1/admin/audit/verify` 响应）。
/// Verification report returned by the admin endpoint.
#[derive(Debug, Serialize)]
pub struct VerifyReport {
    pub entries: u64,
    pub valid: bool,
    pub first_bad_seq: Option<u64>,
    pub segments: usize,
    pub last_hash: String,
}

impl AuditLog {
    /// 打开（或创建）审计日志，并在追加前验证既有链。
    /// Open/create the log and verify the existing chain before appending.
    pub fn open(path: PathBuf, include_reads: bool, max_bytes: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        // 启动校验：链断即拒绝启动（fail-closed）。
        // EN: Startup verification: a broken chain refuses startup (fail-closed).
        let (seq, last_hash, bytes) = if path.exists() {
            let report = verify_files(&path)?;
            if !report.valid {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "audit log chain is broken at seq {:?} ({}); refusing to start \
                         (inspect or restore the log, or move it aside deliberately)",
                        report.first_bad_seq,
                        path.display()
                    ),
                ));
            }
            (
                report.last_seq.map(|s| s + 1).unwrap_or(0),
                report.last_hash,
                path.metadata().map(|m| m.len()).unwrap_or(0),
            )
        } else {
            (0, GENESIS_HASH.to_string(), 0)
        };
        let file = open_append(&path)?;
        Ok(Self {
            path,
            include_reads,
            max_bytes,
            state: Arc::new(Mutex::new(AuditState {
                file,
                seq,
                last_hash,
                bytes,
                poisoned: None,
            })),
            counters: Arc::new(AuditCounters::default()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries_total(&self) -> u64 {
        self.counters.entries.load(Ordering::Relaxed)
    }

    pub fn write_errors_total(&self) -> u64 {
        self.counters.write_errors.load(Ordering::Relaxed)
    }

    /// 记录一条审计事件（阻塞 IO 在 blocking 池执行）。
    /// Append one audit event; file IO runs on the blocking pool.
    pub async fn record(
        &self,
        actor: &str,
        action: &str,
        resource: &str,
        outcome: &str,
    ) -> io::Result<()> {
        let state = Arc::clone(&self.state);
        let counters = Arc::clone(&self.counters);
        let path = self.path.clone();
        let max_bytes = self.max_bytes;
        let (actor, action, resource, outcome) = (
            actor.to_string(),
            action.to_string(),
            resource.to_string(),
            outcome.to_string(),
        );
        let result = tokio::task::spawn_blocking(move || {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(reason) = &st.poisoned {
                counters.write_errors.fetch_add(1, Ordering::Relaxed);
                return Err(io::Error::other(format!(
                    "audit log is poisoned by an earlier append failure ({reason}); \
                     fix or rotate the log and restart"
                )));
            }
            let res = (|| -> io::Result<()> {
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                let seq = st.seq;
                let prev_hash = st.last_hash.clone();
                let body = AuditBody {
                    seq,
                    ts_unix_nanos: ts,
                    actor: &actor,
                    action: &action,
                    resource: &resource,
                    outcome: &outcome,
                    prev_hash: &prev_hash,
                };
                let hash = hash_body(&body)?;
                let rec = AuditRecord {
                    seq,
                    ts_unix_nanos: ts,
                    actor,
                    action,
                    resource,
                    outcome,
                    prev_hash,
                    hash: hash.clone(),
                };
                let mut line = serde_json::to_vec(&rec).map_err(io::Error::other)?;
                line.push(b'\n');
                if max_bytes > 0 && st.bytes > 0 && st.bytes + line.len() as u64 > max_bytes {
                    rotate_locked(&mut st, &path)?;
                }
                st.file.write_all(&line)?;
                st.file.sync_all()?;
                st.seq += 1;
                st.last_hash = hash;
                st.bytes += line.len() as u64;
                counters.entries.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })();
            if let Err(e) = &res {
                // 锁存首个失败：后续追加一律失败，避免「半行之后继续写」
                // 把链破坏扩展成静默假成功。
                // EN: Latch the first failure: later appends all fail, preventing a
                // EN: partial line from extending chain corruption into silent fake success.
                if st.poisoned.is_none() {
                    st.poisoned = Some(e.to_string());
                }
                counters.write_errors.fetch_add(1, Ordering::Relaxed);
            }
            res
        })
        .await
        .map_err(|e| io::Error::other(format!("audit join failed: {e}")))?;
        result
    }

    /// 校验整条链（含轮转段）。
    /// Verify the whole chain, including rotated segments.
    pub async fn verify(&self) -> io::Result<VerifyReport> {
        let state = Arc::clone(&self.state);
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            // 只在快照阶段持锁：轮转段不可变，活动文件按快照长度截断读取，
            // 长扫描期间写路径不被阻塞（OCR 批次三十三）。
            // EN: hold the lock only while snapshotting; the scan itself runs
            // outside so audited writes are not stalled by an admin verify.
            let (files, cap) = {
                let _guard = state.lock().unwrap_or_else(|e| e.into_inner());
                let files = segment_files(&path)?;
                let cap = std::fs::metadata(&path).ok().map(|m| m.len());
                (files, cap)
            };
            verify_plan(files, &path, cap).map(VerifyReport::from_full)
        })
        .await
        .map_err(|e| io::Error::other(format!("audit verify join failed: {e}")))?
    }

    /// 哪些请求进入审计：默认只记写/管理/认证失败；读请求需显式开启。
    /// Default: mutations, admin paths, and auth failures; reads opt-in.
    pub fn should_record(&self, method: &str, path: &str, status: u16) -> bool {
        if matches!(path, "/v1/health" | "/readyz" | "/metrics") {
            return false;
        }
        if self.include_reads {
            return true;
        }
        let mutation = matches!(method, "POST" | "PUT" | "PATCH" | "DELETE");
        let admin = path.starts_with("/v1/admin");
        let auth_failure = status == 401 || status == 403;
        mutation || admin || auth_failure
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

fn rotate_locked(st: &mut AuditState, path: &Path) -> io::Result<()> {
    st.file.sync_all()?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "audit path has no filename"))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let archived = path.with_file_name(format!("{name}.{nanos}"));
    std::fs::rename(path, &archived)?;
    st.file = open_append(path)?;
    st.bytes = 0;
    Ok(())
}

/// 轮转段 + 活动文件的顺序（时间戳后缀升序，活动文件最后）。
/// Segment order: rotated segments by timestamp, active file last.
fn segment_files(path: &Path) -> io::Result<Vec<PathBuf>> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "audit path has no filename"))?;
    let prefix = format!("{name}.");
    let mut archived = Vec::new();
    if parent.exists() {
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            let fname = entry.file_name();
            let s = fname.to_string_lossy();
            if let Some(suffix) = s.strip_prefix(&prefix) {
                if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                    archived.push(entry.path());
                }
            }
        }
    }
    archived.sort();
    let mut files = archived;
    if path.exists() {
        files.push(path.to_path_buf());
    }
    Ok(files)
}

struct FullReport {
    entries: u64,
    valid: bool,
    first_bad_seq: Option<u64>,
    last_seq: Option<u64>,
    last_hash: String,
    segments: usize,
}

fn verify_files(path: &Path) -> io::Result<FullReport> {
    let files = segment_files(path)?;
    verify_plan(files, path, None)
}

/// 按快照的文件列表校验；活动文件只读到 `active_cap`（快照长度）。
/// Verify a snapshot file list; the active file is read up to `active_cap`.
fn verify_plan(
    files: Vec<PathBuf>,
    active_path: &Path,
    active_cap: Option<u64>,
) -> io::Result<FullReport> {
    let mut expected_seq = 0u64;
    let mut prev_hash = GENESIS_HASH.to_string();
    let mut entries = 0u64;
    let mut last_seq = None;
    for file in &files {
        let f = File::open(file)?;
        let reader: Box<dyn BufRead> = if file == active_path {
            match active_cap {
                Some(cap) => Box::new(BufReader::new(f.take(cap))),
                None => Box::new(BufReader::new(f)),
            }
        } else {
            Box::new(BufReader::new(f))
        };
        for (lineno, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let rec: AuditRecord = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(_) => {
                    return Ok(FullReport {
                        entries,
                        valid: false,
                        first_bad_seq: Some(expected_seq),
                        last_seq,
                        last_hash: prev_hash,
                        segments: files.len(),
                    })
                }
            };
            let computed = record_hash(&rec)?;
            let ok = rec.seq == expected_seq && rec.prev_hash == prev_hash && computed == rec.hash;
            if !ok {
                let _ = (file, lineno);
                return Ok(FullReport {
                    entries,
                    valid: false,
                    first_bad_seq: Some(rec.seq.min(expected_seq)),
                    last_seq,
                    last_hash: prev_hash,
                    segments: files.len(),
                });
            }
            prev_hash = rec.hash;
            last_seq = Some(rec.seq);
            expected_seq += 1;
            entries += 1;
        }
    }
    Ok(FullReport {
        entries,
        valid: true,
        first_bad_seq: None,
        last_seq,
        last_hash: prev_hash,
        segments: files.len(),
    })
}

impl VerifyReport {
    fn from_full(r: FullReport) -> Self {
        Self {
            entries: r.entries,
            valid: r.valid,
            first_bad_seq: r.first_bad_seq,
            segments: r.segments,
            last_hash: r.last_hash,
        }
    }
}

/// 从请求头提取审计主体（token 指纹，绝不落原始凭据）。
/// Actor = token fingerprint; raw credentials never reach the log.
pub fn actor_from_headers(headers: &HeaderMap) -> String {
    // 与 require_auth 共用同一套解析（scheme 大小写不敏感、双凭据回退），
    // 否则「认证通过但审计记为 anonymous」或同一 token 出现两种身份
    // （OCR 批次三十三）。
    // EN: share the auth parser — otherwise a valid request can be audited as
    // anonymous under a differently-cased scheme.
    match crate::rbac::extract_tokens(headers).first().copied() {
        Some(token) => {
            let digest = sha256_hex(token.as_bytes());
            format!("token:{}", &digest[..16])
        }
        _ => "anonymous".to_string(),
    }
}

/// HTTP 审计中间件（外层于认证，才能看到 401/403）。
/// Audit middleware, outermost to auth so 401/403 responses are recorded.
pub async fn audit_middleware(
    State(audit): State<Option<Arc<AuditLog>>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(audit) = audit else {
        return next.run(req).await;
    };
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let fallback_actor = actor_from_headers(req.headers());
    let response = next.run(req).await;
    // 认证中间件在响应扩展里留下主体名：审计优先记录人可读身份，
    // 未认证请求回退到 token 指纹/anonymous。
    // EN: prefer the authenticated principal's name for the audit actor.
    let actor = response
        .extensions()
        .get::<crate::rbac::AuthenticatedPrincipal>()
        .map(|p| format!("principal:{}", p.name))
        .unwrap_or(fallback_actor);
    let status = response.status();
    if audit.should_record(&method, &path, status.as_u16()) {
        let outcome = format!("http:{}", status.as_u16());
        if let Err(e) = audit.record(&actor, &method, &path, &outcome).await {
            tracing::error!(
                path = %path,
                method = %method,
                error = %e,
                "audit log write failed"
            );
            // fail-closed：成功的写/管理响应在审计失败时改为 500——写可能已
            // 发生但没有证据，不能向调用方报告成功。
            // EN: a successful mutating response becomes 500 when it could not
            // be audited; the write may have happened without evidence.
            if status.is_success() || status.is_redirection() {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "audit log write failed; the operation may have been applied",
                )
                    .into_response();
            }
        }
    }
    response
}

// ── 管理端点 ───────────────────────────────────────────────────
// EN: Admin endpoints.

/// `GET /v1/admin/audit/verify` 响应（含 enabled 开关）。
/// Response body for the audit verification endpoint.
#[derive(Debug, Serialize)]
pub struct AuditVerifyResponse {
    pub enabled: bool,
    #[serde(flatten)]
    pub report: Option<VerifyReport>,
}

/// 校验审计链（管理员端点；认证由全局中间件承担）。
/// Verify the audit chain (auth handled by the global middleware).
pub async fn handle_admin_audit_verify(
    State(state): State<crate::types::BenchmarkHttpState>,
) -> Result<axum::Json<AuditVerifyResponse>, (StatusCode, String)> {
    match &state.audit {
        None => Ok(axum::Json(AuditVerifyResponse {
            enabled: false,
            report: None,
        })),
        Some(audit) => {
            let report = audit.verify().await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("audit verify failed: {e}"),
                )
            })?;
            Ok(axum::Json(AuditVerifyResponse {
                enabled: true,
                report: Some(report),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::header;
    use axum::routing::post;
    use axum::Router;
    use tower::ServiceExt;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "houyi-audit-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("audit.log")
    }

    #[tokio::test]
    async fn chain_verifies_and_detects_tamper() {
        let path = tmp("tamper");
        let log = AuditLog::open(path.clone(), false, 0).unwrap();
        log.record("token:abc", "POST", "/v1/collections/c/upsert", "http:200")
            .await
            .unwrap();
        log.record("token:abc", "POST", "/v1/collections/c/delete", "http:200")
            .await
            .unwrap();
        let report = log.verify().await.unwrap();
        assert!(report.valid);
        assert_eq!(report.entries, 2);

        // 改写第二条记录 → 链断（hash 不匹配）。
        // EN: Rewriting the second record breaks the chain (hash mismatch).
        let text = std::fs::read_to_string(&path).unwrap();
        let tampered = text.replacen("\"actor\":\"token:abc\"", "\"actor\":\"token:evil\"", 1);
        assert_ne!(text, tampered);
        std::fs::write(&path, tampered).unwrap();
        let report = log.verify().await.unwrap();
        assert!(!report.valid, "tampering must be detected");
        assert_eq!(report.first_bad_seq, Some(0));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn restart_continues_chain_and_rejects_broken_history() {
        let path = tmp("restart");
        {
            let log = AuditLog::open(path.clone(), false, 0).unwrap();
            log.record("a", "POST", "/x", "http:200").await.unwrap();
        }
        // 重启：追加后仍可校验（seq 从 1 继续，prev_hash 接上）。
        // EN: Restart: appends still verify (seq continues from 1, prev_hash links up).
        {
            let log = AuditLog::open(path.clone(), false, 0).unwrap();
            log.record("b", "POST", "/y", "http:200").await.unwrap();
            let report = log.verify().await.unwrap();
            assert!(report.valid);
            assert_eq!(report.entries, 2);
        }
        // 破坏后重启必须拒绝（fail-closed）。
        // EN: Restart after tampering must refuse (fail-closed).
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            text.replacen("\"actor\":\"a\"", "\"actor\":\"z\"", 1),
        )
        .unwrap();
        let err = AuditLog::open(path.clone(), false, 0).unwrap_err();
        assert!(err.to_string().contains("chain is broken"), "got: {err}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn rotation_keeps_chain_verifiable() {
        let path = tmp("rotate");
        // 极小阈值强制轮转：每条 ~200B，阈值 450B 保证产生至少两段。
        // EN: Tiny threshold forces rotation: ~200B per entry; 450B yields >= 2 segments.
        let log = AuditLog::open(path.clone(), false, 450).unwrap();
        for i in 0..6 {
            log.record("t", "POST", &format!("/v1/x/{i}"), "http:200")
                .await
                .unwrap();
        }
        let report = log.verify().await.unwrap();
        assert!(report.valid, "chain must survive rotation");
        assert_eq!(report.entries, 6);
        assert!(report.segments >= 2, "expected rotation, got {report:?}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 追加失败后锁存毒化：后续 record 必须立即失败（fail-closed），
    /// 不能继续在破损链上追加并返回成功（OCR 批次三十三）。
    /// EN: after an append failure every later record must fail fast.
    #[tokio::test]
    async fn append_failure_poisons_the_log() {
        let path = tmp("poison");
        let log = AuditLog::open(path.clone(), false, 0).unwrap();
        log.record("a", "POST", "/x", "http:200").await.unwrap();
        {
            // 模拟 write_all/sync_all 失败后的锁存（真实 IO 失败难以可移植
            // 地制造；这里验证毒化后的行为契约）。
            // EN: Simulate the latch after write_all/sync_all failure (real IO failure is
            // EN: hard to produce portably; this verifies the post-poisoning contract).
            let mut st = log.state.lock().unwrap();
            st.poisoned = Some("injected append failure".to_string());
        }
        let err = log.record("b", "POST", "/y", "http:200").await.unwrap_err();
        assert!(err.to_string().contains("poisoned"), "got: {err}");
        assert!(log.write_errors_total() >= 1);
        // 已成功的记录仍可校验；毒化不会破坏既有链。
        // EN: Already-successful records still verify; poisoning does not corrupt the chain.
        let report = log.verify().await.unwrap();
        assert!(report.valid);
        assert_eq!(report.entries, 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 审计的 actor 解析必须与认证一致：scheme 大小写/多余空格不改变身份。
    /// EN: audit actor parsing matches auth (case-insensitive scheme).
    #[test]
    fn actor_parsing_matches_auth() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "bearer s3cret".parse().unwrap());
        let a = actor_from_headers(&headers);
        headers.insert(header::AUTHORIZATION, "BEARER  s3cret".parse().unwrap());
        let b = actor_from_headers(&headers);
        assert_eq!(a, b);
        assert_eq!(a, format!("token:{}", &sha256_hex(b"s3cret")[..16]));
    }

    #[test]
    fn actor_fingerprint_never_leaks_the_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer s3cret-token".parse().unwrap(),
        );
        let actor = actor_from_headers(&headers);
        assert!(actor.starts_with("token:"));
        assert!(!actor.contains("s3cret"));
        assert_eq!(actor.len(), "token:".len() + 16);

        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "another-secret".parse().unwrap());
        let actor2 = actor_from_headers(&headers);
        assert!(actor2.starts_with("token:"));
        assert!(!actor2.contains("another-secret"));

        assert_eq!(actor_from_headers(&HeaderMap::new()), "anonymous");
    }

    #[test]
    fn should_record_covers_mutations_admin_and_auth_failures() {
        let path = tmp("policy");
        let log = AuditLog::open(path.clone(), false, 0).unwrap();
        assert!(log.should_record("POST", "/v1/collections/c/upsert", 200));
        assert!(log.should_record("GET", "/v1/admin/audit/verify", 200));
        assert!(log.should_record("GET", "/v1/collections", 401));
        assert!(!log.should_record("GET", "/v1/collections", 200));
        assert!(!log.should_record("GET", "/v1/health", 200));
        assert!(!log.should_record("GET", "/metrics", 200));

        let reads = AuditLog::open(path.with_file_name("reads.log"), true, 0).unwrap();
        assert!(reads.should_record("GET", "/v1/collections", 200));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 中间件端到端：写路由产生审计条目；401 也入账；审计关闭时无文件。
    /// EN: Middleware end-to-end: write routes emit audit entries; 401s are recorded
    /// EN: too; audit disabled leaves no file.
    #[tokio::test]
    async fn middleware_records_mutations_and_auth_failures() {
        let path = tmp("mw");
        let log = Arc::new(AuditLog::open(path.clone(), false, 0).unwrap());
        let app = Router::new()
            .route("/write", post(|| async { StatusCode::OK }))
            .route("/fail", post(|| async { StatusCode::UNAUTHORIZED }))
            .layer(axum::middleware::from_fn_with_state(
                Some(Arc::clone(&log)),
                audit_middleware,
            ));

        let req = Request::builder()
            .method("POST")
            .uri("/write")
            .header("authorization", "Bearer tok")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = Request::builder()
            .method("POST")
            .uri("/fail")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let report = log.verify().await.unwrap();
        assert!(report.valid);
        assert_eq!(report.entries, 2, "both mutation and 401 must be audited");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("token:"));
        assert!(!text.contains("Bearer tok"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
