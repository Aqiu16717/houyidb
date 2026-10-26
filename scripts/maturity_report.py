#!/usr/bin/env python3
"""企业就绪记分卡（P4.7 前置）——静态、可复现、可进发布门禁。

设计立场 / Design stance:
- 只测**仓库内的证据**（已入库文件/测试名/脚本），不测「口头承诺」。因此
  每项控制的证据都是可 grep、可复核的；假 PASS 比 FAIL 更危险，所以证据
  缺失一律按 FAIL/PARTIAL 处理。
- 默认只做静态扫描（秒级）；不跑测试、不联网。动态证据（cargo test / 变异
  测试 / 演练）由各自脚本产出，本报告只确认其**存在且被纪律化**。
- L3（受监管市场）需要外部审计/渗透/认证证据，本地不可评估——脚本只报告
  「需要外部证据」，不会伪装成已达标。

EN: Static, reproducible enterprise-readiness scorecard. It only credits
evidence committed to the repository (tests/scripts), never claims. Dynamic
gates remain in their own scripts. L3 always requires external evidence.

用法 / Usage:
  ./scripts/maturity_report.sh                 # markdown 报告
  ./scripts/maturity_report.sh --json          # 机器可读（CI）
  ./scripts/maturity_report.sh --gate l1       # L1 未达标时退出码 1
  ./scripts/maturity_report.sh --write docs/verification/enterprise-YYYYMMDD.md
  ./scripts/maturity_report.sh --selftest      # 记分与等级判定自测
"""

from __future__ import annotations

import argparse
import datetime as _dt
import fnmatch
import json
import re
import subprocess
import sys
from dataclasses import dataclass, asdict
from functools import lru_cache
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

DIMENSIONS = {
    "correctness": ("正确性与验证", 25),
    "security": ("安全与合规就绪", 25),
    "operability": ("运维与生命周期", 20),
    "resilience": ("韧性与 SLO", 20),
    "compatibility": ("兼容性与文档", 10),
}

STATUS_VALUE = {"PASS": 1.0, "PARTIAL": 0.5, "FAIL": 0.0}
# L0 必须存在：evaluate() 在任一 L1 控制未达标时返回 L0，而门禁路径要读它。
# EN: L0 must exist — evaluate() returns it when any L1 control fails, and the
# gate path reads this map (a missing key would crash the diagnostic path).
LEVEL_ORDER = {"L0": 0, "L1": 1, "L2": 2, "L3": 3}


@dataclass
class Result:
    id: str
    dimension: str
    level: str
    weight: int
    title: str
    status: str
    evidence: str


# ── 仓库取证工具 ────────────────────────────────────────────────
# EN: -- repository evidence tools --

@lru_cache(maxsize=1)
def tracked_files() -> tuple[str, ...]:
    """git 跟踪的文件（相对路径）。未入库的文档/产物不算证据。
    EN: Files tracked by git (relative paths). Untracked docs/artifacts do not
    EN: count as evidence.
    """
    try:
        out = subprocess.run(
            ["git", "ls-files", "-z"], cwd=ROOT, capture_output=True, check=True
        ).stdout.decode("utf-8", "replace")
        return tuple(p for p in out.split("\0") if p)
    except (OSError, subprocess.CalledProcessError):
        # 非 git 环境退化为磁盘扫描（仍排除构建产物）。
        # EN: Non-git environments degrade to a disk scan (still excluding build artifacts).
        skip = {".git", "target", "node_modules", ".venv-eco", "bench-results"}
        files = []
        for p in ROOT.rglob("*"):
            if not p.is_file() or any(part in skip for part in p.parts):
                continue
            files.append(str(p.relative_to(ROOT)))
        return tuple(files)


# 记分卡自身不得作为证据来源：控制项文本里含「升级/回滚演练」等关键词，
# 自扫描会制造假 PASS（O7 曾因此通过）。
# EN: never let the scorecard's own text count as evidence (self-match).
EVIDENCE_EXCLUDE = {
    "scripts/maturity_report.py",
    "scripts/maturity_report.sh",
    # 记分卡定义文档本身描述了全部控制与关键词（SLO/Runbook/审计…），
    # 若参与扫描会让未实现的项凭「文档里提过」变 PASS。
    # EN: the scorecard definition describes every control; scanning it would
    # turn unimplemented items green just because they are named there.
    "docs/ENTERPRISE_READINESS.md",
}


def matching(patterns: list[str]) -> list[str]:
    out = []
    for rel in tracked_files():
        if rel in EVIDENCE_EXCLUDE:
            continue
        if any(fnmatch.fnmatch(rel, pat) for pat in patterns):
            out.append(rel)
    return sorted(set(out))


