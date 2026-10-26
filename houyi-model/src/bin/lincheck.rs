//! 简化线性化检查器（P4.5.7 fallback）——读客户端观测历史，检查 write-once
//! register 的线性化。
//! Simplified linearizability checker for write-once registers.
//!
//! 为什么是 fallback（WHY）：计划首选 jepsen-io/elle（Clojure jar + JDK）。
//! 本机虽有 JDK 21，但 Elle 需要 Clojure 运行时与 jar 依赖树；为了不把「跑
//! 一次检查」变成装一整套 JVM 生态，这里实现业界同构的**必要条件检查**
//! （real-time 序 + 读己之所写）。Elle 仍可在具备环境的机器上替换本工具。
//! EN: Elle needs a Clojure runtime and jar tree; this implements the
//! real-time-order + no-lost-write conditions for write-once registers. Elle
//! remains a drop-in upgrade on machines that have it.
//!
//! 输入 / Input（JSONL，一行一个操作，客户端自己记录）：
//! ```json
//! {"op":"upsert","collection":"c","ids":[1,2],"invoke_ns":100,"ok_ns":200,"status":"ok"}
//! {"op":"search","collection":"c","probe":[1,2],"found":[1],"invoke_ns":300,"ok_ns":400,"status":"ok"}
//! ```
//! - `upsert`：把 `ids` 置为「存在」（write-once：重复写同 id 幂等）。
//! - `search`：探测 `probe` 中的 id，`found` 是实际返回的 id 子集。
//! - `status` 三态（与 Jepsen 对齐）：
//!   - `ok`：确认成功；
//!   - `fail`：**确定失败**（服务端回了错误响应）——不得被任何读观察到；
//!   - `info`：**结果不确定**（连接中断/超时）——写可能已生效，读观察到它合法，
//!     但不产生「已确认写」的可见性义务。
//!
//! 判定 / Checks（逐 (collection, id) 独立）：
//! 1. **无幻读**：读看到 id ⇒ 存在某次写在其完成前已发起（`invoke_w < ok_r`）。
//! 2. **无丢写**（strict 模式）：写在读发起前已确认（`ok_w < invoke_r`）⇒ 读必须
//!    看到它。write-once register 下这两条与线性化等价。
//! 3. `--eventual` 模式跳过第 2 条（async 复制允许 staleness），只查幻读。
//!
//! 用法 / Usage:
//!   lincheck --strict hist1.jsonl hist2.jsonl
//!   lincheck --eventual hist-async.jsonl
//!   lincheck --strict --collection c_full hist.jsonl

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Op {
    op: String,
    #[serde(default)]
    collection: String,
    #[serde(default)]
    ids: Vec<u64>,
    #[serde(default)]
    probe: Vec<u64>,
    #[serde(default)]
    found: Vec<u64>,
    #[serde(default)]
    status: String,
    invoke_ns: u64,
    ok_ns: u64,
}

#[derive(Debug, Clone, Copy)]
struct Write {
    invoke: u64,
    ok: u64,
    /// 是否**确定**成功（`ok`）；`info` 的写可能已生效但不算已确认。
    committed: bool,
}

#[derive(Debug, Clone, Copy)]
struct Read {
    invoke: u64,
    ok: u64,
    found: bool,
}

/// 逐 (collection, id) 的检查结果。
#[derive(Debug, Default)]
struct Violations {
    phantom: Vec<String>,
    lost_write: Vec<String>,
}

