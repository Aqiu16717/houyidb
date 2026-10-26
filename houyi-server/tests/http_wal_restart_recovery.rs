//! 回归：HTTP upsert → WAL 落盘后杀进程 → 同一路径冷启动 → 检索仍返回预期 id。
//! Regression: WAL-first upsert survives process death; cold restart replays into index and search works.
//!
//! 与单元测试中 `replay_vector_wal_into_index` 不同，这里走 **真实子进程** + **`/v1/vectors/*`**，覆盖 CLI、监听与序列化路径。

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn unique_test_dir() -> PathBuf {
    // pid + 纳秒 + 进程内原子序号：本二进制两个测试并行跑，同时钟周期取值会
    // 撞名（Apple Silicon 时钟粒度粗于 1ns，d605c33 在 houyi-persistence 修过
    // 同根因）。撞名后一方的 clear_wal_artifacts/remove_dir_all 会删掉另一方
    // 正在使用的目录，表现为子进程 WAL open ENOENT 启动即退、stdout EOF。
    // EN: pid + nanos + in-process atomic sequence: two tests in this binary run in parallel,
    // EN: and same-clock-tick values collide (Apple Silicon clock granularity is coarser than
    // EN: 1ns; d605c33 fixed the same root cause in houyi-persistence). On collision one side's
    // EN: clear_wal_artifacts/remove_dir_all deletes the other's live directory, showing up as
    // EN: child WAL open ENOENT at startup and stdout EOF.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "houyidb-restart-recovery-{}-{nanos}-{seq}",
        std::process::id()
    ))
}

fn clear_wal_artifacts(wal: &Path) {
    let _ = std::fs::remove_file(wal);
    // 与 `houyi-persistence::wal_sidecar::sidecar_path_for_wal` 一致：`foo.wal` → `foo.applied_lsn`
    // EN: Matches `houyi-persistence::wal_sidecar::sidecar_path_for_wal`:
    // EN: `foo.wal` -> `foo.applied_lsn`.
    let parent = wal.parent().unwrap_or_else(|| Path::new("."));
    let stem = wal.file_stem().and_then(|s| s.to_str()).unwrap_or("wal");
    let sidecar = parent.join(format!("{stem}.applied_lsn"));
    let _ = std::fs::remove_file(&sidecar);
    // 对应路径：`foo.wal` → `foo.vector_snapshot.json` / `foo.wal.next_lsn`。
    // EN: `foo.wal` -> `foo.vector_snapshot.json` / `foo.wal.next_lsn`.
    let _ = std::fs::remove_file(wal.with_extension("vector_snapshot.json"));
    let _ = std::fs::remove_file(wal.with_extension("wal.next_lsn"));
}

fn drain_reader_to_eof(mut r: impl Read + Send + 'static) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while r.read(&mut buf).unwrap_or(0) > 0 {}
    });
}

/// 子进程守卫：断言 panic 沿栈展开时也能 kill + wait，防孤儿 houyi-server
/// 泄漏（占着端口/WAL 影响后续用例，CI 上悬挂到 job 超时）。
/// EN: RAII guard that reaps the child even on panic unwind.
struct ChildGuard(Option<std::process::Child>);

