//! 复制的日志前缀指纹（P4.6.1，协议 v4；v5 起与认证并存）。
//! Log-prefix fingerprint for replication (P4.6.1, protocol v4; v5 adds auth).
//!
//! 为什么需要（WHY）：握手只比较 `resume_lsn` 的**长度**。旧主在 promote 后
//! 继续本地写、随后以与新主等长的日志重连时，`follower_ahead` 不会触发；旧主
//! 保留自己同 LSN 的记录并跳过新主的同号记录（`lsn < expected` 幂等跳过），
//! 同 LSN 上的不同内容被永久固化——P4.5.6 的 Stateright 模型给出的反例。
//! 指纹把「长度相同」升级为「长度相同**且内容相同**」。
//! EN: the handshake used to compare only `resume_lsn`. An old leader that kept
//! writing after a promote and then reconnected with an equal-length log passed
//! the check, and the follower's `lsn < expected` skip permanently froze two
//! different records at the same LSN (the P4.5.6 model counterexample). The
//! fingerprint upgrades "same length" to "same length and same content".
//!
//! 指纹定义 / Fingerprint: 对记录 `[0, upto_lsn)` 依次做链式混合
//! `h = mix(h, lsn, crc32(payload))`，空日志取种子常量。**不覆盖 payload 的
//! 每个字节**（只取 CRC）——对完整的 v2 记录，CRC 已经覆盖内容，再逐字节哈希
//! 只会让握手多扫一遍数据。CRC 碰撞概率 2^-32/条，链式后整段前缀的碰撞
//! 可以忽略；这一定位检出的是「协议级分叉」，不是恶意对抗。
//! EN: chain-mix over `(lsn, crc32(payload))`; the CRC already covers the
//! content, so re-hashing payload bytes would only double the scan.
//!
//! IO 成本（P4.6.3 OCR 修正）：单遍读 + 单遍解析。旧实现用
//! `WalTailReader::open(path, 0)` 定位 resume 偏移（整文件读一遍）再 `poll()`
//! 读第二遍——握手成本翻倍且两块文件大小的缓冲并存。现在直接整文件读入一次、
//! 复用 WAL 冷重放解析器，再在解析结果上折叠。仍是 O(file) 内存占用（与
//! `FileWal::open` 同阶）；调用方必须走 [`prefix_hash_blocking`] 把它移出
//! async worker（复制会话两侧都是 tokio task）。
//! EN: one read + one parse; callers must use `prefix_hash_blocking` so the
//! scan never runs on a tokio worker.
//!
//! 边界 / Limit: 依赖 stream 0（collections.wal）**永不截断**的 v1 约定——
//! 两侧都从 LSN 0 起哈希同一条连续历史。若将来引入 stream 0 截断，指纹必须
//! 改为「可校验的子区间」并同步升级协议版本。

use std::io::{self, Read};
use std::path::Path;

/// 空日志的指纹（FNV-1a 64 偏移基值）。
/// Fingerprint of an empty log (FNV-1a 64 offset basis).
pub const LOG_HASH_SEED: u64 = 0xCBF2_9CE4_8422_2325;

