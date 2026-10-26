# 许可（Licensing）

> [English](LICENSING.en.md)
>
> 本文为中文翻译，法律效力以英文原文（LICENSING.en.md）为准。

HouyiDB 采用 **open-core** 模式。本文件说明哪些代码适用哪一种许可证；具有法律约束力的文本是 `LICENSE`（Apache-2.0）以及随企业版交付的商业协议（commercial agreement）。

## 1. 本仓库：Apache-2.0

本仓库（`houyidb`）中的全部源代码、文档、脚本与配置均按 **Apache License, Version 2.0**（Apache-2.0）许可。参见 [`LICENSE`](LICENSE) 与 [`NOTICE`](NOTICE)。

你可以使用、修改、分发本代码并以商业方式托管它，包括用于闭源产品，前提是你遵守 Apache-2.0（保留声明、附上许可证副本等）。

## 2. 企业版：商业许可（commercial license）

私有仓库 `houyidb-enterprise` 及其分发的二进制文件**并非**开源软件，它们按商业协议许可。企业特性包括 SSO/SCIM、外部 KMS/HSM、审计 WORM/留存（audit WORM/retention）、集群管理（fleet management）与认证构建（certified builds）。社区仓库包含完整的单机引擎；它从不依赖企业代码。

企业代码绝不会提交到本仓库，本仓库对其没有构建期或运行期依赖。

## 3. 贡献

除非文件另有明确说明，被本仓库接受的贡献按 Apache-2.0 许可。贡献者必须签署 [`CLA.md`](CLA.md)，以便项目方也能在商业版中使用该贡献。这是标准的双许可（dual-licensing）CLA 模式；它不转移版权所有权（copyright ownership）。

## 4. 第三方依赖

第三方 crate 保留其各自的许可证。`deny.toml` 将允许列表（allowlist）限制为适合商业分发的许可证；新增许可证需要经过明确审查。添加依赖前请运行 `./scripts/security_audit.sh`。

## 5. 商标（trademarks）

Apache-2.0 许可证不授予商标权（trademark rights）。HouyiDB 名称与徽标政策参见 [`TRADEMARK.md`](TRADEMARK.md)。

## 6. 联系方式

有关商业许可、企业评估或重新许可（relicensing）请求，请联系维护者（见 `CONTRIBUTING.md`）。本文件仅为摘要，不构成法律建议；以已签署的协议与 `LICENSE` 为准。
