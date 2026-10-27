## What / 改动

<!-- 说明动机与影响面；关联 issue 用 #123。 -->

## Why / 为什么

<!-- 解释问题与取舍，而不是复述 diff。 -->

## Test plan / 验证

- [ ] `cargo test --all`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo fmt --all -- --check`
- [ ] `./scripts/community_boundary_check.sh`
- [ ] 涉及复制/WAL/HA：`cargo test -p houyi-persistence -p houyi-server`（开发仓另跑演练）
- [ ] 涉及性能：`./scripts/perf_smoke_fast.sh`

## Checklist

- [ ] 未提交企业版代码、私有依赖或私有 registry 引用
- [ ] 新功能/修复有测试；历史 Bug 已补回归
- [ ] 没有削弱 RBAC/审计/加密/配额等安全控制
- [ ] 已阅读并同意 [`CLA.md`](../CLA.md)（外部贡献者）
- [ ] 文档/注释解释「为什么」，中英双语用于公共 API