fn collect(ops: &[Op]) -> BTreeMap<(String, u64), (Vec<Write>, Vec<Read>)> {
    let mut out: BTreeMap<(String, u64), (Vec<Write>, Vec<Read>)> = BTreeMap::new();
    for op in ops {
        match op.op.as_str() {
            "upsert" => {
                // `ok` 与 `info` 都进入「可能生效的写」集合；`fail` 是确定失败，
                // 不得解释任何读。（与 Jepsen 的 :ok/:info/:fail 语义一致。）
                if op.status != "ok" && op.status != "info" {
                    continue;
                }
                let w = Write {
                    invoke: op.invoke_ns,
                    ok: op.ok_ns,
                    committed: op.status == "ok",
                };
                for id in &op.ids {
                    out.entry((op.collection.clone(), *id))
                        .or_default()
                        .0
                        .push(w);
                }
            }
            "search" => {
                if op.status != "ok" {
                    continue;
                }
                let found: BTreeSet<u64> = op.found.iter().copied().collect();
                for id in &op.probe {
                    let r = Read {
                        invoke: op.invoke_ns,
                        ok: op.ok_ns,
                        found: found.contains(id),
                    };
                    out.entry((op.collection.clone(), *id))
                        .or_default()
                        .1
                        .push(r);
                }
            }
            _ => {}
        }
    }
    out
}