impl ChildGuard {
    fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 从 stdout 读到 `Listening on benchmark_http://`；随后后台排空 stdout/stderr，避免关闭管道读端导致子进程 **SIGPIPE/EPIPE**。
///
/// 子串匹配而非行前缀：P4.0 起服务器日志走 tracing（行首是 时间戳+级别+target，
/// 级别/target 还带 ANSI 颜色），消息文本出现在行内而非行首。
///
/// stderr 全程捕获到共享缓冲（只留尾部）：子进程启动即退出时（stdout EOF），
/// 其死因只在 stderr——丢弃它曾让 CI 上的间歇失败无法归因。
/// EN: stderr is captured into a shared tail buffer so early child exits are
/// diagnosable from the returned error.
fn read_benchmark_http_base(
    mut child: std::process::Child,
    timeout: Duration,
) -> std::io::Result<(std::process::Child, String)> {
    const MARKER: &str = "Listening on benchmark_http://";
    let stderr_tail: std::sync::Arc<std::sync::Mutex<String>> =
        std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(mut stderr) = child.stderr.take() {
        let buf = std::sync::Arc::clone(&stderr_tail);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = stderr.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut b = buf.lock().unwrap();
                b.push_str(&String::from_utf8_lossy(&chunk[..n]));
                // 只保留尾部 ~8KB：长跑日志不撑爆内存，启动死因一定在尾部。
                // drain 的字节下标可能落在多字节字符中间（String::drain 对非
                // char 边界直接 panic）——先对齐到边界再删，宁多留几字节。
                // EN: Keep only the last ~8KB: long-run logs do not blow up memory, and startup
                // EN: failures are always at the tail. The drain byte index may land mid multi-byte
                // EN: character (String::drain panics on non-char boundaries) — align to the
                // EN: boundary first, keeping a few extra bytes.
                let len = b.len();
                if len > 8192 {
                    let mut cut = len - 8192;
                    while cut < len && !b.is_char_boundary(cut) {
                        cut += 1;
                    }
                    b.drain(..cut);
                }
            }
        });
    }
    let stdout = child.stdout.take().expect("stdout piped");
    let start = Instant::now();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    while start.elapsed() < timeout {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if let Some(pos) = line.find(MARKER) {
            // tracing 的 ANSI 颜色只包裹级别/target，消息文本无颜色；防御性截掉
            // 可能的转义序列与空白，只留 host:port。
            // EN: tracing ANSI colors wrap only level/target, not message text; defensively strip
            // EN: possible escape sequences and whitespace, keeping only host:port.
            let rest = &line[pos + MARKER.len()..];
            let host_port: String = rest
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '\u{1b}')
                .collect();
            // 必须保持管道打开并持续 drain，否则服务器后续日志可能因写端 broken pipe 异常退出。
            // EN: Must keep the pipe open and drain continuously, or later server logs may
            // EN: exit on a broken pipe.
            drain_reader_to_eof(reader);
            return Ok((child, format!("http://{}", host_port)));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    // 等一拍让 stderr drain 线程把残余字节收进缓冲。
    // EN: Wait one tick for the stderr drain thread to collect remaining bytes.
    std::thread::sleep(Duration::from_millis(50));
    let tail = stderr_tail.lock().unwrap();
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "timeout waiting for Listening on benchmark_http://; child stderr tail: {}",
            tail.trim()
        ),
    ))
}

fn post_json(base: &str, path: &str, body: &serde_json::Value) -> serde_json::Value {
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let mut resp = ureq::post(&url)
        .header("Content-Type", "application/json; charset=utf-8")
        .send_json(body.clone())
        .unwrap_or_else(|e| panic!("POST {url} failed: {e}"));
    resp.body_mut()
        .read_json()
        .unwrap_or_else(|e| panic!("decode JSON from {url}: {e}"))
}

