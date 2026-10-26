# pgwire 外部客户端一致性（P4.8 / M4）

> [English](README.en.md)

`pgwire.sh` 启动真实 houyi-server，并用**外部客户端**驱动生产 wire 实现：

| 客户端 | 覆盖 |
|--------|------|
| `psql` | SSLRequest 'N' 降级、Simple Query、ErrorResponse + SQLSTATE、新连接可继续 |
| `psycopg2` | libpq 客户端、参数化查询、隐式 `BEGIN` 事务控制 |
| `pgbench -M simple` | 并发 Simple Query |
| `pgbench -M extended` | Parse/Bind/Execute/Sync 扩展协议 |

这不是完整的 pgjdbc/pgTAP 套件，但能抓住「仓内自写编解码器通过、真实 libpq
客户端不可用」这一类回归（例如本次发现的 `BEGIN` 未支持）。

```bash
./houyi-server/tests/conformance/pgwire.sh
```

前置：`psql`、`pgbench`、`python3` + `psycopg2`。脚本使用独立端口
17951/17952 与临时 WAL，不依赖 17000 上的历史服务。
