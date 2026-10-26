//! WAL 尾部读取器——复制 leader session 的 catch-up / 增量推送数据源。
//! WAL tail reader — catch-up and incremental shipping source for replication
//! leader sessions.
//!
//! 设计要点 / Design:
//! - **独立只读 fd**：与 writer（FileWal append 句柄）各持文件描述符，
//!   按 offset 增量读，互不干扰；无需锁。
//! - **resume 定位 O(file) 全扫**：open 时解析整文件找到首条 `lsn >= resume_lsn`
//!   的记录偏移。collections.wal 无截断（完整 append-only 历史），v1 接受
//!   全量扫描的内存与时间代价并文档化；稀疏 LSN 索引留待后续。
//! - **撕裂尾停等下轮**：poll 撞见在途写的半条记录（writer 单次 write_all
//!   的可见性窗口）不是腐坏——静默停在最后有效边界，下轮 poll 重读。
//!   复用 `parse_v2_records_tail`（quiet）/ `parse_v1_records`，杜绝第二处
//!   手写解码（v1 骨架的 LE/BE 分裂教训）。
//! - **文件缩小即错**：stream 0（collections.wal）无截断语义，文件长度 <
//!   已消费 offset 只能是截断事故 → Err，由 session 层断连处理。注意检测
//!   基于持有 fd 的 fstat，仅覆盖同 inode 截断；rename 整体替换对本 fd
//!   不可见（旧 inode 数据照读），该场景由 follower 侧 LSN 幂等层兜底。
//! - **CRC 不匹配即错**：完整记录的校验失败是腐坏而非撕裂（撕裂只会呈现为
//!   头/payload 不完整、静默停等下轮），tail 模式显式 Err——静默混同会让
//!   reader 在腐坏点无声停摆（leader 继续追加，follower 永远追不上且无告警）。

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use crate::encryption::Encryption;
use crate::{
    parse_v1_records, parse_v2_records_tail, parse_v2_records_tail_with_cipher, WalEntry,
    WalFormat, WAL_ENCRYPTED_HEADER_LEN, WAL_FORMAT_VERSION, WAL_FORMAT_VERSION_ENCRYPTED,
    WAL_HEADER_LEN, WAL_MAGIC,
};

pub struct WalTailReader {
    file: File,
    /// 下一条待解析记录的文件偏移（已消费前缀 = [data_start, offset)）。
    /// EN: File offset of the next record to parse (consumed prefix = [data_start, offset)).
    offset: u64,
    /// 活动文件格式。 / Active segment format.
    /// EN: Active segment format.
    format: WalFormat,
    /// v3 解密句柄与文件头里的 key id。 / Decryption for v3 segments.
    /// EN: Decryption handle for v3 segments plus the key id from the file header.
    encryption: Option<Arc<Encryption>>,
    key_id: Option<String>,
}

impl WalTailReader {
    /// 打开 `path` 并定位到首条 `lsn >= resume_lsn` 的记录（O(file) 全扫）。
    ///
    /// `resume_lsn` 语义 = follower 的下一条期望 LSN（本地 next_lsn）。
    /// 全部记录均 < resume_lsn 时定位于有效前缀末尾——后续追加经 poll 接续，
    /// 正好覆盖「follower 已追平，等 live 推送」的场景。撕裂尾停在有效边界，
    /// 在途记录由后续 poll 补读。
    /// EN: Opens `path` and seeks to the first record with `lsn >= resume_lsn`
    /// (O(file) full scan). `resume_lsn` is the follower's next expected LSN
    /// (local next_lsn). When every record is < resume_lsn, position lands at
    /// the end of the valid prefix — later appends are picked up by poll,
    /// covering "follower caught up, waiting for live push". A torn tail stops
    /// at the valid boundary; in-flight records are read by later polls.
    pub fn open(path: &Path, resume_lsn: u64) -> io::Result<Self> {
        Self::open_with_encryption(path, resume_lsn, None)
    }

