# Runbook：审计链校验失败 / 审计写错误

> [English](audit-chain-broken.en.md)

**告警**：`increase(houyidb_audit_write_errors_total[5m]) > 0`
或 `GET /v1/admin/audit/verify` 返回失败（启动时链断也会拒绝启动）。

## 症状与影响

- 审计追加失败后日志进入 poison 态（fail-closed），管理操作可能被拒绝。
- 哈希链断裂意味着审计记录可能被篡改/删除/截断——属安全事件，不是普通运维
  故障。

## 前 5 分钟

```bash
# 1) 校验端点（需要 admin token）
curl -fsS -H "Authorization: Bearer $ADMIN_TOKEN" \
  http://127.0.0.1:17000/v1/admin/audit/verify
# 2) 审计段与磁盘
ls -l /data/audit.log*; df -h /data; df -i /data
stat -c '%a %U:%G' /data/audit.log
# 3) 首次失败日志
journalctl -u houyidb --since '1 hour ago' | grep -iE 'audit|hash chain|poison'
```

## 处置

1. **禁止**：删除审计段、手工续写哈希、修改 `prev_hash` 让校验通过。
2. 磁盘满/权限错误：先扩容或修复权限；保留现场副本。
3. 链断裂：
   - 立即停止对该节点的管理写操作；
   - 复制全部审计段到只读证据目录（含校验输出）；
   - 升级安全负责人，按审计事件流程决定是否从已知良好边界重建审计流
     （旧段永久保留，不能删除）。
4. 审计 append 失败导致 poison：根因修复后重启；重启会重新校验链。

## 恢复验证

- `/v1/admin/audit/verify` 返回成功且条目数连续；
- `houyidb_audit_write_errors_total` 停止增长；
- 新的管理操作产生连续的新审计条目。

## 升级

任何非磁盘/权限原因导致的链断 → 安全事件（P0），需要法务/合规知会。
