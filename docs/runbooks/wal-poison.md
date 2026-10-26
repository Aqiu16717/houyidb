# Runbook：WAL 中毒（写入失败后拒绝写）

> [English](wal-poison.en.md)

**触发**：日志出现 `wal poisoned by write failure; all further writes rejected`
（可配 Loki 规则；Prometheus 侧表现为写请求持续 5xx、`wal` 指标停更）

## 症状与影响

- 第一次 fsync/write 失败后，WAL 句柄进入毒化态：所有后续写立即失败，
  避免在撕裂日志上继续追加造成不可恢复的洞。
- 读请求仍可用；复制 follower 可能断流。

## 前 5 分钟

```bash
# 1) 找到根因错误（首条 poison 日志带 op 与 io error）
journalctl -u houyidb --since '30 min ago' | grep -A2 'wal poisoned'
# 2) 磁盘/文件系统
df -h /data; df -i /data; dmesg | tail -30
# 3) 权限与只读挂载
mount | grep ' /data '; ls -l /data/*.wal
# 4) 复制状态（确认 follower 是否还能追平）
curl -fsS http://127.0.0.1:17000/metrics | grep houyidb_replication_
```

## 处置

1. **禁止**：手工截断/编辑活动 WAL、删除 `.wal` 文件、跳过 fsync 重启。
2. 修复根因：扩容磁盘/清理安全可删的归档、修复只读挂载/权限、处理 EIO。
3. 根因消除后**重启节点**（毒化态只在 reopen 时清除）。重启前把损坏窗口的
   日志文件复制到事故目录备查。
4. 若重启后仍立即中毒：停止尝试，转 [replication-divergence.md](replication-divergence.md)
   评估从副本重建；不要让两个节点都带病运行。

## 恢复验证

- 重启后 `/readyz` 通过；
- 写入探测（一个 collection upsert）成功；
- 冷启动日志无 `truncating torn tail` 之外的异常；
- follower `lag_entries` 归零。

## 升级

任何疑似数据丢失（重启后记录数少于写侧账本）→ P0，保留 WAL 与快照，
升级 DBA/存储负责人。
