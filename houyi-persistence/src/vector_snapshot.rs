//! 磁盘向量快照 JSON v1：flatten main ∪ hot ∪ queue，不含 HNSW；冷启动先恢复快照再增量重放 WAL。
//! Disk vector snapshot v1: flattens main ∪ hot ∪ queue; no HNSW — rebuild via insert_batch + replay.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::encryption::{decode_key_id, encode_key_id, Encryption, KEY_ID_LEN};
use crate::vector_wal::VectorItem;

pub const VECTOR_SNAPSHOT_VERSION: u32 = 1;
/// 加密信封版本（P4.7.3）。 / EN: Encrypted envelope version (P4.7.3).
pub const VECTOR_SNAPSHOT_ENCRYPTED_VERSION: u32 = 2;
/// 加密信封魔数（明文快照以 `{` 开头，天然区分）。 / EN: Envelope magic
/// (plaintext snapshots start with `{`, so the two are unambiguous).
pub const VECTOR_SNAPSHOT_ENC_MAGIC: &[u8; 10] = b"HOUYIVECEN";
/// 加密信封头：magic + version + key id + CRC（CRC 覆盖 key id‖密文）。
/// EN: encrypted header; the CRC covers key_id‖ciphertext so a rotten key-id
/// slot reads as corruption instead of a missing key.
pub const VECTOR_SNAPSHOT_ENC_HEADER_LEN: usize = 10 + 4 + KEY_ID_LEN + 4;
/// 缺失密钥错误文案（仅供诊断；本快照的**所有**读取错误都是致命启动错误，
/// 不存在「损坏→回退」分支，因此不需要（也不提供）字符串分类函数——
/// 那正是图快照里被 OCR 批次三十五否决的模式）。
/// EN: diagnostic text only. Every vector-snapshot read error is fatal at the
/// current caller, so no string-prefix classifier is exposed.
pub const VECTOR_SNAPSHOT_MISSING_KEY: &str = "vector snapshot: missing encryption key";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VectorSnapshotV1 {
    pub v: u32,
    pub dim: usize,
    /// `l2` | `cosine`（与 `DistanceMetric` 字符串一致）.
    /// EN: `l2` or `cosine` (matches the `DistanceMetric` strings).
    pub metric: String,
    /// 与 `*.applied_lsn` 对齐：快照时刻已提交的末条 WAL LSN。
    /// EN: Aligned with `*.applied_lsn`: last committed WAL LSN at snapshot time.
    pub checkpoint_lsn: u64,
    pub items: Vec<VectorItem>,
}

/// `foo.wal` → `foo.vector_snapshot.json`（快照路径映射）。
/// EN: `foo.wal` → `foo.vector_snapshot.json` (snapshot path mapping).
pub fn snapshot_path_for_wal(wal_path: &Path) -> PathBuf {
    wal_path.with_extension("vector_snapshot.json")
}

pub fn encode_vector_snapshot_v1(
    dim: usize,
    metric: &str,
    checkpoint_lsn: u64,
    items: Vec<VectorItem>,
) -> Result<Vec<u8>, String> {
    let snap = VectorSnapshotV1 {
        v: VECTOR_SNAPSHOT_VERSION,
        dim,
        metric: metric.to_string(),
        checkpoint_lsn,
        items,
    };
    serde_json::to_vec_pretty(&snap).map_err(|e| e.to_string())
}

/// 加密编码（P4.7.3）：`encryption=None` 时与 [`encode_vector_snapshot_v1`]
/// 完全一致（明文 JSON）。
/// EN: encode with encryption; None yields the plaintext JSON unchanged.
pub fn encode_vector_snapshot_v1_with_encryption(
    dim: usize,
    metric: &str,
    checkpoint_lsn: u64,
    items: Vec<VectorItem>,
    encryption: Option<&Encryption>,
) -> Result<Vec<u8>, String> {
    let json = encode_vector_snapshot_v1(dim, metric, checkpoint_lsn, items)?;
    let Some(enc) = encryption else {
        return Ok(json);
    };
    let blob = enc.encrypt(&json).map_err(|e| e.to_string())?;
    let key_id = encode_key_id(enc.active_id()).map_err(|e| e.to_string())?;
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&key_id);
    hasher.update(&blob);
    let mut out = Vec::with_capacity(VECTOR_SNAPSHOT_ENC_HEADER_LEN + blob.len());
    out.extend_from_slice(VECTOR_SNAPSHOT_ENC_MAGIC);
    out.extend_from_slice(&VECTOR_SNAPSHOT_ENCRYPTED_VERSION.to_be_bytes());
    out.extend_from_slice(&key_id);
    out.extend_from_slice(&hasher.finalize().to_be_bytes());
    out.extend_from_slice(&blob);
    Ok(out)
}

