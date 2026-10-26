# 60-second demo

> [中文](README.md)

`demo.sh` starts the single binary, upserts three vectors with payloads, runs a
vector search and a filtered hybrid search, then kills the process with
`kill -9` and restarts it. WAL replay makes the data searchable again.

```bash
cargo build --release -p houyi-server
./demo/demo.sh            # with pauses, for viewing
DEMO_FAST=1 ./demo/demo.sh  # no pauses, for smoke tests
```

Regenerate the README GIF:

```bash
brew install vhs
vhs demo/demo.tape
```

The GIF uses a fixed `/tmp/houyidb-demo` data directory so recordings do not
expose a personal path; the script cleans it up on exit. Override ports with
`HOUYI_DEMO_URL` / `HOUYI_DEMO_PG`.