/// 把一条记录混入指纹 / Fold one record into the fingerprint.
/// EN: Folds one record into the fingerprint.
fn fold(mut h: u64, lsn: u64, payload: &[u8]) -> u64 {
    let crc = u64::from(crc32fast::hash(payload));
    h ^= lsn.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ crc;
    // xorshift64* 终混（与项目其余处的 splitmix 风格一致）。
    // EN: xorshift64* final mix (matches the project's splitmix style elsewhere).
    h = (h ^ (h >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^ (h >> 31)
}

fn non_contiguous(got: u64, expect: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("log fingerprint: non-contiguous history (got lsn {got}, expect {expect})"),
    )
}

/// 计算 `[0, upto_lsn)` 的日志指纹；`upto_lsn == 0` 返回种子。
/// Compute the fingerprint of records `[0, upto_lsn)`.
///
/// 记录必须从 LSN 0 起连续无洞且恰好 `upto_lsn` 条——不满足即 `InvalidData`：
/// 这种情形下指纹没有意义（两侧哈希的已经不是同一段历史），必须由调用方按
/// 协议错误处理，而不是算出一个「看起来正常」的数字。
/// 边界空洞（如记录 0,1,3 且 `upto_lsn == 3`）按「非连续」而非「历史过短」
/// 报告——运维读到的诊断必须指向真正的洞。
/// EN: records must be contiguous from LSN 0 and exactly `upto_lsn` long;
/// otherwise the fingerprint is meaningless and this errors out. A hole exactly
/// at the boundary is reported as non-contiguous, not as a short history.
pub fn prefix_hash(path: &Path, upto_lsn: u64) -> io::Result<u64> {
    prefix_hash_with_encryption(path, upto_lsn, None)
}

/// 带解密的指纹（P4.7.3）：v3 段先用本地 keyring 解密再哈希**明文**——
/// 两端密钥不同不影响分歧检测。
/// EN: decrypt v3 segments before hashing, so different local keys still
/// produce identical fingerprints for identical plaintext history.
pub fn prefix_hash_with_encryption(
    path: &Path,
    upto_lsn: u64,
    encryption: Option<&crate::encryption::Encryption>,
) -> io::Result<u64> {
    if upto_lsn == 0 {
        return Ok(LOG_HASH_SEED);
    }
    let mut data = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut data)?;
    let (entries, _) = crate::parse_wal_file_entries_with_len(&data, encryption)?;
    let mut h = LOG_HASH_SEED;
    let mut next = 0u64;
    for entry in &entries {
        if entry.lsn >= upto_lsn {
            if next != upto_lsn {
                // 边界洞：前缀里缺记录，紧接着的下一条已越过 upto。
                // EN: Boundary hole: a record is missing and the next one exceeds upto.
                return Err(non_contiguous(entry.lsn, next));
            }
            break;
        }
        if entry.lsn != next {
            return Err(non_contiguous(entry.lsn, next));
        }
        h = fold(h, entry.lsn, &entry.payload);
        next += 1;
    }
    if next != upto_lsn {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("log fingerprint: history shorter than resume_lsn ({next} < {upto_lsn})"),
        ));
    }
    Ok(h)
}

/// 阻塞预算：指纹扫描是整文件读 + CRC（GB 级 WAL 秒级），复制会话两侧都是
/// tokio task，必须移出 worker。全局 64 许可：重连风暴下也不至于把 blocking
/// 池打满（每次握手两侧各一次）。
/// EN: fingerprint scans are whole-file reads and must leave the async worker;
/// a shared 64-permit budget bounds reconnect storms.
#[cfg(not(loom))]
static FINGERPRINT_BUDGET: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();

/// 在阻塞池计算前缀指纹（复制握手唯一入口）。
/// Compute a prefix fingerprint on the blocking pool (the replication entry point).
#[cfg(not(loom))]
pub async fn prefix_hash_blocking(path: std::path::PathBuf, upto_lsn: u64) -> io::Result<u64> {
    prefix_hash_blocking_with_encryption(path, upto_lsn, None).await
}

