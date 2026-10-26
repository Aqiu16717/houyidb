//! JSON 编码的向量 WAL 记录（v1）.
//! Vector WAL records as JSON (v1) for append + cold replay.
//!
//! 未知或旧版 payload（如 `bootstrap` 裸字节）在解码时跳过，保证向前兼容。

use serde::{Deserialize, Serialize};

/// 协议版本 / Protocol version for `v` field.
/// EN: Protocol version carried in the `v` field.
pub const VECTOR_WAL_VERSION: u32 = 1;

/// JSON 形如: `{"v":1,"op":"vector_upsert_batch","dim":1536,"items":[{"id":1,"values":[...]}]}`.
/// EN: JSON shape: `v` version, `op` operation, `dim` dimension, `items` payload list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VectorWalEnvelope {
    pub v: u32,
    pub op: String,
    pub dim: usize,
    pub items: Vec<VectorItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VectorItem {
    pub id: u64,
    pub values: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VectorWalUpsertBatch {
    pub dim: usize,
    pub vectors: Vec<(u64, Vec<f32>)>,
}

const OP_VECTOR_UPSERT_BATCH: &str = "vector_upsert_batch";

/// 编码一批 upsert，供 `FileWal::append`。
/// EN: Encodes a batch of upserts for `FileWal::append`.
pub fn encode_vector_upsert_batch(
    dim: usize,
    batch: &[(u64, Vec<f32>)],
) -> Result<Vec<u8>, String> {
    let items: Vec<VectorItem> = batch
        .iter()
        .map(|(id, values)| VectorItem {
            id: *id,
            values: values.clone(),
        })
        .collect();
    let env = VectorWalEnvelope {
        v: VECTOR_WAL_VERSION,
        op: OP_VECTOR_UPSERT_BATCH.to_string(),
        dim,
        items,
    };
    serde_json::to_vec(&env).map_err(|e| e.to_string())
}

/// 解码一条 WAL payload；非 JSON 或旧格式返回 `Ok(None)`。
/// EN: Decodes one WAL payload; non-JSON or legacy formats yield `Ok(None)`.
pub fn decode_vector_wal_payload(payload: &[u8]) -> Result<Option<VectorWalUpsertBatch>, String> {
    if payload.is_empty() {
        return Ok(None);
    }
    if payload[0] != b'{' {
        return Ok(None);
    }
    let env: VectorWalEnvelope = match serde_json::from_slice(payload) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    if env.v != VECTOR_WAL_VERSION {
        return Ok(None);
    }
    if env.op != OP_VECTOR_UPSERT_BATCH {
        return Ok(None);
    }
    for it in &env.items {
        if it.values.len() != env.dim {
            return Err(format!(
                "dim mismatch: item id={} len={} expected dim={}",
                it.id,
                it.values.len(),
                env.dim
            ));
        }
    }
    let vectors: Vec<(u64, Vec<f32>)> =
        env.items.into_iter().map(|it| (it.id, it.values)).collect();
    Ok(Some(VectorWalUpsertBatch {
        dim: env.dim,
        vectors,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_upsert_batch() {
        let dim = 4;
        let batch: Vec<(u64, Vec<f32>)> =
            vec![(1, vec![0.1, 0.2, 0.3, 0.4]), (2, vec![0.5, 0.6, 0.7, 0.8])];
        let bytes = encode_vector_upsert_batch(dim, &batch).unwrap();
        let dec = decode_vector_wal_payload(&bytes).unwrap().unwrap();
        assert_eq!(dec.dim, dim);
        assert_eq!(dec.vectors, batch);
    }

    #[test]
    fn legacy_bootstrap_skipped() {
        assert!(decode_vector_wal_payload(b"bootstrap").unwrap().is_none());
    }
}