@lru_cache(maxsize=2048)
def text_of(rel: str) -> str:
    try:
        p = ROOT / rel
        if p.stat().st_size > 2 * 1024 * 1024:
            return ""
        return p.read_text(encoding="utf-8", errors="ignore")
    except OSError:
        return ""


def grep(pattern: str, patterns: list[str]) -> list[tuple[str, int, str]]:
    """所有匹配行（不按文件去重：计数型证据需要真实条数）。
    EN: All matching lines (not deduplicated per file: count-based evidence needs
    EN: the real number).
    """
    rx = re.compile(pattern)
    hits = []
    for rel in matching(patterns):
        for i, line in enumerate(text_of(rel).splitlines(), 1):
            if rx.search(line):
                hits.append((rel, i, line.strip()))
    return hits


def fn_exists(name: str) -> list[tuple[str, int, str]]:
    return grep(rf"\bfn\s+{re.escape(name)}\s*\(", SRC_GLOBS)


def count_files(patterns: list[str], needle: str | None = None) -> int:
    files = matching(patterns)
    if needle is None:
        return len(files)
    return sum(1 for rel in files if needle in text_of(rel))


# fnmatch 的 `/` 是字面量：`src/**/*.rs` 不会匹配 `src/http_handler.rs`
#（** 与 * 同义，但中间的 `/` 必须存在）。因此每个目录同时列直接子文件与深目录。
# EN: fnmatch treats `/` literally, so list both depth-0 and nested patterns.
SRC_GLOBS = [
    "houyi-*/src/*.rs",
    "houyi-*/src/**/*.rs",
    "houyi-*/tests/*.rs",
    "houyi-*/tests/**/*.rs",
    "sdk/*.rs",
    "sdk/**/*.rs",
]
DOC_GLOBS = ["docs/*.md", "docs/**/*.md", "deploy/*.md", "deploy/**/*.md"]
SCRIPT_GLOBS = ["scripts/*.sh", "scripts/*.py", "scripts/**/*.sh", "scripts/**/*.py"]
CI_GLOBS = [".github/workflows/*.yml"]


def evidence_list(hits, empty: str) -> str:
    if not hits:
        return empty
    rel, line, _ = hits[0]
    more = f" (+{len(hits) - 1})" if len(hits) > 1 else ""
    return f"{rel}:{line}{more}"


def st(cond: bool) -> str:
    return "PASS" if cond else "FAIL"


# ── 故障模式覆盖矩阵 ────────────────────────────────────────────
# 每条 = 一类已知故障 / 失效窗口。证据必须是**测试函数名、演练脚本**或
# 注入点标识；只有实现代码而没有验证手段不算覆盖。
# EN: -- failure mode coverage matrix --
# EN: Each entry = one known failure class / failure window. Evidence must be a **test
# EN: function name, drill script**, or injection-point id; implementation code alone
# EN: without a verification method does not count as coverage.
FAILURE_MODES: list[tuple[str, str]] = [
    ("WAL 撕裂尾恢复", r"torn_tail"),
    ("WAL CRC 腐坏可见", r"corrupt_record_is_error_not_silent_stall"),
    ("WAL 写失败毒化", r"poisoned_wal_rejects_writes_until_reopen"),
    ("WAL 截断自愈", r"successful_truncate_clears_poison"),
    ("WAL LSN 越序拒绝", r"file_wal_append_lsn_gap_rejected"),
    ("指纹 LSN 洞拒绝", r"lsn_hole_is_an_error"),
    ("崩溃一致性演练", r"chaos_invariants\.py"),
    ("fsync 失败丢弃未确认批", r"flush_batch_drops_pending_on_fsync_error"),
    ("apply 瞬时失败重试", r"apply_retries_transient_errors_in_place"),
    ("apply 解码腐败不重试", r"apply_does_not_retry_decode_corruption"),
    ("本地日志不可指纹分类", r"local_log_fingerprint_failure_is_classified"),
    ("断连续传无重复", r"reconnect_resumes_without_duplicates"),
    ("follower_ahead 拒绝", r"follower_ahead_is_rejected_and_counted"),
    ("日志分叉拒绝", r"diverged_log_is_rejected_and_counted"),
    ("复制认证不匹配", r"auth_mismatch_is_rejected_and_counted"),
    ("promote 保留认证", r"promote_preserves_replication_auth_token"),
    ("网络分区恢复", r"turmoil_partition_then_catch_up_without_gaps_or_duplicates"),
    ("空闲链路活性", r"idle_link_survives_ack_timeout_via_heartbeat_acks"),
    ("慢 follower 踢除", r"hub_kicks_slow_follower_and_keeps_fast"),
    ("promote 幂等与角色翻转", r"promote_flips_role_unlocks_writes_and_is_idempotent"),
    ("备份恢复演练", r"backup_restore_drill\.sh"),
    ("冷重放状态机", r"recovery_state_machine"),
    ("过滤 TLP 三分区", r"tlp_partition_on_tag"),
    ("检索暴力差分", r"flat_search_equals_brute_force"),
    ("ENOSPC/写前错误注入", r"wal_append_pre"),
    ("DNS 解析超时", r"fn\s+[a-z0-9_]*dns[a-z0-9_]*\s*\("),
    ("阻塞调用并发预算", r"blocking_budget|blocking_wal_[a-z_]*\("),
]