fn check_register(
    key: &(String, u64),
    writes: &[Write],
    reads: &[Read],
    strict: bool,
) -> Violations {
    let mut v = Violations::default();
    for r in reads {
        // 幻读：读看到 id，但没有任何一次**可能生效**的写在此读完成前被发起。
        // （`info` 写可解释读——客户端不知道它是否落了盘。）
        if r.found && !writes.iter().any(|w| w.invoke < r.ok) {
            v.phantom.push(format!(
                "collection={} id={}: read [{},{}] found the id but no write was invoked before {}",
                key.0, key.1, r.invoke, r.ok, r.ok
            ));
        }
        if !strict {
            continue;
        }
        // 丢写：写在读发起前已**确认**，读却没看到。
        if !r.found && writes.iter().any(|w| w.committed && w.ok < r.invoke) {
            let w = writes
                .iter()
                .filter(|w| w.committed && w.ok < r.invoke)
                .min_by_key(|w| w.ok)
                .unwrap();
            v.lost_write.push(format!(
                "collection={} id={}: write ok at {} but read [{},{}] did not see it",
                key.0, key.1, w.ok, r.invoke, r.ok
            ));
        }
    }
    v
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut strict = false;
    let mut collection_filter: Option<String> = None;
    let mut files: Vec<String> = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--strict" => strict = true,
            "--eventual" => strict = false,
            "--collection" => collection_filter = args.next(),
            "--help" | "-h" => {
                eprintln!(
                    "usage: lincheck [--strict|--eventual] [--collection NAME] <history.jsonl>..."
                );
                return ExitCode::SUCCESS;
            }
            path => files.push(path.to_string()),
        }
    }
    if files.is_empty() {
        eprintln!("!! lincheck: no history files given");
        return ExitCode::from(2);
    }

    let mut ops = Vec::new();
    for path in &files {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("!! lincheck: read {path}: {e}");
                return ExitCode::from(2);
            }
        };
        for (lineno, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Op>(line) {
                Ok(op) => {
                    if collection_filter
                        .as_ref()
                        .is_none_or(|c| *c == op.collection)
                    {
                        ops.push(op);
                    }
                }
                Err(e) => {
                    eprintln!("!! lincheck: {path}:{}: bad JSON: {e}", lineno + 1);
                    return ExitCode::from(2);
                }
            }
        }
    }

    let registers = collect(&ops);
    let mut phantom = 0usize;
    let mut lost = 0usize;
    let mut shown = 0usize;
    for (key, (writes, reads)) in &registers {
        let v = check_register(key, writes, reads, strict);
        phantom += v.phantom.len();
        lost += v.lost_write.len();
        if shown < 20 {
            for msg in v.phantom.iter().chain(v.lost_write.iter()).take(20 - shown) {
                eprintln!("!! {msg}");
                shown += 1;
            }
        }
    }

    let mode = if strict {
        "strict (linearizable)"
    } else {
        "eventual (stale reads allowed)"
    };
    println!(
        "lincheck {mode}: {} register(s), {} operation(s), phantom={phantom}, lost_write={lost}",
        registers.len(),
        ops.len()
    );
    if phantom + lost > 0 {
        return ExitCode::FAILURE;
    }
    println!("✓ 线性化检查通过");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsert(ids: &[u64], invoke: u64, ok: u64) -> Op {
        Op {
            op: "upsert".into(),
            collection: "c".into(),
            ids: ids.to_vec(),
            probe: vec![],
            found: vec![],
            status: "ok".into(),
            invoke_ns: invoke,
            ok_ns: ok,
        }
    }

    fn info_upsert(ids: &[u64], invoke: u64, ok: u64) -> Op {
        let mut op = upsert(ids, invoke, ok);
        op.status = "info".into();
        op
    }

    fn search(probe: &[u64], found: &[u64], invoke: u64, ok: u64) -> Op {
        Op {
            op: "search".into(),
            collection: "c".into(),
            ids: vec![],
            probe: probe.to_vec(),
            found: found.to_vec(),
            status: "ok".into(),
            invoke_ns: invoke,
            ok_ns: ok,
        }
    }

    fn check(ops: &[Op], strict: bool) -> (usize, usize) {
        let regs = collect(ops);
        let mut p = 0;
        let mut l = 0;
        for (k, (w, r)) in &regs {
            let v = check_register(k, w, r, strict);
            p += v.phantom.len();
            l += v.lost_write.len();
        }
        (p, l)
    }

    #[test]
    fn linearizable_history_passes() {
        let ops = vec![upsert(&[1], 100, 150), search(&[1], &[1], 200, 250)];
        assert_eq!(check(&ops, true), (0, 0));
    }

    #[test]
    fn lost_write_is_detected_in_strict_mode() {
        let ops = vec![upsert(&[1], 100, 150), search(&[1], &[], 200, 250)];
        assert_eq!(check(&ops, true), (0, 1));
        // eventual 模式允许 stale read
        assert_eq!(check(&ops, false), (0, 0));
    }

    #[test]
    fn phantom_read_is_detected_in_both_modes() {
        // 读完成早于任何写的发起，却看到了 id。
        let ops = vec![search(&[1], &[1], 10, 20), upsert(&[1], 100, 150)];
        assert_eq!(check(&ops, true), (1, 0));
        assert_eq!(check(&ops, false), (1, 0));
    }

    #[test]
    fn overlapping_write_and_read_are_linearizable() {
        // 读与写重叠：把写排在读之前即可线性化。
        let ops = vec![upsert(&[1], 100, 300), search(&[1], &[1], 200, 400)];
        assert_eq!(check(&ops, true), (0, 0));
        // 读先完成、写后发起 → 幻读。
        let ops = vec![search(&[1], &[1], 100, 200), upsert(&[1], 300, 400)];
        assert_eq!(check(&ops, true), (1, 0));
    }

    #[test]
    fn indeterminate_write_can_explain_a_read() {
        // 连接中断（info）的写可能已生效：读看到它合法，且不构成丢写义务。
        let ops = vec![info_upsert(&[1], 100, 150), search(&[1], &[1], 200, 250)];
        assert_eq!(check(&ops, true), (0, 0));
        // 但 info 写不被「读没看到」判为丢写。
        let ops = vec![info_upsert(&[1], 100, 150), search(&[1], &[], 200, 250)];
        assert_eq!(check(&ops, true), (0, 0));
    }

    #[test]
    fn definite_failure_cannot_explain_a_read() {
        let mut fail = upsert(&[1], 100, 150);
        fail.status = "fail".into();
        let ops = vec![fail, search(&[1], &[1], 200, 250)];
        assert_eq!(check(&ops, true), (1, 0));
    }

    #[test]
    fn failed_operations_are_ignored() {
        let mut fail = upsert(&[1], 100, 150);
        fail.status = "fail".into();
        let ops = vec![fail, search(&[1], &[], 200, 250)];
        assert_eq!(check(&ops, true), (0, 0));
    }
}
