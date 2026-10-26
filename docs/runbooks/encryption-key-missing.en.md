# Runbook: Missing Encryption Key for Encrypted Snapshot / WAL Causes Startup Failure

> [中文](encryption-key-missing.md)

**Trigger**: Startup fails with logs containing
`graph snapshot: missing encryption key`, `vector snapshot: missing encryption key`,
or WAL v3 decryption failure; the process is not listening on any port.

## Symptoms and Impact

- The node is completely unavailable (fail-closed; this is the designed behavior).
- Common causes: the keyring file is not mounted, `active=` points to a nonexistent key, or the old key was deleted after rotation.
- **Never** disable encryption in order to start: that would make new writes fall back to plaintext, while old data would still be unreadable.

## First 5 Minutes

```bash
# 1) Key ids in the logs and the configured keyring
journalctl -u houyidb --since '10 min ago' | grep -E 'missing encryption key|key id|keyring'
grep -E 'encryption_key_file|encryption_key_id' /etc/houyidb/config.toml 2>/dev/null
ls -l /data/keyring; stat -c '%a %U:%G' /data/keyring
# 2) Keyring contents (check ids only; do not print keys)
grep -E '^(active=|[A-Za-z0-9_.-]+=)' /data/keyring | cut -d= -f1
```

## Handling

1. Restore the correct keyring file/mount; confirm that the key ids in the snapshots/WAL exist in the keyring.
2. Missing old key after rotation: restore the old key from the key backup (keep it; do not replace the new key),
   restart; consider cleanup only after old segments read successfully.
3. Keyring corrupted and no backup: data is unrecoverable — escalate to the security lead immediately and handle it as a data-loss
   incident; do not attempt brute force or key forgery.
4. After recovery, check file permissions: `chmod 600 /data/keyring`, with the service account as owner.

## Recovery Verification

- The startup log shows `encryption at rest enabled` (active key correct);
- `/readyz` passes;
- Reading a historical record (a snapshot/WAL from before the rotation) succeeds;
- After a new write, confirm the active WAL segment header key id is the current active key.

## Escalation

Key loss/suspected leak → security incident process (P0); also evaluate whether to rotate the
encryption keys for all affected data and rebuild replicas.