pub fn decode_vector_snapshot_v1(bytes: &[u8]) -> Result<VectorSnapshotV1, String> {
    if bytes.is_empty() || bytes[0] != b'{' {
        return Err("vector snapshot: not JSON".to_string());
    }
    let s: VectorSnapshotV1 = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if s.v != VECTOR_SNAPSHOT_VERSION {
        return Err(format!("vector snapshot: unsupported v {}", s.v));
    }
    for it in &s.items {
        if it.values.len() != s.dim {
            return Err(format!(
                "vector snapshot: id={} len {} != dim {}",
                it.id,
                it.values.len(),
                s.dim
            ));
        }
    }
    Ok(s)
}

/// 带解密解码（P4.7.3）。缺密钥错误带 [`VECTOR_SNAPSHOT_MISSING_KEY`] 前缀。
/// EN: decode with decryption; a missing key carries the special prefix.
pub fn decode_vector_snapshot_v1_with_encryption(
    bytes: &[u8],
    encryption: Option<&Encryption>,
) -> Result<VectorSnapshotV1, String> {
    if !bytes.starts_with(VECTOR_SNAPSHOT_ENC_MAGIC) {
        return decode_vector_snapshot_v1(bytes);
    }
    if bytes.len() < VECTOR_SNAPSHOT_ENC_HEADER_LEN {
        return Err(format!(
            "vector snapshot: encrypted header truncated ({} < {VECTOR_SNAPSHOT_ENC_HEADER_LEN})",
            bytes.len()
        ));
    }
    let version = u32::from_be_bytes(bytes[10..14].try_into().unwrap());
    if version != VECTOR_SNAPSHOT_ENCRYPTED_VERSION {
        return Err(format!(
            "vector snapshot: unsupported encrypted version {version}"
        ));
    }
    let key_id = decode_key_id(&bytes[14..14 + KEY_ID_LEN])
        .map_err(|e| format!("vector snapshot: bad key id slot: {e}"))?;
    // CRC 覆盖 key id‖密文，且在查密钥之前校验：key id 槽位的位翻转必须
    // 归为损坏，而不是冒充缺密钥把启动钉死（OCR 批次三十五）。
    // EN: CRC over key_id‖ciphertext, checked before key lookup — a rotten
    // key-id slot must classify as corruption, not as a missing key.
    let crc_expected = u32::from_be_bytes(
        bytes[14 + KEY_ID_LEN..VECTOR_SNAPSHOT_ENC_HEADER_LEN]
            .try_into()
            .unwrap(),
    );
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&bytes[14..14 + KEY_ID_LEN]);
    hasher.update(&bytes[VECTOR_SNAPSHOT_ENC_HEADER_LEN..]);
    let crc_actual = hasher.finalize();
    if crc_actual != crc_expected {
        return Err(format!(
            "vector snapshot: crc32 mismatch (expected {crc_expected:#010x}, got {crc_actual:#010x})"
        ));
    }
    let enc = encryption.ok_or_else(|| {
        format!(
            "{VECTOR_SNAPSHOT_MISSING_KEY} {key_id:?} (snapshot is encrypted but no keyring is configured)"
        )
    })?;
    if !enc.has_key(&key_id) {
        return Err(format!(
            "{VECTOR_SNAPSHOT_MISSING_KEY} {key_id:?} (configure the keyring that holds it)"
        ));
    }
    let json = enc
        .decrypt(&key_id, &bytes[VECTOR_SNAPSHOT_ENC_HEADER_LEN..])
        .map_err(|e| format!("vector snapshot: decrypt failed: {e}"))?;
    decode_vector_snapshot_v1(&json)
}

pub fn write_vector_snapshot_atomic(wal_path: &Path, json_bytes: &[u8]) -> std::io::Result<()> {
    write_vector_snapshot_bytes(wal_path, json_bytes)
}

