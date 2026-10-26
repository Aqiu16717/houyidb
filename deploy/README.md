# HouyiDB 部署资产

> [English](README.en.md)

完整部署步骤（Docker、二进制 + systemd、源码、Kubernetes、认证/TLS/加密、
备份与主备）见 [`docs/DEPLOYMENT_GUIDE.md`](../docs/DEPLOYMENT_GUIDE.md)。

本目录只放部署相关的配置资产：

```
deploy/
├── helm/houyidb/            # Helm chart（单机 / 主备两个 release）
├── prometheus/              # 告警规则
├── macos/houyi-server       # 历史预编译二进制（发布包以 GitHub Release 为准）
└── python/houyidb_client.py # 历史演示客户端（新项目请用 sdk/python/houyidb）
```

- 容器镜像：仓库根目录 `Dockerfile` 与 `docker-compose.yml`；
- Kubernetes：见 `deploy/helm/houyidb/README.md`；
- Python / TypeScript / Go / Java / .NET / Rust 客户端：见 `sdk/`。
