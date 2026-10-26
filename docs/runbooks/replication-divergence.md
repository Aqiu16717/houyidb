# Runbook：复制日志分歧 / follower_ahead

> [English](replication-divergence.en.md)

**告警**：`increase(houyidb_replication_diverged_rejections_total[5m]) > 0`
或 `increase(houyidb_replication_follower_ahead_rejections_total[5m]) > 0`

## 症状与影响

- follower 反复重连，握手被拒；follower 保持只读。
- 含义：两端在同一 LSN 上的历史内容不同，或 follower 的日志比 leader 长。
- **这不是网络抖动**：继续重试不会自愈，必须先人工确定权威副本。

## 前 5 分钟

```bash
# 1) 确认两端复制状态与角色
curl -fsS http://<node>:17000/metrics | grep houyidb_replication_(role|lag_entries|diverged)
# 2) 抓取拒绝日志（leader 与 follower 都要看）
journalctl -u houyidb --since '15 min ago' | grep -E 'diverged|follower_ahead|fingerprint'
# 3) 记录两端 next_lsn / 日志文件大小（不要再写！）
curl -fsS http://<leader>:17000/v1/wal/status
curl -fsS http://<follower>:17000/v1/wal/status
```

## 处置决策

1. **禁止**：删除 follower 的 WAL / 用 `--reset` 类操作“让它重新开始”。
2. 判断权威侧（按数据新旧与业务确认）：
   - leader 权威：停旧 follower → 将其数据目录整体移走备份 → 用 leader 的
     备份/快照重建 follower（`full_sync` collection 必须重新全量同步）。
   - follower 权威（旧主在 promote 后仍本地写产生的分叉）：先 promote 该
     follower 或确认新主，再把旧主按 follower 重建；旧主的本地写过内容需要
     业务层比对后人工补写。
3. 重建期间保持旧目录只读备份，事故复盘需要指纹/日志证据。

## 恢复验证

- `houyidb_replication_diverged_rejections_total` 不再增长；
- follower `houyidb_replication_lag_entries` 回落到 0；
- `houyidb_replication_followers_active >= 1`（leader 侧）；
- 抽样查询：在 follower 上读得到 leader 最新写入的 id。

## 升级

涉及 full_sync collection 或无法判断权威侧 → 立即升级 DBA/架构负责人；
数据丢失风险按 P0 事故处理并保留现场。
