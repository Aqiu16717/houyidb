package houyidb

import (
	"encoding/binary"

	"golang.org/x/crypto/blake2b"
)

// u64Mask 正整数键空间的上半位留空（与服务端 u64 id 对齐，避免符号位歧义）。
// EN: u64Mask leaves the top bit of the positive u64 key space clear (aligned
// with the server's u64 ids, avoiding sign-bit ambiguity).
const u64Mask = uint64(1)<<63 - 1

// StableID 把任意字符串 id 确定性映射为 HouyiDB 的 u64 向量 id。
//
// 与 Python SDK（hashlib.blake2b(digest_size=8)）、TS SDK（@noble/hashes
// dkLen:8）和服务端网关（blake2 crate）四方同算法：BLAKE2b-8 大端 &
// (2^63-1)。同一输入在任何语言恒得同一输出，因此 delete 可跨 SDK 重放。
//
// 注意：BLAKE2b 的 digest 长度是算法参数（改变 IV），不是截断——
// blake2b.New(8, nil) ≠ blake2b.Sum512 的前 8 字节。
//
// EN: StableID deterministically maps an arbitrary string id to HouyiDB's
// u64 vector id.
//
// EN: Uses the same algorithm across the Python SDK (hashlib.blake2b(
// digest_size=8)), the TS SDK (@noble/hashes dkLen:8), and the server gateway
// (blake2 crate): BLAKE2b-8 big-endian & (2^63-1). The same input always
// yields the same output in any language, so delete can be replayed across
// SDKs.
//
// EN: Note: BLAKE2b's digest length is an algorithm parameter (it changes the
// IV), not a truncation—blake2b.New(8, nil) ≠ the first 8 bytes of
// blake2b.Sum512.
func StableID(text string) uint64 {
	// blake2b.New 仅在 size 越界时返回 error；8 是合法常量，永不失败。
	// EN: blake2b.New only returns an error when size is out of range; 8 is a
	// valid constant, so it never fails.
	h, _ := blake2b.New(8, nil)
	h.Write([]byte(text))
	return binary.BigEndian.Uint64(h.Sum(nil)) & u64Mask
}
