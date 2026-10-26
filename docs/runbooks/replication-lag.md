# Runbook：复制滞后 / 慢 follower 被踢

> [English](replication-lag.en.md)

**告警**：`houyidb_replication_lag_entries > 10000` 持续 10 分钟；
或 `increase(houyidb_replication_slow_follower_kicks_total[10m]) > 0`

## 症状与影响

- follower 的已接收 LSN 落后 leader；semi_sync/full_sync 写可能因等待超时
  降级为 async（`houyidb_replication_sync_timeouts_total` 上升）。
- follower 队列积满会被踢出（保护 WAL 写线程与其他 follower），随后自动重连
  catch-up；数据不一定丢失，但 RPO 窗口扩大。

## 前 5 分钟

```bash
curl -fsS http://<leader>:17000/metrics | grep -E \
  'replication_(lag_entries|sync_timeouts_total|slow_follower_kicks_total|followers_active|shipped_batches_total)'
curl -fsS http://<follower>:17000/metrics | grep -E \
  'replication_(recv_lsn|durable_lsn|apply_failures_total|reconnects_total|fsync_total)'
# 网络与磁盘
ping -c 3 <follower>; ssh <follower> 'df -h /data; iostat -x 1 3'
```

## 处置

1. 先判断是**带宽/磁盘**还是**apply 阻塞**：
   - follower `apply_failures_total` / `apply_retries_total` 增长 → 转
     [wal-poison.md](wal-poison.md) 检查 apply 错误与磁盘。
   - follower 磁盘 IO 饱和、`df` 接近满 → 扩容或清理归档（不得删除活动 WAL）。
   - 网络丢包/延迟 → 修网络；队列深度 `replication_queue_batches` 不应为了
     追平而盲目调大（会拖累 WAL 写线程内存）。
2. **不要**在 follower 上执行写操作或 manual promote 来“省事”。
3. 被踢是保护机制：确认 follower 恢复后会自动 catch-up；若长期反复被踢，
   降低写入速率或提升 follower 配置。

## 恢复验证

- `lag_entries` 持续下降并归零；
- `slow_follower_kicks_total` 停止增长；
- `sync_timeouts_total` 停止增长，semi/full_sync 写延迟恢复。

## 升级

滞后超过 30 分钟或 RPO 风险不可接受 → 升级 DBA，评估临时降级（async）
与补写策略。
