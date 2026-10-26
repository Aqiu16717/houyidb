#![no_main]
//! 目标 1：WAL v2 字节流解析（整文件 / 尾部增量 / collection 记录）。
//! EN: Target 1: WAL v2 byte stream parsing (whole file / tail increment /
//! EN: collection records).
//!
//! 断言的不变量 / Invariants:
//! EN: Invariants:
//! - 任意字节不得 panic（解码器是攻击面：磁盘位腐烂、撕裂写、外部注入）。
//! EN: - Arbitrary bytes must not panic (the decoder is an attack surface: disk bit
//! EN:   rot, torn writes, external injection).
//! - 整文件模式：CRC 失败 = 日志终点，返回**有效前缀**而非 Err 崩溃路径。
//! EN: - Whole-file mode: CRC failure = end of log; return the **valid prefix**
//! EN:   rather than an Err crash path.
//! - 尾部增量模式：完整记录 CRC 不匹配 = Err（腐坏，不静默）；头/payload 不完整
//!   = 静默撕裂（返回已确认前缀）。
//! EN: - Tail-increment mode: CRC mismatch on a complete record = Err (corruption,
//! EN:   not silent); incomplete header/payload = silent tear (return the confirmed
//! EN:   prefix).
//! - v≠2 走版本门；非 v2 magic 走 v1 遗留路径（无校验，垃圾进垃圾出是既定兼容
//!   策略，只要求不 panic）。
//! EN: - v≠2 goes through the version gate; non-v2 magic takes the legacy v1 path
//! EN:   (no checksums; garbage in, garbage out is the established compatibility
//! EN:   policy; only no panic is required).
//! - collection 记录：非 JSON / v≠1 / 未知 op → `Ok(None)` 静默跳过；已知 op
//!   腐败 → `Err`（OpProbe fail-fast，静默跳过等于重放时无声丢数据）。
//! EN: - collection records: non-JSON / v≠1 / unknown op → `Ok(None)` silently
//! EN:   skipped; known op corruption → `Err` (OpProbe fail-fast; silently
//! EN:   skipping equals losing data without notice on replay).
//!
//! 覆盖率提示 / Coverage note: 记录体受 CRC32 保护，随机变异几乎必然在 CRC 校验处
//! 终止，故本目标主要覆盖「magic/version/长度/边界」门；记录体语义由 P4.5.0 属性
//! 测试（houyi-persistence/src/proptests.rs）与 scripts/chaos_drill.sh 覆盖。
//! EN: Coverage note: record bodies are CRC32-protected, so random mutation almost
//! EN: always stops at the CRC check; this target mainly covers the
//! EN: magic/version/length/boundary gates. Record-body semantics are covered by
//! EN: the P4.5.0 property tests (houyi-persistence/src/proptests.rs) and
//! EN: scripts/chaos_drill.sh.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 整文件（v1/v2 自动判别）。
    // EN: Whole file (automatic v1/v2 discrimination).
    let _ = houyi_persistence::fuzz_api::parse_wal_file(data);
    // v2 尾部增量解析。
    // EN: v2 tail-increment parsing.
    let _ = houyi_persistence::fuzz_api::parse_v2_tail(data);
    // collection WAL 记录（envelope + op 分派）。
    // EN: collection WAL records (envelope + op dispatch).
    let _ = houyi_persistence::collection_wal::decode_collection_wal_payload(data);
});