/// `Listening on...` 打印后，`axum::serve` 的 task 可能尚未开始 accept；轮询直到健康检查成功。
/// EN: After `Listening on...` is printed, the `axum::serve` task may not have started
/// EN: accepting; poll until the health check succeeds.
fn wait_for_health_ok(base: &str, timeout: Duration) {
    let url = format!("{}/v1/health", base.trim_end_matches('/'));
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(mut resp) = ureq::get(&url).call() {
            if let Ok(v) = resp.body_mut().read_json::<serde_json::Value>() {
                if v["status"] == "ok" {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timeout waiting for GET {url} status=ok");
}

/// 启动 `houyi-server` 子进程（HTTP / PG 端口均为 0，由内核分配）；返回 `(child, benchmark base URL)`。
/// EN: Start the `houyi-server` child process (HTTP / PG ports are 0, kernel-assigned);
/// EN: returns `(child, benchmark base URL)`.
fn spawn_houyi_server(wal: &Path) -> (std::process::Child, String) {
    // Cargo 在 `cargo test` 时注入 `CARGO_BIN_EXE_<bin>`（`houyi-server` → `..._houyi_server`）。
    // EN: Cargo injects `CARGO_BIN_EXE_<bin>` during `cargo test` (`houyi-server` ->
    // EN: `..._houyi_server`).
    let bin = std::env::var("CARGO_BIN_EXE_houyi-server")
        .or_else(|_| std::env::var("CARGO_BIN_EXE_houyi_server"))
        .expect("binary path missing — run via `cargo test -p houyi-server`");
    let child = Command::new(bin)
        .args([
            "--benchmark-http-addr",
            "127.0.0.1:0",
            "--pgwire-addr",
            "127.0.0.1:0",
            "--wal-path",
            wal.to_str().expect("utf8 wal path"),
            "--vector-dim",
            "4",
            "--worker-threshold",
            "8",
            "--worker-tick-ms",
            "20",
            "--metrics-interval-secs",
            "120",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn houyi-server");
    let (child, base) =
        read_benchmark_http_base(child, Duration::from_secs(45)).expect("read benchmark http url");
    (child, base)
}

#[test]
#[cfg_attr(not(unix), ignore = "uses std::process::Child::kill (unix)")]
fn wal_persisted_vectors_survive_server_restart() {
    let dir = unique_test_dir();
    let _ = std::fs::create_dir_all(&dir);
    let wal = dir.join("bench.wal");
    clear_wal_artifacts(&wal);

    let upsert_body = serde_json::json!({
        "vectors": [
            { "id": 1, "values": [1.0_f32, 0.0, 0.0, 0.0] },
            { "id": 2, "values": [0.0_f32, 1.0, 0.0, 0.0] }
        ]
    });

    let (child1, base) = spawn_houyi_server(&wal);
    let child1 = ChildGuard::new(child1);
    wait_for_health_ok(&base, Duration::from_secs(5));

    let up = post_json(&base, "/v1/vectors/upsert", &upsert_body);
    assert_eq!(up["inserted"], 2);

    let search = post_json(
        &base,
        "/v1/vectors/search",
        &serde_json::json!({
            "vector": [1.0_f32, 0.0, 0.0, 0.0],
            "k": 2,
            "ef_search": 32
        }),
    );
    let ids_before: Vec<u64> = search["ids"]
        .as_array()
        .expect("ids array")
        .iter()
        .map(|x| x.as_u64().expect("id u64"))
        .collect();
    assert!(
        ids_before.contains(&1),
        "before restart: expected id 1 in top-k, got {:?}",
        ids_before
    );

    drop(child1); // SIGKILL + reap，随后同 WAL 路径冷启动

    let (child2, base2) = spawn_houyi_server(&wal);
    let child2 = ChildGuard::new(child2);
    wait_for_health_ok(&base2, Duration::from_secs(5));

    let search_after = post_json(
        &base2,
        "/v1/vectors/search",
        &serde_json::json!({
            "vector": [1.0_f32, 0.0, 0.0, 0.0],
            "k": 2,
            "ef_search": 32
        }),
    );
    let ids_after: Vec<u64> = search_after["ids"]
        .as_array()
        .expect("ids array")
        .iter()
        .map(|x| x.as_u64().expect("id u64"))
        .collect();
    assert!(
        ids_after.contains(&1),
        "after restart: expected id 1 in top-k, got {:?}",
        ids_after
    );

    // 先停服务再清理工件：活进程握着 WAL/侧车句柄，清理不应与其并发。
    // EN: Stop the server before cleaning artifacts: a live process holds WAL/sidecar handles,
    // EN: so cleanup must not race with it.
    drop(child2);
    clear_wal_artifacts(&wal);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 故障注入（P4.1.2）：写入负载进行中 SIGKILL —— WAL 尾部可能是撕裂的半条
/// 记录（异步写线程批量 flush 被杀在半途），重启后：
/// 1) 服务必须能起（v2 CRC + 撕裂尾容忍，重放不 panic）；
/// 2) 所有「已收到 200 确认」的向量必须可检索（page cache 在进程死后仍存活，
///    ack 即持久）；在途未确认的请求允许丢失（客户端未收到成功，可安全重试）。
///
/// EN: SIGKILL mid-write-load: the WAL tail may be a torn partial record.
/// After restart every acknowledged vector must be searchable; unacknowledged
/// in-flight writes may be lost (client never saw success).
#[test]
#[cfg_attr(not(unix), ignore = "uses std::process::Child::kill (unix)")]
fn wal_survives_sigkill_under_write_load() {
    use std::sync::{Arc, Mutex};

    let dir = unique_test_dir();
    let _ = std::fs::create_dir_all(&dir);
    let wal = dir.join("bench.wal");
    clear_wal_artifacts(&wal);

    // id → 确定性向量（方向随 id 变化，检索自身向量必得 score=1 的 top 组）。
    // EN: id -> deterministic vector (direction varies with id; searching its own vector yields
    // EN: the score=1 top group).
    fn vec_for_id(id: u64) -> [f32; 4] {
        [
            (id % 7 + 1) as f32,
            (id % 5 + 1) as f32,
            (id % 3 + 1) as f32,
            (id % 11 + 1) as f32,
        ]
    }

    let (child1, base) = spawn_houyi_server(&wal);
    let child1 = ChildGuard::new(child1);
    wait_for_health_ok(&base, Duration::from_secs(5));

    // 后台写入线程：持续批量 upsert；被杀后连接断开自然出错退出。
    // EN: Background writer thread: continuous batched upserts; after the kill the connection
    // EN: drops and it exits with an error naturally.
    let acked: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    // 写入线程最后一次错误：被杀后的连接错误是预期退出路径，被杀前的错误
    // 是真实故障——断言失败时并入诊断，省得盲猜是负载不足还是服务器早崩。
    // EN: Writer thread's last error: after the kill, a connection error is the expected exit
    // EN: path; before the kill it is a real fault — include it in assertion diagnostics
    // EN: instead of guessing whether load was insufficient or the server died early.
    let writer_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let writer = {
        let base = base.clone();
        let acked = Arc::clone(&acked);
        let writer_err = Arc::clone(&writer_err);
        std::thread::spawn(move || {
            const BATCH: u64 = 20;
            let mut next: u64 = 1_000_000; // 避开 main 自检向量 id 0..2
            loop {
                let vectors: Vec<serde_json::Value> = (next..next + BATCH)
                    .map(|id| {
                        let v = vec_for_id(id);
                        serde_json::json!({ "id": id, "values": v })
                    })
                    .collect();
                let url = format!("{}/v1/vectors/upsert", base.trim_end_matches('/'));
                // ureq 3：非 2xx 响应走 Err(Error::StatusCode)，连接错误走
                // 其余 Err 变体——Ok 即成功，无独立 status 分支可判。
                // EN: ureq 3: non-2xx responses yield Err(Error::StatusCode); connection errors
                // EN: use other Err variants — Ok means success, no separate status branch.
                match ureq::post(&url)
                    .header("Content-Type", "application/json; charset=utf-8")
                    .send_json(serde_json::json!({ "vectors": vectors }))
                {
                    Ok(_) => {
                        acked.lock().unwrap().extend(next..next + BATCH);
                        next += BATCH;
                    }
                    Err(e) => {
                        *writer_err.lock().unwrap() = Some(format!("POST {url} => {e}"));
                        break; // 服务器已被杀：连接错误即退出
                    }
                }
            }
        })
    };

    // 等足够多批次确认后，在写入流中段 SIGKILL（保证杀在写路径活跃期）。
    // 写入线程若提前死亡（如服务器在负载期崩溃），立即退出轮询而非干等
    // 15s——下方断言会带着 writer_err 诊断爆炸。
    // EN: After enough acked batches, SIGKILL mid-stream (ensures the write path is active).
    // EN: If the writer dies early (e.g. the server crashes under load), exit the poll
    // EN: immediately instead of waiting 15s — the assertions below explode with writer_err
    // EN: diagnostics.
    let start = Instant::now();
    loop {
        let n = acked.lock().unwrap().len();
        if n >= 200 || writer.is_finished() || start.elapsed() > Duration::from_secs(15) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(child1); // SIGKILL + reap（ChildGuard）
                  // join 结果必须检查：writer panic（而非记录错误）时若静默吞掉，
                  // 后面的 acked 断言会在一个不完整的数据集上"通过"。
                  // EN: The join result must be checked: if a writer panic (not a recorded error)
                  // EN: is swallowed, the later acked assertions "pass" on an incomplete dataset.
    writer.join().expect("writer thread must not panic");
    // join 之后才取快照：写入线程已停，快照即最终全集。先 take 后杀的旧顺序
    // 存在竞态窗口——窗口内确认的 id 会漏出校验集（已确认却未验证）。
    // EN: Take the snapshot only after join: the writer has stopped, so it is the final set.
    // EN: The old take-then-kill order had a race window where acked ids escaped verification.
    let acked_ids: Vec<u64> = std::mem::take(&mut *acked.lock().unwrap());
    let last_writer_err = writer_err.lock().unwrap().take();
    assert!(
        acked_ids.len() >= 200,
        "load phase must acknowledge >= 200 vectors, got {}; last writer error: {:?}",
        acked_ids.len(),
        last_writer_err
    );

    // 重启：撕裂 WAL 不得阻碍启动。
    // EN: Restart: a torn WAL must not block startup.
    let (child2, base2) = spawn_houyi_server(&wal);
    let child2 = ChildGuard::new(child2);
    wait_for_health_ok(&base2, Duration::from_secs(10));

    // 抽样已确认 id（首/尾各 5 + 均匀 10 个），每个必须落在自身向量检索的
    // top-8（score=1 同分组内，k=8 吸收同分平局）。
    // EN: Sample acked ids (first/last 5 + 10 uniform); each must land in the top-8 of its
    // EN: own-vector search (score=1 tie group; k=8 absorbs equal-score ties).
    let mut sample: Vec<u64> = Vec::new();
    sample.extend_from_slice(&acked_ids[..5]);
    sample.extend_from_slice(&acked_ids[acked_ids.len() - 5..]);
    let step = (acked_ids.len() / 10).max(1);
    sample.extend(acked_ids.iter().step_by(step).take(10).copied());
    for id in sample {
        let v = vec_for_id(id);
        let res = post_json(
            &base2,
            "/v1/vectors/search",
            &serde_json::json!({ "vector": v, "k": 8, "ef_search": 64 }),
        );
        let ids: Vec<u64> = res["ids"]
            .as_array()
            .expect("ids array")
            .iter()
            .map(|x| x.as_u64().expect("id u64"))
            .collect();
        assert!(
            ids.contains(&id),
            "acknowledged id {id} must be searchable after SIGKILL restart, got {ids:?}"
        );
    }

    // 先停服务再清理工件（同测试一）。
    // EN: Stop the server before cleaning artifacts (same as test one).
    drop(child2);
    clear_wal_artifacts(&wal);
    let _ = std::fs::remove_dir_all(&dir);
}
