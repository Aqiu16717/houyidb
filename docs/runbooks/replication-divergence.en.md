# Runbook: Replication Log Divergence / follower_ahead

> [中文](replication-divergence.md)

**Alert**: `increase(houyidb_replication_diverged_rejections_total[5m]) > 0`
or `increase(houyidb_replication_follower_ahead_rejections_total[5m]) > 0`

## Symptoms and Impact

- The follower reconnects repeatedly and its handshake is rejected; the follower remains read-only.
- Meaning: the two sides have different history content at the same LSN, or the follower's log is longer than the leader's.
- **This is not network jitter**: continuing to retry will not self-heal; an authoritative replica must be determined manually first.

## First 5 Minutes

```bash
# 1) Confirm replication status and role on both sides
curl -fsS http://<node>:17000/metrics | grep houyidb_replication_(role|lag_entries|diverged)
# 2) Capture rejection logs (check both leader and follower)
journalctl -u houyidb --since '15 min ago' | grep -E 'diverged|follower_ahead|fingerprint'
# 3) Record next_lsn / log file size on both sides (stop writing now!)
curl -fsS http://<leader>:17000/v1/wal/status
curl -fsS http://<follower>:17000/v1/wal/status
```

## Handling Decision

1. **Do not**: delete the follower's WAL / use `--reset`-style operations to "make it start over".
2. Determine the authoritative side (based on data recency and business confirmation):
   - Leader is authoritative: stop the old follower → move its entire data directory away as a backup → rebuild the follower from the leader's
     backup/snapshot (`full_sync` collections must be fully resynchronized).
   - Follower is authoritative (fork caused by the old primary still writing locally after promote): first promote that
     follower or confirm the new primary, then rebuild the old primary as a follower; content written locally on the old primary must be
     compared at the business layer and backfilled manually.
3. Keep the old directories as read-only backups during the rebuild; incident postmortems need the fingerprints/log evidence.

## Recovery Verification

- `houyidb_replication_diverged_rejections_total` stops increasing;
- The follower's `houyidb_replication_lag_entries` falls back to 0;
- `houyidb_replication_followers_active >= 1` (on the leader side);
- Spot-check query: reading on the follower returns the id most recently written on the leader.

## Escalation

Involving a full_sync collection or unable to determine the authoritative side → escalate immediately to the DBA/architecture lead;
data-loss risk is handled as a P0 incident and the scene is preserved.