def failure_mode_scan():
    covered, missing = [], []
    for name, pattern in FAILURE_MODES:
        hits = grep(pattern, SRC_GLOBS + SCRIPT_GLOBS)
        (covered if hits else missing).append((name, hits))
    return covered, missing


# ── 秘密扫描（高置信度模式；占位符不误报）──────────────────────
# EN: -- secret scan (high-confidence patterns; placeholders do not false-positive) --

SECRET_PATTERNS = [
    ("私钥块", re.compile(r"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----")),
    ("AWS Access Key", re.compile(r"\bAKIA[0-9A-Z]{16}\b")),
    ("GitHub Token", re.compile(r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36}\b")),
    ("Slack Token", re.compile(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b")),
]
TEXT_EXTS = (
    ".rs", ".py", ".go", ".java", ".cs", ".ts", ".js", ".toml", ".yaml",
    ".yml", ".json", ".md", ".sh", ".html", ".css", ".xml", ".txt",
)


def secret_scan() -> list[tuple[str, int, str, str]]:
    findings = []
    for rel in tracked_files():
        if not rel.lower().endswith(TEXT_EXTS):
            continue
        text = text_of(rel)
        for label, rx in SECRET_PATTERNS:
            for i, line in enumerate(text.splitlines(), 1):
                if rx.search(line):
                    findings.append((label, i, rel, line.strip()[:120]))
    return findings


# ── 控制项评估 ─────────────────────────────────────────────────
# EN: -- control evaluation --

def build_results():
    res: list[Result] = []

    def add(id, dim, level, weight, title, status, evidence):
        res.append(Result(id, dim, level, weight, title, status, evidence))

    # 正确性与验证 / correctness
    # EN: Correctness and verification
    proptest_files = count_files(SRC_GLOBS, "proptest!")
    add("C1", "correctness", "L2", 4, "属性化测试（proptest）",
        st(proptest_files >= 3), f"{proptest_files} 个文件含 proptest!")

    fuzz_targets = count_files(["fuzz/fuzz_targets/*.rs"])
    corpus_files = count_files(["fuzz/corpus/*", "fuzz/corpus/**/*"])
    fuzz_script = count_files(["scripts/fuzz_nightly.sh"])
    add("C2", "correctness", "L2", 4, "结构感知 fuzz 与语料",
        "PASS" if fuzz_targets >= 4 and corpus_files >= 4 and fuzz_script else
        ("PARTIAL" if fuzz_targets >= 1 else "FAIL"),
        f"{fuzz_targets} targets / {corpus_files} corpus 文件 / nightly 脚本={bool(fuzz_script)}")

    shuttle = grep(r"shuttle::", SRC_GLOBS)
    loom = grep(r"loom::", SRC_GLOBS)
    conc_script = count_files(["scripts/concurrency_check.sh"])
    add("C3", "correctness", "L2", 3, "调度随机化 + 内存序穷举",
        st(bool(shuttle and loom and conc_script)),
        f"shuttle={bool(shuttle)} loom={bool(loom)} 脚本={bool(conc_script)}")

    turmoil_tests = grep(r"fn\s+turmoil_", SRC_GLOBS + ["houyi-server/tests/*.rs"])
    turmoil_script = count_files(["scripts/turmoil_check.sh"])
    add("C4", "correctness", "L2", 3, "确定性网络仿真（turmoil）",
        st(len(turmoil_tests) >= 3 and bool(turmoil_script)),
        f"{len(turmoil_tests)} 个 turmoil 用例 / 脚本={bool(turmoil_script)}")

    always = grep(r"Property::always\(", SRC_GLOBS)
    sometimes = grep(r"Property::sometimes\(", SRC_GLOBS)
    add("C5", "correctness", "L2", 3, "协议模型检查（Stateright）",
        st(len(always) >= 3),
        f"{len(always)} 条 always 不变量 / {len(sometimes)} 条 sometimes")

    chaos = count_files(["scripts/chaos_drill.sh"])
    chaos_inv = count_files(["scripts/chaos_invariants.py"], "--selftest")
    add("C6", "correctness", "L1", 3, "崩溃/故障注入演练",
        st(bool(chaos and chaos_inv)),
        f"演练脚本={bool(chaos)} 校验器自测={bool(chaos_inv)}")

    covered, missing = failure_mode_scan()
    ratio = len(covered) / len(FAILURE_MODES)
    add("C7", "correctness", "L2", 3, "故障模式覆盖矩阵",
        "PASS" if ratio >= 0.85 else ("PARTIAL" if ratio >= 0.60 else "FAIL"),
        f"{len(covered)}/{len(FAILURE_MODES)}（{ratio:.0%}）；缺口：" +
        "、".join(n for n, _ in missing[:6]))

    seeds = count_files(["**/*.proptest-regressions", "**/proptest-regressions/*"])
    seed_discipline = grep(r"CHAOS_SEED|PROPTEST_CASES|SHUTTLE_RANDOM_SEED", SCRIPT_GLOBS)
    add("C8", "correctness", "L1", 2, "失败种子入库与复现纪律",
        st(seeds > 0 and bool(seed_discipline)),
        f"{seeds} 个回归种子文件 / 种子环境变量={len(seed_discipline)} 处")

    # 安全 / security
    # EN: Security
    http_auth = grep(r"pub async fn require_auth", SRC_GLOBS)
    auth_tests = fn_exists("require_auth_guards_routes_and_exempts_probes")
    add("S1", "security", "L1", 3, "HTTP 接口认证（Bearer）",
        st(bool(http_auth and auth_tests)),
        evidence_list(http_auth or auth_tests, "未找到 require_auth 及其测试"))

    pg_auth = count_files(["houyi-server/tests/pgwire_auth.rs"])
    add("S2", "security", "L2", 3, "pgwire 认证",
        st(bool(pg_auth)), "houyi-server/tests/pgwire_auth.rs")

    tls_tests = count_files(["houyi-server/tests/pgwire_tls.rs"])
    tls_cfg = grep(r"tls_cert_path", ["houyi-server/src/types.rs"])
    add("S3", "security", "L2", 3, "传输层加密（TLS）",
        st(bool(tls_tests and tls_cfg)),
        f"TLS 测试={bool(tls_tests)} 配置项={bool(tls_cfg)}")

    rep_auth = grep(r"pub fn tokens_match", SRC_GLOBS)
    rep_auth_kind = grep(r"ERROR_KIND_AUTH", SRC_GLOBS)
    rep_auth_test = fn_exists("auth_mismatch_is_rejected_and_counted")
    add("S4", "security", "L2", 4, "复制共享秘密认证",
        st(bool(rep_auth and rep_auth_kind and rep_auth_test)),
        f"tokens_match={bool(rep_auth)} AUTH kind={bool(rep_auth_kind)} 测试={bool(rep_auth_test)}")

    # 只认声明与测试，不认注释关键词（comment-only 匹配曾制造假 PASS）。
    # EN: declarations + tests only; a comment mentioning "rbac" must not pass.
    rbac_impl = grep(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum|trait)\s+"
        r"(?:Role|Principal|Permission|Authz)",
        SRC_GLOBS,
    )
    rbac_test = grep(r"^\s*(?:async\s+)?fn\s+[a-z0-9_]*rbac[a-z0-9_]*\s*\(", SRC_GLOBS)
    add("S5", "security", "L2", 3, "RBAC / 最小权限",
        st(bool(rbac_impl and rbac_test)),
        f"实现声明={bool(rbac_impl)} 授权测试={bool(rbac_test)}")

    # 审计必须同时具备：实现、哈希链、校验入口、篡改检测测试——只出现
    # “audit log” 字符串不算（防文档/命名假 PASS）。
    # EN: Audit must have all of: implementation, hash chain, verification entry point,
    # EN: tamper-detection test — merely the string "audit log" does not count (guards
    # EN: against documentation/naming fake PASS).
    audit_impl = grep(r"\bAuditLog\b", SRC_GLOBS)
    audit_chain = grep(r"GENESIS_HASH|prev_hash|hash_body", SRC_GLOBS)
    audit_verify = grep(r"/v1/admin/audit/verify|handle_admin_audit_verify", SRC_GLOBS)
    audit_test = fn_exists("chain_verifies_and_detects_tamper")
    audit_ok = bool(audit_impl and audit_chain and audit_verify and audit_test)
    add("S6", "security", "L2", 3, "不可篡改审计日志",
        st(audit_ok),
        f"实现={bool(audit_impl)} 哈希链={bool(audit_chain)} 校验端点={bool(audit_verify)} "
        f"篡改测试={bool(audit_test)}")

    enc_impl = grep(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum|trait|fn)\s+"
        r"\w*(?:Kms|KMS|Encryption|Cipher)\w*",
        SRC_GLOBS,
    )
    enc_test = grep(r"^\s*(?:async\s+)?fn\s+[a-z0-9_]*(?:encrypt|kms)[a-z0-9_]*\s*\(", SRC_GLOBS)
    add("S7", "security", "L2", 3, "静态加密与 KMS/HSM 接口",
        st(bool(enc_impl and enc_test)),
        f"加密/KMS 声明={bool(enc_impl)} 测试={bool(enc_test)}")

    # 方案 A（v1 裁决）：租户隔离 = 主体 collection 作用域 + 列表过滤；
    # 配额 = 每主体集合数/在途/写速率。只认字段声明与测试名。
    # EN: v1 tenancy = per-principal scopes + list filtering + quotas.
    tenant_impl = grep(
        r"^\s*pub max_collections: Option<usize>|^\s*pub fn collection_quota_exceeded",
        SRC_GLOBS,
    )
    tenant_test = grep(
        r"^\s*(?:async\s+)?fn\s+[a-z0-9_]*(?:quota|scope)[a-z0-9_]*\s*\(",
        SRC_GLOBS,
    )
    add("S8", "security", "L2", 3, "租户隔离与配额（作用域 + 配额）",
        st(bool(tenant_impl and tenant_test)),
        f"作用域/配额声明={bool(tenant_impl)} 测试={bool(tenant_test)}")

    ci_audit = grep(r"cargo[ -](?:audit|deny)|cyclonedx|sbom|SBOM", CI_GLOBS)
    lockfile = count_files(["Cargo.lock"])
    add("S9", "security", "L2", 3, "供应链审计与 SBOM",
        "PASS" if ci_audit else ("PARTIAL" if lockfile else "FAIL"),
        evidence_list(ci_audit, "CI 无 cargo audit/deny/SBOM；仅提交 Cargo.lock"))

    secrets = secret_scan()
    add("S10", "security", "L2", 2, "凭据扫描",
        st(not secrets),
        "未发现高置信度凭据" if not secrets else
        f"{len(secrets)} 处：{secrets[0][2]}:{secrets[0][1]} ({secrets[0][0]})")

    # 运维 / operability
    # EN: Operability
    health = grep(r'"/v1/health"', SRC_GLOBS)
    ready = grep(r'"/readyz"', SRC_GLOBS)
    add("O1", "operability", "L1", 3, "存活/就绪探针",
        st(bool(health and ready)), f"health={bool(health)} readyz={bool(ready)}")

    metric_count = len(re.findall(r"# TYPE houyidb_", text_of("houyi-server/src/http_handler.rs")))
    add("O2", "operability", "L1", 3, "指标面（Prometheus）",
        st(metric_count >= 20), f"http_handler 中 {metric_count} 个指标族")

    backup = count_files(["scripts/backup_restore_drill.sh"], "restore")
    add("O3", "operability", "L1", 3, "备份/恢复演练",
        st(bool(backup)), "scripts/backup_restore_drill.sh")

    repl = count_files(["scripts/replication_drill.sh"], "RPO=0")
    add("O4", "operability", "L2", 3, "主备切换演练",
        st(bool(repl)), "scripts/replication_drill.sh（含 RPO=0 断言）")

    soak = bool(count_files(["scripts/soak_nightly.sh"])) and bool(
        count_files(["scripts/soak_probe.py"])
    )
    add("O5", "operability", "L2", 2, "长稳/soak 哨兵",
        st(soak), "soak_nightly.sh + soak_probe.py")

    docker = count_files(["Dockerfile", "docker-compose.yml"])
    helm = count_files([
        "**/Chart.yaml",
        "deploy/*/values.yaml",
        "deploy/**/values.yaml",
        "deploy/**/k8s*.yaml",
    ])
    add("O6", "operability", "L2", 3, "容器与编排部署",
        "PASS" if docker >= 2 and helm else ("PARTIAL" if docker else "FAIL"),
        f"容器文件={docker} 编排清单={helm}")

    # 证据必须是**测试函数名或脚本文件名**，不能只靠注释里的关键词
    # （否则任何提及「升级」的文档都会假 PASS）。
    # EN: evidence must be a test name or a script file, never a comment match.
    upgrade_tests = grep(
        r"\bfn\s+[a-z0-9_]*(?:upgrade|rollback)[a-z0-9_]*\s*\(", SRC_GLOBS
    )
    upgrade_scripts = [
        rel for rel in matching(SCRIPT_GLOBS)
        if re.search(r"upgrade|rollback", rel, re.I)
    ]
    upgrade = upgrade_tests or [(s, 0, s) for s in upgrade_scripts]
    add("O7", "operability", "L2", 3, "升级/回滚演练（N-1 版本偏移）",
        st(bool(upgrade)),
        evidence_list(upgrade, "未发现升级/回滚测试或脚本"))

    cfg_validation = fn_exists("toml_blank_auth_token_is_fatal_fail_closed")
    add("O8", "operability", "L2", 2, "配置校验 fail-fast",
        st(bool(cfg_validation)), "TOML/ENV 配置 fail-fast 测试")

    # 韧性 / resilience
    # EN: Resilience
    add("R1", "resilience", "L1", 4, "崩溃一致性",
        st(bool(chaos and chaos_inv)), "chaos_drill + 不变量校验器")

    partition = fn_exists("turmoil_partition_then_catch_up_without_gaps_or_duplicates")
    add("R2", "resilience", "L2", 4, "HA 与故障域切换",
        st(bool(repl and partition)),
        f"主备演练={bool(repl)} 分区恢复={bool(partition)}")

    add("R3", "resilience", "L2", 3, "实测 RPO",
        st(bool(repl)), "full_sync RPO=0（replication_drill 断言）")

    rto = grep(r"(?i)\bRTO\b|\bMTTR\b|恢复时间目标", SCRIPT_GLOBS + ["docs/verification/*.md"])
    add("R4", "resilience", "L2", 3, "实测 RTO/MTTR",
        st(bool(rto)), evidence_list(rto, "未发现 RTO/MTTR 实测记录"))

    slo = grep(r"(?i)\bSLO\b|error budget|错误预算", SCRIPT_GLOBS + DOC_GLOBS)
    add("R5", "resilience", "L2", 3, "SLO 与错误预算",
        st(bool(slo)), evidence_list(slo, "未定义 SLO/错误预算"))

    backpressure = fn_exists("hub_kicks_slow_follower_and_keeps_fast")
    queue_cfg = grep(r"replication_queue_batches", SRC_GLOBS)
    add("R6", "resilience", "L2", 3, "背压与慢节点治理",
        st(bool(backpressure and queue_cfg)),
        f"慢 follower 踢除测试={bool(backpressure)} 队列上限配置={bool(queue_cfg)}")

    # 兼容性 / compatibility
    # EN: Compatibility
    changelog = count_files(["CHANGELOG.md"])
    changelog_age = None
    if changelog:
        dates = re.findall(r"\((\d{4}-\d{2}-\d{2})\)", text_of("CHANGELOG.md"))
        latest = max(dates) if dates else None
        if latest:
            try:
                changelog_age = (
                    _dt.date.fromisoformat(commit_date())
                    - _dt.date.fromisoformat(latest)
                ).days
            except ValueError:
                changelog_age = None
    # 企业发布要求变更日志跟得上代码；120 天未更新按 PARTIAL（不是 FAIL——
    # 日志存在但陈旧，属于流程债而非能力缺失）。
    # EN: a stale changelog is process debt -> PARTIAL, not FAIL.
    add("M1", "compatibility", "L2", 2, "版本与变更日志（新鲜度）",
        "PASS" if changelog and changelog_age is not None and changelog_age <= 120
        else ("PARTIAL" if changelog else "FAIL"),
        f"CHANGELOG 最近更新距今 {changelog_age} 天" if changelog_age is not None
        else ("CHANGELOG 无日期" if changelog else "缺少 CHANGELOG.md"))

    pinned = fn_exists("protocol_version_is_pinned")
    add("M2", "compatibility", "L2", 2, "线协议版本钉死",
        st(bool(pinned)), "protocol_version_is_pinned")

    sdk_langs = sorted({
        rel.split("/")[1] for rel in tracked_files()
        if rel.startswith("sdk/") and rel.count("/") >= 2
    })
    add("M3", "compatibility", "L2", 2, "多语言 SDK",
        st(len(sdk_langs) >= 4), f"{len(sdk_langs)} 种：{', '.join(sdk_langs)}")

    pgwire_tests = count_files(["houyi-server/tests/pgwire_*.rs"])
    conformance = count_files(["**/conformance/**", "**/pgwire_conformance*"])
    add("M4", "compatibility", "L2", 2, "pgwire 一致性套件",
        "PASS" if conformance else ("PARTIAL" if pgwire_tests >= 2 else "FAIL"),
        f"仓内协议测试={pgwire_tests} 外部一致性套件={bool(conformance)}")

    runbook = grep(r"(?i)runbook|处置手册|应急处置", DOC_GLOBS)
    add("M5", "compatibility", "L2", 2, "告警 Runbook",
        st(bool(runbook)), evidence_list(runbook, "未发现 Runbook/处置手册"))

    return res, covered, missing


# ── 评分与等级判定 ─────────────────────────────────────────────
# EN: -- scoring and grade determination --

def evaluate(results: list[Result]):
    dim_scores = {}
    for dim in DIMENSIONS:
        items = [r for r in results if r.dimension == dim and r.status != "NA"]
        if not items:
            continue
        got = sum(r.weight * STATUS_VALUE[r.status] for r in items)
        total = sum(r.weight for r in items)
        dim_scores[dim] = 100.0 * got / total

    num = sum(dim_scores[d] * DIMENSIONS[d][1] for d in dim_scores)
    den = sum(DIMENSIONS[d][1] for d in dim_scores)
    overall = num / den if den else 0.0

    def blockers_for(level: str):
        req = [r for r in results if LEVEL_ORDER[r.level] <= LEVEL_ORDER[level]]
        return [r for r in req if r.status != "PASS"]

    l1_block = blockers_for("L1")
    l2_block = blockers_for("L2")
    l1_ok = not l1_block and overall >= 60.0
    l2_ok = l1_ok and not l2_block and overall >= 80.0
    achieved = "L2" if l2_ok else ("L1" if l1_ok else "L0")
    return {
        "overall": overall,
        "dimensions": dim_scores,
        "achieved": achieved,
        "l1_blockers": l1_block,
        "l2_blockers": l2_block,
    }


def render_markdown(results, covered, missing, ev, meta):
    lines = []
    lines.append("# HouyiDB 企业就绪记分卡")
    lines.append("")
    lines.append(f"- 生成时间：{meta['generated_at']}")
    lines.append(f"- 提交：`{meta['commit']}`　版本：`{meta['version']}`")
    lines.append(f"- 综合得分：**{ev['overall']:.1f} / 100**")
    lines.append(f"- 本地可评估等级：**{ev['achieved']}**（L3 需外部审计/认证证据）")
    lines.append("")
    lines.append("> 本报告是**代理指标**：只统计仓库内可复核的证据（测试名/脚本/"
                 "实现点），不等同于认证。证据缺失按 FAIL/PARTIAL 计，宁可低估。")
    lines.append("")
    lines.append("## 维度得分")
    lines.append("")
    lines.append("| 维度 | 权重 | 得分 |")
    lines.append("|------|------|------|")
    for dim, (label, weight) in DIMENSIONS.items():
        score = ev["dimensions"].get(dim)
        lines.append(f"| {label} | {weight} | {score:.1f} |" if score is not None
                     else f"| {label} | {weight} | N/A |")
    lines.append("")
    lines.append("## 等级阻断项")
    lines.append("")
    if ev["achieved"] in ("L1", "L2"):
        lines.append(f"当前已满足 **{ev['achieved']}**。")
    else:
        lines.append("当前**未达到 L1**。")
    for lvl, blockers in (("L1", ev["l1_blockers"]), ("L2", ev["l2_blockers"])):
        if blockers:
            lines.append("")
            lines.append(f"**{lvl} 阻断**（{len(blockers)} 项）：")
            for r in blockers:
                lines.append(f"- `{r.id}` {r.title}：{r.status} — {r.evidence}")
    if not ev["l2_blockers"] and ev["achieved"] == "L1":
        lines.append("")
        lines.append("**L2 阻断**：需总分 ≥ 80。")
    lines.append("")
    lines.append("## 控制明细")
    lines.append("")
    for dim in DIMENSIONS:
        label = DIMENSIONS[dim][0]
        items = [r for r in results if r.dimension == dim]
        if not items:
            continue
        lines.append(f"### {label}")
        lines.append("")
        lines.append("| 等级 | 控制 | 状态 | 权重 | 证据 |")
        lines.append("|------|------|------|------|------|")
        for r in items:
            lines.append(
                f"| {r.level} | `{r.id}` {r.title} | {r.status} | "
                f"{r.weight} | {r.evidence} |"
            )
        lines.append("")
    lines.append("## 故障模式覆盖")
    lines.append("")
    lines.append(f"覆盖 {len(covered)}/{len(FAILURE_MODES)}。")
    if missing:
        lines.append("")
        lines.append("缺口（未发现自动化验证证据）：")
        for name, _ in missing:
            lines.append(f"- {name}")
    lines.append("")
    lines.append("## 说明")
    lines.append("")
    lines.append("- 控制目录、等级门槛与合规映射见 `docs/ENTERPRISE_READINESS.md`。")
    lines.append("- 动态门禁（测试/演练/变异测试）仍由各自脚本执行；本报告只确认其"
                 "存在且被纪律化。")
    lines.append("- 新增安全/运维能力时，必须同步更新控制项与证据；不得只改文档。")
    lines.append("")
    return "\n".join(lines)


# ── 自测（不依赖仓库状态）──────────────────────────────────────
# EN: -- self-test (does not depend on repository state) --

def selftest() -> int:
    def mk(id, dim, lvl, status, weight=1):
        return Result(id, dim, lvl, weight, id, status, "synthetic")

    failures = []

    all_pass = [mk("a", "correctness", "L1", "PASS"), mk("b", "security", "L2", "PASS")]
    ev = evaluate(all_pass)
    if ev["achieved"] != "L2" or abs(ev["overall"] - 100.0) > 1e-9:
        failures.append("all-pass must score 100 and reach L2")

    l1_blocked = [mk("a", "correctness", "L1", "PARTIAL"), mk("b", "security", "L2", "PASS")]
    ev = evaluate(l1_blocked)
    if ev["achieved"] != "L0" or LEVEL_ORDER.get(ev["achieved"]) != 0:
        failures.append("an L1 PARTIAL must block L1 and be gate-readable as L0")

    # 总分 ≥ 60（L1 达标）但 L2 有 FAIL ⇒ 封顶 L1 并给出阻断项。
    # EN: Total >= 60 (L1 met) but an L2 FAIL => cap at L1 and list the blockers.
    l2_blocked = [
        mk("a", "correctness", "L1", "PASS"),
        mk("c", "security", "L2", "PASS"),
        mk("b", "security", "L2", "FAIL"),
    ]
    ev = evaluate(l2_blocked)
    if ev["achieved"] != "L1" or not ev["l2_blockers"]:
        failures.append("an L2 FAIL must cap at L1 with a blocker")

    # NA 不参与分母：单个 NA 维度不应把其他维度的分数拉低。
    # EN: NA is excluded from the denominator: a single NA dimension must not drag down
    # EN: other dimensions' scores.
    ev = evaluate([mk("a", "correctness", "L1", "PASS", 3),
                   Result("n", "security", "L2", 3, "n", "NA", "synthetic")])
    if abs(ev["dimensions"]["correctness"] - 100.0) > 1e-9:
        failures.append("NA controls must be excluded from the denominator")

    for f in failures:
        print(f"selftest FAIL: {f}", file=sys.stderr)
    if failures:
        return 1
    print("selftest PASS: 记分与等级判定（4 例）")
    return 0


# ── 主入口 ─────────────────────────────────────────────────────
# EN: -- main entry --

def git_commit() -> str:
    try:
        return subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"], cwd=ROOT,
            capture_output=True, check=True, text=True
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


def workspace_version() -> str:
    m = re.search(r'^version\s*=\s*"([^"]+)"', text_of("Cargo.toml"), re.M)
    return m.group(1) if m else "unknown"


@lru_cache(maxsize=1)
def commit_date() -> str:
    try:
        out = subprocess.run(
            ["git", "log", "-1", "--format=%cs"], cwd=ROOT,
            capture_output=True, check=True, text=True
        ).stdout.strip()
        return out or _dt.date.today().isoformat()
    except (OSError, subprocess.CalledProcessError):
        return _dt.date.today().isoformat()


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="HouyiDB enterprise readiness scorecard")
    ap.add_argument("--json", action="store_true", help="JSON 输出")
    ap.add_argument("--gate", choices=["l1", "l2", "l3"], help="门禁：等级未达则退出码 1")
    ap.add_argument("--write", metavar="PATH", help="把 markdown 报告写入文件")
    ap.add_argument("--selftest", action="store_true", help="记分逻辑自测")
    args = ap.parse_args(argv)

    if args.selftest:
        return selftest()

    results, covered, missing = build_results()
    ev = evaluate(results)
    meta = {
        "generated_at": _dt.datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "commit": git_commit(),
        "version": workspace_version(),
    }

    if args.json:
        payload = {
            "meta": meta,
            "overall": round(ev["overall"], 2),
            "level": ev["achieved"],
            "dimensions": {k: round(v, 2) for k, v in ev["dimensions"].items()},
            "controls": [asdict(r) for r in results],
            "failure_modes": {
                "total": len(FAILURE_MODES),
                "covered": [n for n, _ in covered],
                "missing": [n for n, _ in missing],
            },
            "blockers": {
                "l1": [r.id for r in ev["l1_blockers"]],
                "l2": [r.id for r in ev["l2_blockers"]],
            },
            "l3": "external audit/certification evidence required",
        }
        out = json.dumps(payload, ensure_ascii=False, indent=2)
    else:
        out = render_markdown(results, covered, missing, ev, meta)

    if args.write:
        path = Path(args.write)
        if not path.is_absolute():
            path = ROOT / path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(out + "\n", encoding="utf-8")
        # JSON 模式下状态消息走 stderr，避免污染可解析的 stdout。
        # EN: In JSON mode, status messages go to stderr to avoid polluting parseable stdout.
        print(f"报告已写入 {path}", file=sys.stderr if args.json else sys.stdout)

    if args.json or not args.write:
        print(out)

    if args.gate:
        want = args.gate.upper()
        if want == "L3":
            print("gate L3: 本地不可评估——需要外部审计/认证证据", file=sys.stderr)
            return 1
        achieved = ev["achieved"]
        ok = LEVEL_ORDER[achieved] >= LEVEL_ORDER[want]
        if not ok:
            blockers = ev["l1_blockers"] if want == "L1" else ev["l2_blockers"]
            print(
                f"gate {want} 未通过：当前 {achieved}；阻断 "
                + ", ".join(r.id for r in blockers[:10]),
                file=sys.stderr,
            )
            return 1
        print(f"gate {want} 通过（当前 {achieved}）", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
