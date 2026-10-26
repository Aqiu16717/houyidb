# Runbook：配额拒绝 / 认证失败风暴

> [English](quota-and-auth-storm.en.md)

**告警**：
- `rate(houyidb_pgwire_quota_denials_total[5m]) > 1`
- `rate(houyidb_pgwire_auth_failures_total[5m]) > 0.2`
- `rate(houyidb_pgwire_authz_denials_total[5m]) > 0.2`

## 症状与影响

- 客户端收到 HTTP 429 / SQLSTATE 54000（配额）或 401/42501（认证/权限）。
- 配额拒绝是**背压信号**（容量/重试问题）；认证/越权拒绝是**安全信号**。
  两个计数器刻意分开，不要混为一谈。

## 前 5 分钟

```bash
curl -fsS http://127.0.0.1:17000/metrics | grep -E \
  'pgwire_(quota_denials|auth_failures|authz_denials|connections_active)'
# 日志中的主体与原因（不含 token）
journalctl -u houyidb --since '15 min ago' | grep -E \
  'quota exceeded|insufficient privilege|authentication'
# 当前主体配置（只看 name/role/collections/配额）
sed -n '/auth_principals/,$p' /etc/houyidb/config.toml | grep -E 'name|role|max_|writes_per_sec'
```

## 处置

### 配额（429 / 54000）
1. 定位主体：某个 writer 达到 `max_collections` / `max_inflight` /
   `writes_per_sec`，或客户端无退避重试放大。
2. 合法业务增长：调整该主体配额（需变更审批），或水平扩展实例。
3. 客户端 bug：要求指数退避 + 抖动；**不要**用提高全局配额掩盖重试风暴。

### 认证 / 越权（401 / 42501）
1. 区分：401 = token 未知（凭据问题/暴力尝试）；403/42501 = 已知主体越权。
2. 401 激增：检查客户端凭据轮换、是否有扫描器；必要时在入口限流。
3. 42501 激增：检查主体 `collections` 作用域是否漏配；确认不是攻击探测。
4. 不要为了“消除告警”给主体加 `*` 权限或关闭认证。

## 恢复验证

- 拒绝速率回落，正常请求成功率恢复；
- 配额调整后对应主体无 429，且集合数量符合预期；
- 认证失败无持续增长（排除扫描/暴力破解后）。

## 升级

401 来自未知外部来源或伴随异常 SQL → 安全事件；配额长期不足 →
容量规划/DBA。
