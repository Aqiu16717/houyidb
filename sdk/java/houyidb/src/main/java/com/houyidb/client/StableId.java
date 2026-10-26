package com.houyidb.client;

import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import org.bouncycastle.crypto.digests.Blake2bDigest;

/**
 * 把任意字符串 id 确定性映射为 HouyiDB 的 u64 向量 id。
 * EN: Deterministic string→u64 id mapping.
 *
 * <p>与 Python/TS/Go/Rust/.NET SDK、服务端 OpenAI 网关六方同算法：BLAKE2b-8 大端
 * &amp; 2^63-1。注意 digest 长度是算法参数（改变 IV 参数块），不是 blake2b-512
 * 的截断——因此同一字符串在任何语言得到同一 u64，删除可跨 SDK 重放。
 * EN: Same algorithm as the Python/TS/Go/Rust/.NET SDKs and the server OpenAI
 * gateway: BLAKE2b-8 big-endian &amp; 2^63-1. The digest length is an algorithm
 * parameter (it changes the IV parameter block), not a truncation of blake2b-512,
 * so one string maps to the same u64 in every language and deletes replay across
 * SDKs.
 *
 * <p>返回值落在 [0, 2^63-1]，Java 有符号 long 无损承载。
 * EN: The return value falls in [0, 2^63-1], carried losslessly by Java's signed long.
 */
public final class StableId {

    /**
     * u64 正整数键空间的上半位留空（与服务端 U64_MASK 同口径）。
     * EN: Leaves the upper half of the u64 positive-key space unused (same rule
     * as the server's U64_MASK).
     */
    private static final long U64_MASK = Long.MAX_VALUE; // (1L << 63) - 1

    private StableId() {}

    /**
     * 计算 text 的 stable id。
     * EN: Computes the stable id of text.
     */
    public static long of(String text) {
        // Blake2bDigest(64)：digest 长度 64 bit = 8 byte，作为算法参数写入参数块。
        // EN: Blake2bDigest(64) means a 64-bit (8-byte) digest length passed as an
        // algorithm parameter into the parameter block.
        Blake2bDigest digest = new Blake2bDigest(64);
        byte[] input = text.getBytes(StandardCharsets.UTF_8);
        digest.update(input, 0, input.length);
        byte[] out = new byte[8];
        digest.doFinal(out, 0);
        return ByteBuffer.wrap(out).getLong() & U64_MASK;
    }
}