/// 带加密的原子写（P4.7.3）：`json_bytes` 是明文 JSON，加密信封在此层生成，
/// 调用方不可能忘记加密。
/// EN: atomic write with encryption — the envelope is built here so callers
/// cannot forget to encrypt.
pub fn write_vector_snapshot_atomic_with_encryption(
    wal_path: &Path,
    json_bytes: &[u8],
    encryption: Option<&Encryption>,
) -> std::io::Result<()> {
    let Some(enc) = encryption else {
        return write_vector_snapshot_bytes(wal_path, json_bytes);
    };
    let blob = enc.encrypt(json_bytes)?;
    let key_id = encode_key_id(enc.active_id())?;
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&key_id);
    hasher.update(&blob);
    let mut bytes = Vec::with_capacity(VECTOR_SNAPSHOT_ENC_HEADER_LEN + blob.len());
    bytes.extend_from_slice(VECTOR_SNAPSHOT_ENC_MAGIC);
    bytes.extend_from_slice(&VECTOR_SNAPSHOT_ENCRYPTED_VERSION.to_be_bytes());
    bytes.extend_from_slice(&key_id);
    bytes.extend_from_slice(&hasher.finalize().to_be_bytes());
    bytes.extend_from_slice(&blob);
    write_vector_snapshot_bytes(wal_path, &bytes)
}

fn write_vector_snapshot_bytes(wal_path: &Path, json_bytes: &[u8]) -> std::io::Result<()> {
    let path = snapshot_path_for_wal(wal_path);
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("vector_snapshot.json");
    let tmp = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{name}.tmp"));
    fs::write(&tmp, json_bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_snapshot_v1() {
        let items = vec![
            VectorItem {
                id: 1,
                values: vec![0.1, 0.2, 0.3, 0.4],
            },
            VectorItem {
                id: 2,
                values: vec![0.5, 0.6, 0.7, 0.8],
            },
        ];
        let bytes = encode_vector_snapshot_v1(4, "cosine", 7, items.clone()).unwrap();
        let dec = decode_vector_snapshot_v1(&bytes).unwrap();
        assert_eq!(dec.dim, 4);
        assert_eq!(dec.metric, "cosine");
        assert_eq!(dec.checkpoint_lsn, 7);
        assert_eq!(dec.items, items);
    }

    fn test_encryption(id: &str, byte: u8) -> Encryption {
        let hex = format!("{byte:02x}").repeat(crate::encryption::KEY_LEN);
        let provider =
            crate::encryption::LocalKeyProvider::parse(&format!("{id}={hex}"), None).unwrap();
        Encryption::from_provider(std::sync::Arc::new(provider)).unwrap()
    }

    #[test]
    fn encrypted_snapshot_roundtrip_and_missing_key() {
        let enc = test_encryption("k1", 4);
        let items = vec![VectorItem {
            id: 1,
            values: vec![0.1, 0.2],
        }];
        let bytes =
            encode_vector_snapshot_v1_with_encryption(2, "cosine", 3, items.clone(), Some(&enc))
                .unwrap();
        assert!(bytes.starts_with(VECTOR_SNAPSHOT_ENC_MAGIC));
        assert!(
            !bytes.windows(3).any(|w| w == b"0.1"),
            "plaintext values must not appear in the encrypted envelope"
        );
        let dec = decode_vector_snapshot_v1_with_encryption(&bytes, Some(&enc)).unwrap();
        assert_eq!(dec.items, items);

        let err = decode_vector_snapshot_v1_with_encryption(&bytes, None).unwrap_err();
        assert!(err.contains(VECTOR_SNAPSHOT_MISSING_KEY), "got: {err}");
        // keyring 只有 k2 → k1 不可得，属于缺密钥（启动失败）。
        // EN: keyring only holds k2 → k1 unavailable: missing key (fatal at startup).
        let other = test_encryption("k2", 5);
        let err = decode_vector_snapshot_v1_with_encryption(&bytes, Some(&other)).unwrap_err();
        assert!(err.contains(VECTOR_SNAPSHOT_MISSING_KEY), "got: {err}");
        // key id 槽位位翻转（仍是合法 id）→ CRC 先报错，不得冒充缺密钥。
        // EN: key-id slot bit flip (still a valid id) → CRC fails first, not missing key.
        let mut tampered = bytes.clone();
        tampered[14] ^= 0x01;
        let err = decode_vector_snapshot_v1_with_encryption(&tampered, Some(&enc)).unwrap_err();
        assert!(
            err.contains("crc32 mismatch"),
            "key-id rot must be corruption, got: {err}"
        );
        assert!(!err.contains(VECTOR_SNAPSHOT_MISSING_KEY), "got: {err}");
        // 明文快照在加密节点上仍可读。
        // EN: Plaintext snapshots remain readable on an encrypted node.
        let plain = encode_vector_snapshot_v1(2, "cosine", 3, items).unwrap();
        assert!(decode_vector_snapshot_v1_with_encryption(&plain, Some(&enc)).is_ok());
    }
}
