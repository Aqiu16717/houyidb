/**
 * stable_id：把任意字符串 id 确定性映射为 HouyiDB 的 u64 向量 id。
 *
 * 与 Python SDK（hashlib.blake2b(digest_size=8)）和服务端网关（blake2 crate）
 * 三方同算法：BLAKE2b-8 大端 & (2^63-1)。同一输入在任何语言恒得同一输出，
 * 因此 delete(ids) 可以跨 SDK 重放同一映射。
 *
 * 注意：BLAKE2b 的 digest 长度是算法参数（改变 IV），不是截断——
 * blake2b-8 ≠ blake2b-512 的前 8 字节，必须用 dkLen: 8 参数化。
 *
 * EN: stable_id: deterministically map an arbitrary string id to HouyiDB's
 * u64 vector id.
 *
 * EN: Uses the same algorithm across the Python SDK (hashlib.blake2b(
 * digest_size=8)) and the server gateway (blake2 crate): BLAKE2b-8 big-endian
 * & (2^63-1). The same input always yields the same output in any language,
 * so delete(ids) can replay the same mapping across SDKs.
 *
 * EN: Note: BLAKE2b's digest length is an algorithm parameter (it changes the
 * IV), not a truncation—blake2b-8 ≠ the first 8 bytes of blake2b-512; you
 * must parameterize with dkLen: 8.
 */

import { blake2b } from "@noble/hashes/blake2.js";

/**
 * u64 正整数键空间的上半位留空（与服务端 u64 id 对齐，避免符号位歧义）。
 * EN: The top bit of the positive u64 key space is left clear (aligned with
 * the server's u64 ids, avoiding sign-bit ambiguity).
 */
const U64_MASK = (1n << 63n) - 1n;

/**
 * 字符串 id → u64 向量 id（bigint；超出 Number.MAX_SAFE_INTEGER 的键空间必须用它）。
 * EN: String id → u64 vector id (bigint; required for a key space beyond
 * Number.MAX_SAFE_INTEGER).
 */
export function stableId(text: string): bigint {
  const digest = blake2b(new TextEncoder().encode(text), { dkLen: 8 });
  let v = 0n;
  for (const b of digest) v = (v << 8n) | BigInt(b);
  return v & U64_MASK;
}
