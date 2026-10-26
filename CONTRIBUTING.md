# Contributing to HouyiDB

> [English](CONTRIBUTING.en.md)

感谢参与。HouyiDB 是 open-core 项目：本仓库（社区版）采用 Apache-2.0，
企业能力在独立私有仓库中开发。提交前请阅读
[`LICENSING.md`](LICENSING.md)。

## 1. 贡献许可（CLA）

所有外部贡献必须先签署 [`CLA.md`](CLA.md)（个人；受雇贡献需公司授权）。
当前流程：在 PR 描述中写明

> I have read the CLA Document and I hereby sign the CLA.

维护者会记录签署。未签署 CLA 的 PR 不会被合并。CLA 允许项目方在
Apache-2.0 与商业许可下使用同一份贡献，不转移版权所有权。

## 2. 开发环境

```bash
# 稳定版 Rust（仓库 rust-version 为最低要求）
cargo build --all
cargo test --all
```

本地门禁（与 CI 同口径，提交前应全绿）：

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
./scripts/community_boundary_check.sh
./scripts/security_audit.sh        # 需 cargo-audit/deny/cyclonedx
```

涉及 WAL/复制/HA 的改动，另跑：

```bash
./scripts/replication_drill.sh
./scripts/concurrency_check.sh
./scripts/turmoil_check.sh
./scripts/chaos_drill.sh
```

## 3. 代码与提交规范

- 技术讨论与注释使用简体中文；公共 API/关键算法中英双语。
- 注释解释「为什么」，不要复述「做了什么」。
- 提交信息：`<type>: <description>`（feat/fix/refactor/docs/test/chore/perf/ci），
  说明动机与影响面；性能改动附测量数据。
- 一个 PR 聚焦一件事；不要顺手重排无关代码。
- 历史 Bug 必须补回归测试；性能门禁与正确性门禁不得绕过。
- 安全漏洞**不要**开公开 issue，见 [`SECURITY.md`](SECURITY.md)。

## 4. 边界纪律（重要）

- 公共仓库不得出现企业版代码、私有依赖、私有 registry 或跨仓 path 依赖；
  CI 的 `community_boundary_check.sh` 会拒绝。
- 不要为「未来商业版」预埋空抽象；扩展点按真实需求增加。
- 不要删除/弱化已有安全控制（RBAC、审计、加密、配额）来简化实现。

## 5. 评审与合并

1. PR 需通过全部 CI（test/clippy/fmt/security/boundary）。
2. 至少一名维护者 approve；核心持久化/复制/安全改动需两名。
3. 合并方式：squash merge（保持线性历史与清晰动机）。
4. 发布由维护者打 tag；社区版与企业版分别发布。

## 6. 行为准则

参与本项目即表示同意 [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md)。
