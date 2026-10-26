#![no_main]
//! 目标 4：pg_wire 前端消息解码。
//! EN: Target 4: pg_wire frontend message decoding.
//!
//! 断言的不变量 / Invariants:
//! EN: Invariants:
//! - 任意字节进 `decode_frontend_message` / `decode_startup_packet` /
//!   `parse_startup_message` 只返回 `Ok`/`Err`，不得 panic（长度字段来自网络：
//!   越界切片、`len+1 != buf.len()` 归为 Err；UTF-8 校验失败归为 Err）。
//! EN: - Arbitrary bytes into `decode_frontend_message` / `decode_startup_packet` /
//! EN:   `parse_startup_message` return only `Ok`/`Err`, never panic (length fields
//! EN:   come from the network: out-of-bounds slicing and `len+1 != buf.len()` are
//! EN:   Err; UTF-8 validation failures are Err).
//! - 解析器不得因超长/畸形 cstring 而无限循环或越界。
//! EN: - Parsers must not loop forever or read out of bounds on oversized or
//! EN:   malformed cstrings.
//!
//! 覆盖策略 / Coverage strategy: 长度前缀必须与缓冲自洽（`len + 1 == buf.len()`），
//! 纯字节变异会把绝大多数预算耗在长度字段上。故本目标按**合法 tag × 任意 body**
//! 组装自洽帧——让变异落在 SQL/参数/portal 名的语法上（`parse_parse_payload` /
//! `parse_bind_payload` 由此可达）。
//! EN: Coverage strategy: the length prefix must agree with the buffer
//! EN: (`len + 1 == buf.len()`); pure byte mutation would spend most of the budget
//! EN: on length fields. So this target assembles self-consistent frames from
//! EN: **legal tag × arbitrary body**, letting mutations land on SQL/parameter/
//! EN: portal-name syntax (`parse_parse_payload` / `parse_bind_payload` become
//! EN: reachable).
//!
//! 结构感知 / Structure-aware: 首字节解成 `tag`，余下字节 `take_rest()` 作 body。
//! 语料种子 = 「tag 字节 + 该 tag 的真实报文体」（如 `b'Q' ++ "SELECT 1\0"`），
//! 组装出的帧自洽且直接落到 SQL 解析路径。
//! EN: Structure-aware: the first byte decodes as `tag`; the rest becomes the body
//! EN: via `take_rest()`. Corpus seeds = "tag byte + the real message body for
//! EN: that tag" (e.g. `b'Q' ++ "SELECT 1\0"`); the assembled frame is
//! EN: self-consistent and lands directly on the SQL parse path.

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;

/// 前端消息 tag（pg_wire 简化子集）。
/// EN: Frontend message tags (a simplified pg_wire subset).
const FRONTEND_TAGS: &[u8] = &[
    b'Q', // Query
    b'P', // Parse
    b'B', // Bind
    b'E', // Execute
    b'D', // Describe
    b'C', // Close
    b'S', // Sync
    b'H', // Flush
    b'F', // FunctionCall
    b'p', // SASL / 密码消息（认证路径）
    b'X', // Terminate
];

/// 组装自洽的前端帧：`tag + len(含自身 4 字节) + body`，满足
/// `decode_frontend_message` 的 `len + 1 == buf.len()` 前置。
/// EN: Assemble a self-consistent frontend frame: `tag + len(including its own 4 bytes) +
/// EN: body`, satisfying the `len + 1 == buf.len()` precondition of
/// EN: `decode_frontend_message`.
fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(tag);
    out.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// 组装自洽的 startup packet：`len(含自身) + protocol_version + params`。
/// EN: Assemble a self-consistent startup packet: `len(including itself) +
/// EN: protocol_version + params`.
fn startup_packet(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
    out.extend_from_slice(body);
    out
}

fuzz_target!(|data: &[u8]| {
    use houyi_query::pgwire::{
        decode_frontend_message, decode_startup_packet, parse_startup_message,
    };

    let mut u = Unstructured::new(data);
    let tag = u.arbitrary::<u8>().unwrap_or(b'Q');
    let body = u.take_rest().to_vec();

    // ── 1) 原始字节路径（真实攻击面）────────────────────────────
    // EN: -- 1) Raw byte path (real attack surface) --
    let _ = decode_frontend_message(&body);
    let _ = decode_startup_packet(&body);
    let _ = parse_startup_message(&body);

    // ── 2) 合法 tag × 任意 body（自洽长度前缀）──────────────────
    // EN: -- 2) Legal tag × arbitrary body (self-consistent length prefix) --
    for &t in FRONTEND_TAGS {
        let _ = decode_frontend_message(&frame(t, &body));
    }
    let _ = decode_frontend_message(&frame(tag, &body));

    // ── 3) startup packet（协议版本 + 参数 cstring 列表）─────────
    // EN: -- 3) Startup packet (protocol version + cstring param list) --
    let _ = decode_startup_packet(&startup_packet(&body));
    // 长度字段与缓冲不符必须 Err（不得越界读）。
    // EN: A length field inconsistent with the buffer must be Err (no out-of-bounds read).
    let mut bad = startup_packet(&body);
    bad[0] ^= 0x80;
    let _ = decode_startup_packet(&bad);
});