    /// 带解密句柄打开（P4.7.3）：v3 段必须提供 keyring。
    /// EN: open with decryption; v3 segments require the keyring.
    pub fn open_with_encryption(
        path: &Path,
        resume_lsn: u64,
        encryption: Option<Arc<Encryption>>,
    ) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();

        // 格式识别与 FileWal::open 同级：≥12 字节按魔数判定；更小的文件不
        // 可能是有效数据（FileWal 对同一情形判「撕裂残屑」并重写为 v2）。
        // tail reader 只读不能重写，且 v1 单条记录至少 12 字节——按 v1 解析
        // 只会得出空记录集并把 v2 标志错误固化，故显式报错（启动顺序保证
        // leader 监听前 WAL 头已落盘，此分支仅覆盖事故现场）。
        // EN: Format detection matches FileWal::open: >=12 bytes is decided by
        // magic; smaller files cannot hold valid data (FileWal treats the same
        // case as torn debris and rewrites v2). The tail reader is read-only and
        // v1 records are at least 12 bytes, so parsing as v1 would yield an empty
        // set and wrongly pin v2=false — hence an explicit error (startup order
        // guarantees the header is durable before the leader listens; this
        // branch only covers incident scenes).
        if file_len < WAL_HEADER_LEN as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "wal file too short: {file_len} < {WAL_HEADER_LEN} (torn debris, not tail-able)"
                ),
            ));
        }
        let (format, data_start, key_id) = {
            let mut head = [0u8; WAL_HEADER_LEN];
            file.read_exact(&mut head)?;
            if &head[..8] == WAL_MAGIC {
                let version = u32::from_be_bytes(head[8..12].try_into().unwrap());
                match version {
                    WAL_FORMAT_VERSION => (WalFormat::V2, WAL_HEADER_LEN as u64, None),
                    WAL_FORMAT_VERSION_ENCRYPTED => {
                        if file_len < WAL_ENCRYPTED_HEADER_LEN as u64 {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "encrypted wal header truncated",
                            ));
                        }
                        let mut slot = [0u8; crate::encryption::KEY_ID_LEN];
                        file.seek(SeekFrom::Start(WAL_HEADER_LEN as u64))?;
                        file.read_exact(&mut slot)?;
                        let key_id = crate::encryption::decode_key_id(&slot).map_err(|e| {
                            io::Error::new(io::ErrorKind::InvalidData, e.to_string())
                        })?;
                        let enc = encryption.as_ref().ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "encrypted WAL segment requires an encryption keyring",
                            )
                        })?;
                        if !enc.has_key(&key_id) {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("encrypted WAL segment uses unknown key id {key_id:?}"),
                            ));
                        }
                        (WalFormat::V3, WAL_ENCRYPTED_HEADER_LEN as u64, Some(key_id))
                    }
                    other => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "unsupported wal format version {other} (expect \
                                 {WAL_FORMAT_VERSION} or {WAL_FORMAT_VERSION_ENCRYPTED})"
                            ),
                        ))
                    }
                }
            } else {
                (WalFormat::V1, 0, None)
            }
        };

        // resume 定位：整段读入并完整校验解析（CRC 验证过的偏移才可信——
        // 只走记录头不验 CRC 的轻量扫描会被腐坏的 len 字段带偏，把 follower
        // 卡在错误偏移上；v1 接受内存代价换取「定位即可信」）。按已知长度
        // 预分配，免 read_to_end 在 GB 级 WAL 上的倍增重拷链。
        // EN: Resume positioning reads the whole segment and fully validates it
        // (only CRC-verified offsets are trustworthy — a header-only scan can be
        // misled by a corrupt len field and pin the follower at a wrong offset;
        // v1 accepts the memory cost for trustworthy positioning). Preallocate
        // from the known length to avoid read_to_end's doubling copies on GB WALs.
        file.seek(SeekFrom::Start(data_start))?;
        let mut buf = Vec::with_capacity((file_len - data_start) as usize);
        file.read_to_end(&mut buf)?;
        let (entries, consumed) = match (&format, &encryption, &key_id) {
            (WalFormat::V3, Some(enc), Some(key)) => {
                parse_v2_records_tail_with_cipher(&buf, enc, key)?
            }
            // v3 缺 cipher 是构造期不变量的破坏：绝不能落到 v1 解析器把密文
            // 当明文读成垃圾记录（OCR 批次三十五）。
            // EN: V3 without a cipher must fail loudly instead of being parsed
            // as v1 records.
            (WalFormat::V3, _, _) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "encrypted WAL segment requires an encryption handle and key id",
                ))
            }
            (WalFormat::V2, _, _) => parse_v2_records_tail(&buf)?,
            (WalFormat::V1, _, _) => parse_v1_records(&buf),
        };

        // v3 磁盘记录 = 16B 头 + 12B nonce + 明文 + 16B tag。
        // EN: v3 on-disk record = 16B header + 12B nonce + plaintext + 16B tag.
        let record_overhead = match format {
            WalFormat::V1 => 12u64,
            WalFormat::V2 => 16u64,
            WalFormat::V3 => 16 + 12 + 16,
        };
        let mut offset = data_start;
        for e in &entries {
            if e.lsn >= resume_lsn {
                break;
            }
            offset += record_overhead + e.payload.len() as u64;
        }
        if entries.last().map(|e| e.lsn < resume_lsn).unwrap_or(true) {
            // 未 break：所有记录均 < resume_lsn（或文件为空/撕裂尾在前），
            // 定位到有效前缀末尾，等待 live 追加。
            // EN: No break: every record is < resume_lsn (or the file is empty /
            // the torn tail comes first); position at the valid prefix end and
            // wait for live appends.
            offset = data_start + consumed as u64;
        }

        file.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            file,
            offset,
            format,
            encryption,
            key_id,
        })
    }

    /// 增量读取自上次消费位以来的新记录；无新数据返回空 Vec。
    /// 撕裂尾（在途写）静默停等下轮；文件缩小（截断/替换事故）返回 Err。
    /// EN: Incrementally reads records appended since the last consumed offset;
    /// returns an empty Vec when there is no new data. A torn tail (write in
    /// flight) quietly waits for the next round; a shrunken file (truncation or
    /// replacement incident) returns Err.
    pub fn poll(&mut self) -> io::Result<Vec<WalEntry>> {
        let file_len = self.file.metadata()?.len();
        if file_len < self.offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "wal shrank below consumed offset: len {file_len} < offset {}",
                    self.offset
                ),
            ));
        }
        if file_len == self.offset {
            return Ok(Vec::new());
        }
        self.file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::with_capacity((file_len - self.offset) as usize);
        self.file.read_to_end(&mut buf)?;
        let (entries, consumed) = match (&self.format, &self.encryption, &self.key_id) {
            (WalFormat::V3, Some(enc), Some(key)) => {
                parse_v2_records_tail_with_cipher(&buf, enc, key)?
            }
            (WalFormat::V3, _, _) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "encrypted WAL segment requires an encryption handle and key id",
                ))
            }
            (WalFormat::V2, _, _) => parse_v2_records_tail(&buf)?,
            (WalFormat::V1, _, _) => parse_v1_records(&buf),
        };
        self.offset += consumed as u64;
        Ok(entries)
    }

    /// 当前消费偏移（测试与 session 观测用）。
    /// EN: Current consumed offset (for tests and session observation).
    pub fn offset(&self) -> u64 {
        self.offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileWal, Wal};

    fn temp_wal_path() -> std::path::PathBuf {
        crate::unique_temp_path("houyi-waltail-test", "wal")
    }

    /// 手工编码 v2 记录（含正确 CRC），用于撕裂尾补全场景。
    /// EN: Hand-encodes a v2 record (with correct CRC) for torn-tail completion tests.
    fn v2_record_bytes(lsn: u64, payload: &[u8]) -> Vec<u8> {
        let mut h = crc32fast::Hasher::new();
        h.update(&lsn.to_be_bytes());
        h.update(&(payload.len() as u32).to_be_bytes());
        h.update(payload);
        let crc = h.finalize();
        let mut r = Vec::new();
        r.extend_from_slice(&lsn.to_be_bytes());
        r.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        r.extend_from_slice(&crc.to_be_bytes());
        r.extend_from_slice(payload);
        r
    }

    #[test]
    fn open_from_zero_reads_all_records() {
        let path = temp_wal_path();
        {
            let (mut wal, _) = FileWal::open(&path).unwrap();
            let _ = wal.append(b"alpha".to_vec());
            let _ = wal.append(b"beta".to_vec());
            let _ = wal.append(b"gamma".to_vec());
        }
        let mut tail = WalTailReader::open(&path, 0).unwrap();
        let entries = tail.poll().unwrap();
        let payloads: Vec<&[u8]> = entries.iter().map(|e| e.payload.as_slice()).collect();
        assert_eq!(
            payloads,
            vec![b"alpha".as_slice(), b"beta".as_slice(), b"gamma".as_slice()]
        );
        assert_eq!(entries[0].lsn, 0);
        assert_eq!(entries[2].lsn, 2);
        // 再 poll 无新数据
        // EN: Polling again yields no new data.
        assert!(tail.poll().unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_resumes_from_first_lsn_ge_resume() {
        let path = temp_wal_path();
        {
            let (mut wal, _) = FileWal::open(&path).unwrap();
            for i in 0..5u8 {
                let _ = wal.append(vec![i]);
            }
        }
        let mut tail = WalTailReader::open(&path, 3).unwrap();
        let entries = tail.poll().unwrap();
        let lsns: Vec<u64> = entries.iter().map(|e| e.lsn).collect();
        assert_eq!(lsns, vec![3, 4]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_caught_up_positions_at_end_then_picks_up_appends() {
        let path = temp_wal_path();
        let mut wal = {
            let (mut wal, _) = FileWal::open(&path).unwrap();
            let _ = wal.append(b"a".to_vec());
            let _ = wal.append(b"b".to_vec());
            wal
        };
        // resume_lsn = 2（next_lsn）→ 已追平，定位到文件末尾
        // EN: resume_lsn = 2 (next_lsn) → already caught up, positioned at end of file.
        let mut tail = WalTailReader::open(&path, 2).unwrap();
        assert!(tail.poll().unwrap().is_empty());
        // leader 继续写入（同进程另一句柄，模拟 wal worker）
        // EN: The leader keeps writing (another handle in this process, mimicking the wal worker).
        let _ = wal.append(b"c".to_vec());
        let entries = tail.poll().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].lsn, 2);
        assert_eq!(entries[0].payload, b"c");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn torn_tail_waits_then_reads_completed_record() {
        let path = temp_wal_path();
        {
            let (mut wal, _) = FileWal::open(&path).unwrap();
            let _ = wal.append(b"good".to_vec());
        }
        // 撕裂：只写记录的前 10 字节（lsn8 + len 前 2 字节）
        // EN: Torn write: only the first 10 bytes (lsn8 + first 2 length bytes).
        let full = v2_record_bytes(1, b"late");
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(&full[..10]).unwrap();
        }
        let mut tail = WalTailReader::open(&path, 0).unwrap();
        let entries = tail.poll().unwrap();
        assert_eq!(entries.len(), 1, "torn record must wait");
        assert_eq!(entries[0].payload, b"good");
        let offset_at_torn = tail.offset();
        assert!(tail.poll().unwrap().is_empty(), "still torn, no progress");
        assert_eq!(
            tail.offset(),
            offset_at_torn,
            "offset must not advance past torn record"
        );
        // writer 补全剩余字节
        // EN: The writer completes the remaining bytes.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(&full[10..]).unwrap();
        }
        let entries = tail.poll().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].lsn, 1);
        assert_eq!(entries[0].payload, b"late");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn v1_legacy_file_auto_detected() {
        let path = temp_wal_path();
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&path).unwrap();
            for (lsn, p) in [(0u64, b"old-a".as_slice()), (1, b"old-b"), (2, b"old-c")] {
                f.write_all(&lsn.to_be_bytes()).unwrap();
                f.write_all(&(p.len() as u32).to_be_bytes()).unwrap();
                f.write_all(p).unwrap();
            }
        }
        let mut tail = WalTailReader::open(&path, 1).unwrap();
        let entries = tail.poll().unwrap();
        let lsns: Vec<u64> = entries.iter().map(|e| e.lsn).collect();
        assert_eq!(lsns, vec![1, 2]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_error() {
        let path = temp_wal_path();
        let _ = std::fs::remove_file(&path);
        assert!(WalTailReader::open(&path, 0).is_err());
    }

    #[test]
    fn short_file_is_error() {
        // <12 字节：FileWal 判撕裂残屑重写为 v2；tail 只读不能重写，显式报错
        // （按 v1 解析会把 v2=false 错误固化，后续 poll 全乱）。
        // EN: <12 bytes: FileWal treats this as torn debris and rewrites v2; the
        // read-only tail cannot rewrite, so it errors explicitly (parsing as v1
        // would pin v2=false and derail every later poll).
        let path = temp_wal_path();
        std::fs::write(&path, b"tiny").unwrap();
        let err = WalTailReader::open(&path, 0).err().unwrap();
        assert!(err.to_string().contains("too short"), "got: {err}");
        let _ = std::fs::remove_file(&path);
    }

    /// 完整记录 CRC 不匹配 = 腐坏（非撕裂）：open 的全扫与 poll 的增量喂法
    /// 都必须显式 Err——静默停等会让 follower 在腐坏点永远追不上且无告警。
    /// EN: A CRC mismatch on a complete record means corruption (not a torn
    /// tail): both open's full scan and poll's incremental feed must return an
    /// explicit Err — silently waiting would leave the follower stuck at the
    /// corrupt point forever with no alert.
    #[test]
    fn corrupt_record_is_error_not_silent_stall() {
        let path = temp_wal_path();
        {
            let (mut wal, _) = FileWal::open(&path).unwrap();
            let _ = wal.append(b"good".to_vec());
        }
        // 追加一条 CRC 故意写错的完整记录
        // EN: Append a complete record with a deliberately wrong CRC.
        let mut bad = v2_record_bytes(1, b"rotten");
        bad[12] ^= 0xFF; // crc 字段打翻（v2 记录布局：lsn8 + len4 + crc4 + payload）
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(&bad).unwrap();
        }
        // open 全扫：腐坏在中段 → Err
        // EN: open's full scan sees corruption mid-file → Err.
        let err = WalTailReader::open(&path, 0).err().unwrap();
        assert!(err.to_string().contains("checksum mismatch"), "got: {err}");
        let _ = std::fs::remove_file(&path);

        // poll 增量：open 时文件干净，随后腐坏记录落盘 → poll Err
        // EN: poll incremental: the file was clean at open, then a corrupt record lands → Err.
        let path = temp_wal_path();
        {
            let (mut wal, _) = FileWal::open(&path).unwrap();
            let _ = wal.append(b"good".to_vec());
        }
        let mut tail = WalTailReader::open(&path, 0).unwrap();
        assert_eq!(tail.poll().unwrap().len(), 1);
        let mut bad = v2_record_bytes(1, b"rotten");
        bad[12] ^= 0xFF;
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(&bad).unwrap();
        }
        let err = tail.poll().unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "got: {err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn shrunk_file_is_error() {
        let path = temp_wal_path();
        {
            let (mut wal, _) = FileWal::open(&path).unwrap();
            let _ = wal.append(b"x".to_vec());
        }
        let mut tail = WalTailReader::open(&path, 0).unwrap();
        assert_eq!(tail.poll().unwrap().len(), 1);
        // 外部截断（事故场景）
        // EN: External truncation (incident scenario).
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(0).unwrap();
        drop(f);
        let err = tail.poll().unwrap_err();
        assert!(err.to_string().contains("wal shrank"));
        let _ = std::fs::remove_file(&path);
    }
}
