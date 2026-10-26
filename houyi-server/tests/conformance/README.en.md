# pgwire External Client Conformance (P4.8 / M4)

> [中文](README.md)

`pgwire.sh` starts a real houyi-server and drives the production wire implementation with **external clients**:

| Client | Coverage |
|--------|----------|
| `psql` | SSLRequest 'N' downgrade, Simple Query, ErrorResponse + SQLSTATE, new connections remain usable |
| `psycopg2` | libpq client, parameterized queries, implicit `BEGIN` transaction control |
| `pgbench -M simple` | Concurrent Simple Query |
| `pgbench -M extended` | Parse/Bind/Execute/Sync extended protocol |

This is not a complete pgjdbc/pgTAP suite, but it catches regressions of the kind where "the in-repo hand-written codec passes but a real libpq client cannot connect" (for example, the missing `BEGIN` support found this time).

```bash
./houyi-server/tests/conformance/pgwire.sh
```

Prerequisites: `psql`, `pgbench`, `python3` + `psycopg2`. The script uses dedicated ports 17951/17952 and a temporary WAL, and does not depend on any historical service on port 17000.
