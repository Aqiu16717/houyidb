# Runbook: Quota Rejection / Authentication Failure Storm

> [中文](quota-and-auth-storm.md)

**Alerts**:
- `rate(houyidb_pgwire_quota_denials_total[5m]) > 1`
- `rate(houyidb_pgwire_auth_failures_total[5m]) > 0.2`
- `rate(houyidb_pgwire_authz_denials_total[5m]) > 0.2`

## Symptoms and Impact

- Clients receive HTTP 429 / SQLSTATE 54000 (quota) or 401/42501 (authentication/authorization).
- Quota rejections are a **backpressure signal** (capacity/retry issues); authentication/authorization rejections are a **security signal**.
  The two counters are deliberately kept separate; do not conflate them.

## First 5 Minutes

```bash
curl -fsS http://127.0.0.1:17000/metrics | grep -E \
  'pgwire_(quota_denials|auth_failures|authz_denials|connections_active)'
# Principal and reason in the logs (excluding tokens)
journalctl -u houyidb --since '15 min ago' | grep -E \
  'quota exceeded|insufficient privilege|authentication'
# Current principal configuration (name/role/collections/quota only)
sed -n '/auth_principals/,$p' /etc/houyidb/config.toml | grep -E 'name|role|max_|writes_per_sec'
```

## Handling

### Quota (429 / 54000)
1. Identify the principal: a writer has reached `max_collections` / `max_inflight` /
   `writes_per_sec`, or a client retries without backoff and amplifies the load.
2. Legitimate business growth: adjust that principal's quota (requires change approval), or scale out instances.
3. Client bug: require exponential backoff + jitter; do **not** mask a retry storm by raising global quotas.

### Authentication / Authorization (401 / 42501)
1. Distinguish: 401 = unknown token (credential problem/brute-force attempt); 403/42501 = a known principal is overstepping its authority.
2. 401 spike: check client credential rotation and whether scanners are present; rate-limit at the ingress if necessary.
3. 42501 spike: check whether the principal's `collections` scope is missing entries; confirm this is not attack probing.
4. Do not grant a principal `*` permissions or disable authentication just to "silence the alert".

## Recovery Verification

- The rejection rate falls and the normal request success rate recovers;
- After the quota adjustment, the corresponding principal receives no 429s and its collection count matches expectations;
- Authentication failures are no longer increasing (after ruling out scanning/brute force).

## Escalation

401s from unknown external sources or accompanied by anomalous SQL → security incident; long-term quota shortfall →
capacity planning/DBA.
