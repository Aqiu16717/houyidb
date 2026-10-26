# Runbook：加密快照 / WAL 缺密钥导致启动失败

> [English](encryption-key-missing.en.md)

**触发**：启动失败，日志包含
`graph snapshot: missing encryption key`、`vector snapshot: missing encryption key`
或 WAL v3 解密失败；进程未监听端口。

## 症状与影响

- 节点完全不可用（fail-closed，这是设计行为）。
- 常见原因：keyring 文件未挂载、`active=` 指向不存在的 key、轮换后删除了旧 key。
- **绝不能**为了启动而关闭加密：那会让新写入落回明文，且旧数据仍不可读。

## 前 5 分钟

```bash
# 1) 日志里的 key id 与配置的 keyring
journalctl -u houyidb --since '10 min ago' | grep -E 'missing encryption key|key id|keyring'
grep -E 'encryption_key_file|encryption_key_id' /etc/houyidb/config.toml 2>/dev/null
ls -l /data/keyring; stat -c '%a %U:%G' /data/keyring
# 2) keyring 内容（只看 id，不打印密钥）
grep -E '^(active=|[A-Za-z0-9_.-]+=)' /data/keyring | cut -d= -f1
```

## 处置

1. 恢复正确的 keyring 文件/挂载；确认快照/WAL 里的 key id 在 keyring 中存在。
2. 轮换后缺旧 key：从密钥备份恢复旧 key（**保留**，不要替换掉新 key），
   重启；旧段读取成功后再考虑清理。
3. keyring 损坏且无备份：数据不可恢复——立即升级安全负责人，按数据丢失事故
   处理；禁止尝试暴力/伪造密钥。
4. 恢复后检查文件权限：`chmod 600 /data/keyring`，属主为服务账号。

## 恢复验证

- 启动日志出现 `encryption at rest enabled`（active key 正确）；
- `/readyz` 通过；
- 读取一条历史数据（跨轮换前的快照/WAL）成功；
- 新写入后确认活动 WAL 段头 key id 为当前 active key。

## 升级

密钥丢失/疑似泄漏 → 安全事件流程（P0）；同时评估是否轮换所有受影响数据的
加密密钥并重建副本。
