# Runbook: Audit Chain Verification Failure / Audit Write Error

> [中文](audit-chain-broken.md)

**Alert**: `increase(houyidb_audit_write_errors_total[5m]) > 0`
or `GET /v1/admin/audit/verify` returns failure (a broken chain at startup also refuses to start).

## Symptoms and Impact

- After an audit append fails, the log enters a poisoned state (fail-closed), and management operations may be rejected.
- A broken hash chain means audit records may have been tampered with/deleted/truncated — this is a security incident, not an ordinary operational
  failure.

## First 5 Minutes

```bash
# 1) Verification endpoint (requires an admin token)
curl -fsS -H "Authorization: Bearer $ADMIN_TOKEN" \
  http://127.0.0.1:17000/v1/admin/audit/verify
# 2) Audit segments and disk
ls -l /data/audit.log*; df -h /data; df -i /data
stat -c '%a %U:%G' /data/audit.log
# 3) First failure log
journalctl -u houyidb --since '1 hour ago' | grep -iE 'audit|hash chain|poison'
```

## Handling

1. **Do not**: delete audit segments, manually extend the hash chain, or modify `prev_hash` to make verification pass.
2. Disk full/permission error: expand capacity or fix permissions first; keep an on-site copy.
3. Broken chain:
   - Immediately stop management writes to that node;
   - Copy all audit segments to a read-only evidence directory (including the verification output);
   - Escalate to the security lead; per the audit incident process, decide whether to rebuild the audit stream from a known-good boundary
     (old segments are retained permanently and must not be deleted).
4. Poisoned due to audit append failure: restart after fixing the root cause; restart re-verifies the chain.

## Recovery Verification

- `/v1/admin/audit/verify` returns success with a contiguous entry count;
- `houyidb_audit_write_errors_total` stops increasing;
- New management operations produce contiguous new audit entries.

## Escalation

Any broken chain not caused by disk/permissions → security incident (P0), requiring legal/compliance notification.
