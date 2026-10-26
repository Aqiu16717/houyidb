# Runbook：内存水位 / 磁盘不足 / 快照被拒

> [English](resource-watermarks.en.md)

**告警**：
- `increase(houyidb_memory_watermark_rejected_total[5m]) > 0`
- `node_filesystem_avail_bytes{mountpoint="/data"} / node_filesystem_size_bytes < 0.1`
- `increase(houyidb_graph_snapshot_rejects_total[1h]) > 0`
- `increase(houyidb_replication_apply_failures_total[5m]) > 0`

## 症状与影响

- 内存水位：写入被拒（HTTP 503 / pgwire 53200），读仍可用。
- 磁盘不足：WAL/快照写入失败 → 可能触发 WAL poison（转 wal-poison 手册）。
- 快照被拒：冷启动会回退更早的快照或全量 WAL 重放，启动变慢但通常不丢数据。

## 前 5 分钟

```bash
df -h /data; df -i /data
curl -fsS http://127.0.0.1:17000/metrics | grep -E \
  'process_rss_bytes|memory_watermark|graph_snapshot_rejects|wal_active_bytes'
du -sh /data/* 2>/dev/null | sort -h | tail -10
# 快照拒绝原因
journalctl -u houyidb --since '2 hours ago' | grep -E 'graph snapshot|snapshot discarded'
```

## 处置

1. **内存水位**：确认 RSS 是否真超限（`houyidb_process_rss_bytes`）；
   临时降载/扩容；检查是否有异常大查询或 collection 膨胀。
   `max_memory_bytes=0` 表示禁用限流——生产应设置合理值。
2. **磁盘**：清理**安全可删**的内容（旧备份/归档副本/已迁移的明文段），
   不得直接删除活动 WAL 或最近快照；优先扩容。
3. **快照被拒**：读取日志中的 CRC/长度/密钥原因：
   - CRC/截断 → 文件损坏，保留损坏样本后让下次快照覆盖；
   - 缺密钥 → 转 [encryption-key-missing.md](encryption-key-missing.md)；
   - 恢复失败 → 检查图 payload 兼容性，必要时全量重放。
4. 若写已经失败并中毒，转 [wal-poison.md](wal-poison.md)。

## 恢复验证

- 水位拒绝计数停止增长，写入探测成功；
- `df` 可用空间 > 20%；
- 下一轮周期快照 `writes_total` 增长且 `rejects_total` 不增长；
- follower 追平（若受影响）。

## 升级

磁盘无法在 30 分钟内扩容或 WAL 已中毒 → 升级 DBA/基础设施；数据目录
任何删除动作需双人确认。
