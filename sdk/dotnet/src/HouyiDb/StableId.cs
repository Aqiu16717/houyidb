using System.Buffers.Binary;
using System.Text;
using Konscious.Security.Cryptography;

namespace HouyiDb;

/// <summary>
/// 把任意字符串 id 确定性映射为 HouyiDB 的 u64 向量 id。
/// EN: Deterministic string→u64 id mapping.
/// </summary>
/// <remarks>
/// 与 Python/TS/Go/Rust SDK、服务端 OpenAI 网关五方同算法：BLAKE2b-8 大端
/// &amp; 2^63-1。注意 digest 长度是算法参数（改变 IV 参数块），不是 blake2b-512
/// 的截断——因此同一字符串在任何语言得到同一 u64，删除可跨 SDK 重放。
/// EN: Same algorithm as the Python/TS/Go/Rust SDKs and the server OpenAI
/// gateway: BLAKE2b-8 big-endian &amp; 2^63-1. The digest length is an algorithm
/// parameter (it changes the IV parameter block), not a truncation of blake2b-512,
/// so one string maps to the same u64 in every language and deletes replay across
/// SDKs.
/// </remarks>
public static class StableId
{
    /// <summary>
    /// u64 正整数键空间的上半位留空（与服务端 U64_MASK 同口径）。
    /// EN: Leaves the upper half of the u64 positive-key space unused (same rule
    /// as the server's U64_MASK).
    /// </summary>
    private const ulong U64Mask = (1UL << 63) - 1;

    /// <summary>
    /// 计算 text 的 stable id。
    /// EN: Computes the stable id of text.
    /// </summary>
    public static ulong Of(string text)
    {
        // HMACBlake2B 无 key 构造即标准 BLAKE2b；64 bit = 8 byte 的 digest 长度
        // 作为算法参数写入参数块（改变 IV），不是 blake2b-512 的截断。
        // EN: HMACBlake2B with no key is standard BLAKE2b; the 64-bit (8-byte)
        // digest length is passed as an algorithm parameter into the parameter
        // block (changing the IV), not a truncation of blake2b-512.
        using var hasher = new HMACBlake2B(64);
        var digest = hasher.ComputeHash(Encoding.UTF8.GetBytes(text));
        return BinaryPrimitives.ReadUInt64BigEndian(digest) & U64Mask;
    }
}