/// 带解密的阻塞池指纹。 / blocking fingerprint with decryption.
/// EN: Blocking-pool fingerprint with decryption.
#[cfg(not(loom))]
pub async fn prefix_hash_blocking_with_encryption(
    path: std::path::PathBuf,
    upto_lsn: u64,
    encryption: Option<std::sync::Arc<crate::encryption::Encryption>>,
) -> io::Result<u64> {
    let sem = FINGERPRINT_BUDGET.get_or_init(|| tokio::sync::Semaphore::new(64));
    let _permit = sem
        .acquire()
        .await
        .map_err(|_| io::Error::other("fingerprint budget closed"))?;
    tokio::task::spawn_blocking(move || {
        prefix_hash_with_encryption(&path, upto_lsn, encryption.as_deref())
    })
    .await
    .map_err(|e| io::Error::other(format!("fingerprint join failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileWal, Wal};
    use std::io::Write;

    fn tmp(tag: &str) -> std::path::PathBuf {
        crate::unique_temp_path(&format!("fingerprint-{tag}"), "wal")
    }

    #[test]
    fn empty_log_is_seed() {
        let p = tmp("empty");
        let _ = std::fs::remove_file(&p);
        let _ = FileWal::open(&p).unwrap(); // 创建 v2 文件
        assert_eq!(prefix_hash(&p, 0).unwrap(), LOG_HASH_SEED);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn same_records_same_hash() {
        let (p1, p2) = (tmp("same1"), tmp("same2"));
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
        let (mut a, _) = FileWal::open(&p1).unwrap();
        let (mut b, _) = FileWal::open(&p2).unwrap();
        for payload in [b"one".as_slice(), b"two"] {
            a.append(payload.to_vec());
            b.append(payload.to_vec());
        }
        assert_eq!(prefix_hash(&p1, 2).unwrap(), prefix_hash(&p2, 2).unwrap());
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn different_content_same_length_differs() {
        let (p1, p2) = (tmp("diff1"), tmp("diff2"));
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
        let (mut a, _) = FileWal::open(&p1).unwrap();
        let (mut b, _) = FileWal::open(&p2).unwrap();
        a.append(b"same".to_vec());
        b.append(b"same".to_vec());
        a.append(b"alpha".to_vec());
        b.append(b"beta".to_vec()); // 同 LSN、不同内容
        assert_ne!(prefix_hash(&p1, 2).unwrap(), prefix_hash(&p2, 2).unwrap());
        // 只哈希共同前缀则相同。
        // EN: Hashing only the common prefix makes them equal.
        assert_eq!(prefix_hash(&p1, 1).unwrap(), prefix_hash(&p2, 1).unwrap());
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
    }

    /// P4.7.3：两端各自用**不同的本地密钥**加密同一段明文历史，
    /// 指纹必须相同——否则主备在轮换密钥/独立 keyring 后无法重连。
    /// EN: P4.7.3: both sides encrypt the same plaintext history with **different
    /// local keys**; fingerprints must match or replicas break after key rotation.
    #[test]
    fn fingerprints_match_across_different_local_keys() {
        fn test_encryption(byte: u8) -> crate::encryption::Encryption {
            let hex = format!("{byte:02x}").repeat(crate::encryption::KEY_LEN);
            let provider = crate::encryption::LocalKeyProvider::parse(&hex, None).unwrap();
            crate::encryption::Encryption::from_provider(std::sync::Arc::new(provider)).unwrap()
        }
        let (p1, p2) = (tmp("enc1"), tmp("enc2"));
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
        let enc1 = std::sync::Arc::new(test_encryption(1));
        let enc2 = std::sync::Arc::new(test_encryption(2));
        {
            let (mut a, _) = FileWal::open_with_encryption(&p1, Some(enc1.clone())).unwrap();
            let (mut b, _) = FileWal::open_with_encryption(&p2, Some(enc2.clone())).unwrap();
            for payload in [b"alpha".as_slice(), b"beta"] {
                a.append(payload.to_vec());
                b.append(payload.to_vec());
            }
        }
        // 磁盘上的密文必然不同（随机 nonce + 不同密钥）。
        // EN: On-disk ciphertexts necessarily differ (random nonce + different keys).
        assert_ne!(
            std::fs::read(&p1).unwrap(),
            std::fs::read(&p2).unwrap(),
            "ciphertext must differ"
        );
        // 指纹按明文计算，两端一致。
        // EN: Fingerprints are computed over plaintext, so both sides match.
        assert_eq!(
            prefix_hash_with_encryption(&p1, 2, Some(enc1.as_ref())).unwrap(),
            prefix_hash_with_encryption(&p2, 2, Some(enc2.as_ref())).unwrap()
        );
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn shorter_history_is_an_error() {
        let p = tmp("short");
        let _ = std::fs::remove_file(&p);
        let (mut w, _) = FileWal::open(&p).unwrap();
        w.append(b"only".to_vec());
        assert!(prefix_hash(&p, 2).is_err());
        let _ = std::fs::remove_file(&p);
    }

    /// OCR 批次二十七：无洞测试覆盖——回归会让「同 LSN 不同内容」的防线失效。
    /// 手工写一条 LSN 跳跃的记录（FileWal 的 append_entry 拒绝 LSN 洞，
    /// 只能绕过写入侧直接构造磁盘字节）。
    /// EN: a skipped-LSN record is written directly (the appender rejects gaps).
    #[test]
    fn lsn_hole_is_an_error() {
        let p = tmp("hole");
        let _ = std::fs::remove_file(&p);
        {
            let (mut w, _) = FileWal::open(&p).unwrap();
            w.append(b"zero".to_vec());
        }
        {
            // 追加一条 LSN=3 的 v2 记录：历史 0,3 —— 在 1 处有洞。
            // EN: Append an LSN=3 v2 record: history is 0,3 — a hole at 1.
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(&crate::encode_record(3, b"three", crate::WalFormat::V2, None).unwrap())
                .unwrap();
        }
        let err = prefix_hash(&p, 4).unwrap_err();
        assert!(
            err.to_string().contains("non-contiguous"),
            "hole must be classified as non-contiguous, got: {err}"
        );
        // 边界洞（upto == 下一条记录 LSN）同样按非连续报告。
        // EN: A boundary hole (upto == next record's LSN) is non-contiguous too.
        let err = prefix_hash(&p, 3).unwrap_err();
        assert!(
            err.to_string().contains("non-contiguous"),
            "boundary hole must be non-contiguous, got: {err}"
        );
        let _ = std::fs::remove_file(&p);
    }
}
