//! 标量过滤 oracle（P4.5.3）：**独立参考实现 + 三分区（TLP）属性化差分对照**。
//! Scalar-filter oracle: differential testing against an independent reference
//! implementation, plus three-partition (TLP) cross-validation.
//!
//! 为什么需要 oracle / Why an oracle: `filter_to_bitset` 是混合检索的**掩码来源**——
//! 它错了，检索就会返回从未满足条件的文档（假阳性），而单元测试只覆盖手挑用例。
//! 这里用「随机页 × 随机谓词树」把实现与一份**写法独立**的参考实现逐位对照，并用
//! TLP 把命中集 T 与补集 F 分别喂回实现，验证 `Q(P|D) = Q(P|T) = T` 且 `Q(P|F) = ∅`。
//! EN: `filter_to_bitset` produces the allow-list mask for hybrid search; when it
//! is wrong, search returns documents that never satisfied the predicate. The
//! oracle compares the implementation bit-for-bit against an independently
//! written reference over random pages and random predicate trees, and applies
//! TLP: feeding only the true-partition / only the false-partition back into the
//! implementation must yield all-rows / no-rows respectively.
//!
//! 参考实现为何独立 / Why the reference is independent: 若两侧共用同一份「比较后
//! 兜底 Equal」的写法，同源 bug 会同时出现在两侧、oracle 空转。故参考实现先做
//! **类型分类**（数值类 / 文本类 / 时间类 / 不可比类），再按「同类才可比」的真值
//! 表求值——与生产的单函数 `compare` 分解方式不同。
//! EN: A shared "compare then fall back to Equal" shape would hide the same bug on
//! both sides. The reference classifies operands first and evaluates a same-class
//! truth table instead of a single comparison function.

use houyi_core::{HouyiPage, ScalarPredicate, ScalarValue};
use proptest::prelude::*;
use std::cmp::Ordering;
use std::collections::HashMap;

// ── 参考实现（独立写法）／Reference implementation ───────────────────────
// EN: Reference implementation (written independently).

/// 操作数分类：同类才可比。
/// Operand classes — only same-class pairs are comparable.
#[derive(Debug, Clone, Copy)]
enum Class {
    /// 整数（i64）
    /// EN: Integer (i64).
    Int(i64),
    /// 实数（f64；整数与实数同属「数值类」，按 f64 提升比较）
    /// EN: Real number (f64; integers and reals form one "numeric" class,
    /// promoted to f64 for comparison).
    Real(f64),
    Text,
    Timestamp,
    /// 布尔、JSON 文本（数组/对象/null 载荷）、以及缺失列的 `JsonText("null")` 占位
    /// EN: Booleans, JSON text (array/object/null payloads), and the
    /// `JsonText("null")` placeholder for missing columns.
    Incomparable,
}

fn classify(v: &ScalarValue) -> Class {
    match v {
        ScalarValue::Int64(i) => Class::Int(*i),
        ScalarValue::Float64(f) => Class::Real(*f),
        ScalarValue::Text(_) => Class::Text,
        ScalarValue::Timestamp(_) => Class::Timestamp,
        ScalarValue::Bool(_) | ScalarValue::JsonText(_) => Class::Incomparable,
    }
}

/// 参考比较：先分类，再按「数值类可混比 / 文本同类 / 时间同类 / 其余不可比」求值。
/// Reference comparison — classify first, then apply the same-class rules.
fn reference_compare(a: &ScalarValue, b: &ScalarValue) -> Option<Ordering> {
    match (classify(a), classify(b)) {
        (Class::Int(x), Class::Int(y)) => Some(x.cmp(&y)),
        (Class::Int(x), Class::Real(y)) => Some((x as f64).total_cmp(&y)),
        (Class::Real(x), Class::Int(y)) => Some(x.total_cmp(&(y as f64))),
        (Class::Real(x), Class::Real(y)) => Some(x.total_cmp(&y)),
        (Class::Text, Class::Text) => match (a, b) {
            (ScalarValue::Text(x), ScalarValue::Text(y)) => Some(x.cmp(y)),
            _ => unreachable!("classify said Text"),
        },
        (Class::Timestamp, Class::Timestamp) => match (a, b) {
            (ScalarValue::Timestamp(x), ScalarValue::Timestamp(y)) => Some(x.cmp(y)),
            _ => unreachable!("classify said Timestamp"),
        },
        // 任何跨类组合（含一切涉及 Bool / JsonText 的组合）不可比。
        // EN: Any cross-class pair (including everything involving Bool / JsonText)
        // is incomparable.
        _ => None,
    }
}

/// 与实现同口径（不要用 `to_bits`）：同类型浮点按 `==`（NaN != NaN、
/// -0.0 == 0.0，与 `ScalarValue` 的派生相等一致）；数值类跨
/// `Int64`/`Float64` 按数值提升——写入侧 `json_value_to_scalar` 会把 JSON
/// `10` 与 `10.0` 落成不同变体，运行期同一列可以合法混有两者。
/// EN: mirror production semantics — `==` for same-type floats (not `to_bits`)
/// and numeric promotion across Int64/Float64.
fn reference_eq(a: &ScalarValue, b: &ScalarValue) -> bool {
    match (a, b) {
        (ScalarValue::Int64(x), ScalarValue::Int64(y)) => x == y,
        (ScalarValue::Float64(x), ScalarValue::Float64(y)) => x == y,
        (ScalarValue::Int64(x), ScalarValue::Float64(y)) => (*x as f64) == *y,
        (ScalarValue::Float64(x), ScalarValue::Int64(y)) => *x == (*y as f64),
        (ScalarValue::Text(x), ScalarValue::Text(y)) => x == y,
        (ScalarValue::Bool(x), ScalarValue::Bool(y)) => x == y,
        (ScalarValue::Timestamp(x), ScalarValue::Timestamp(y)) => x == y,
        (ScalarValue::JsonText(x), ScalarValue::JsonText(y)) => x == y,
        _ => false,
    }
}

/// 参考谓词求值：不可比 ⇒ 范围比较一律不命中。
/// Reference predicate evaluation — incomparable ⇒ every range op misses.
fn reference_matches(pred: &ScalarPredicate, value: &ScalarValue) -> bool {
    match pred {
        ScalarPredicate::Eq(lit) => reference_eq(value, lit),
        ScalarPredicate::In(list) => list.iter().any(|lit| reference_eq(value, lit)),
        ScalarPredicate::Gt(lit) => reference_compare(value, lit) == Some(Ordering::Greater),
        ScalarPredicate::Lt(lit) => reference_compare(value, lit) == Some(Ordering::Less),
        ScalarPredicate::GtEq(lit) => matches!(
            reference_compare(value, lit),
            Some(Ordering::Greater | Ordering::Equal)
        ),
        ScalarPredicate::LtEq(lit) => matches!(
            reference_compare(value, lit),
            Some(Ordering::Less | Ordering::Equal)
        ),
        ScalarPredicate::And(l, r) => reference_matches(l, value) && reference_matches(r, value),
        ScalarPredicate::Or(l, r) => reference_matches(l, value) || reference_matches(r, value),
    }
}

// ── 随机输入生成／Random input generation ──────────────────────────────
// EN: Random input generation.

/// 列名小字母表：小域 ⇒ 大量「本行缺列 / 类型漂移」组合被覆盖。
/// Small column alphabet ⇒ plenty of missing-column and type-drift combinations.
const COLUMNS: [&str; 3] = ["a", "b", "c"];

fn arb_scalar() -> impl Strategy<Value = ScalarValue> {
    prop_oneof![
        (-3i64..=3).prop_map(ScalarValue::Int64),
        prop_oneof![
            Just(0.5f64),
            Just(-1.5f64),
            Just(2.0f64),
            Just(-3.0f64),
            Just(1e17f64)
        ]
        .prop_map(ScalarValue::Float64),
        prop::bool::ANY.prop_map(ScalarValue::Bool),
        prop_oneof![Just("x"), Just("y"), Just("zz")]
            .prop_map(|s| ScalarValue::Text(s.to_string())),
        // 数组 / null 载荷：写入侧 `json_value_to_scalar` 会把它们变成 JsonText。
        // EN: Array / null payloads: the write side's `json_value_to_scalar` turns
        // them into JsonText.
        prop_oneof![Just("[1,2]"), Just("null"), Just("{\"k\":1}")]
            .prop_map(|s| ScalarValue::JsonText(s.to_string())),
        (0i64..=2).prop_map(ScalarValue::Timestamp),
    ]
}

/// 一行 = 列名的**子集**（缺列由 PAX 以 `JsonText("null")` 补齐——占位不得命中）。
/// One row = a subset of columns; PAX pads the rest with `JsonText("null")`.
fn arb_row() -> impl Strategy<Value = HashMap<String, ScalarValue>> {
    prop::collection::vec((0usize..COLUMNS.len(), arb_scalar()), 0..=COLUMNS.len()).prop_map(
        |pairs| {
            let mut row = HashMap::new();
            for (idx, value) in pairs {
                row.insert(COLUMNS[idx].to_string(), value);
            }
            row
        },
    )
}

fn arb_page() -> impl Strategy<Value = Vec<HashMap<String, ScalarValue>>> {
    prop::collection::vec(arb_row(), 1..=8)
}

fn arb_literal() -> impl Strategy<Value = ScalarValue> {
    prop_oneof![
        (-3i64..=3).prop_map(ScalarValue::Int64),
        prop_oneof![Just(0.5f64), Just(2.0f64), Just(-1.5f64)].prop_map(ScalarValue::Float64),
        prop_oneof![Just("x"), Just("y")].prop_map(|s| ScalarValue::Text(s.to_string())),
        prop::bool::ANY.prop_map(ScalarValue::Bool),
        Just(ScalarValue::JsonText("null".to_string())),
    ]
}

fn arb_leaf() -> impl Strategy<Value = ScalarPredicate> {
    (0usize..COLUMNS.len(), arb_literal()).prop_flat_map(|(col, lit)| {
        let _ = col; // 列名不进入谓词（本测试逐列对照），仅用于让策略形状统一
        prop_oneof![
            Just(ScalarPredicate::Eq(lit.clone())),
            Just(ScalarPredicate::Gt(lit.clone())),
            Just(ScalarPredicate::Lt(lit.clone())),
            Just(ScalarPredicate::GtEq(lit.clone())),
            Just(ScalarPredicate::LtEq(lit.clone())),
            prop::collection::vec(arb_literal(), 1..=3).prop_map(ScalarPredicate::In),
        ]
    })
}

fn arb_predicate() -> impl Strategy<Value = ScalarPredicate> {
    arb_leaf().prop_recursive(2, 8, 3, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone())
                .prop_map(|(l, r)| ScalarPredicate::And(Box::new(l), Box::new(r))),
            (inner.clone(), inner.clone())
                .prop_map(|(l, r)| ScalarPredicate::Or(Box::new(l), Box::new(r))),
        ]
    })
}

// ── 工具／Helpers ────────────────────────────────────────────────────
// EN: Helpers.

fn build_page(rows: &[HashMap<String, ScalarValue>]) -> HouyiPage {
    let mut page = HouyiPage::new(rows.len().max(1) * 8);
    for row in rows {
        page.insert_row(row.clone(), None);
    }
    page
}

fn column_values(rows: &[HashMap<String, ScalarValue>], column: &str) -> Vec<ScalarValue> {
    let page = build_page(rows);
    match page.scalar_column(column) {
        Some(values) => values.to_vec(),
        // 整列不存在：实现按「空掩码」处理，参考侧同样无行可命中。
        // EN: The whole column is absent: the implementation treats it as an empty
        // mask, and the reference has no rows to match either.
        None => Vec::new(),
    }
}

/// 参考命中行号（按页列序，与 `filter_to_bitset` 同口径）。
/// EN: Reference matching row numbers (in page column order, the same convention
/// as `filter_to_bitset`).
fn reference_rows(
    rows: &[HashMap<String, ScalarValue>],
    column: &str,
    pred: &ScalarPredicate,
) -> Vec<u64> {
    column_values(rows, column)
        .iter()
        .enumerate()
        .filter_map(|(idx, v)| reference_matches(pred, v).then_some(idx as u64))
        .collect()
}

// ── 属性／Properties ─────────────────────────────────────────────────
// EN: Properties.

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// 主对照：随机页 × 随机谓词树，实现的命中集必须与参考实现**逐位相同**。
    /// Primary differential property: the implementation's match set must equal the
    /// reference's, bit for bit, over random pages and random predicate trees.
    #[test]
    fn filter_matches_independent_reference(rows in arb_page(), pred in arb_predicate()) {
        let page = build_page(&rows);
        for column in COLUMNS {
            let got = page.filter_to_bitset(column, &pred);
            let want = reference_rows(&rows, column, &pred);
            prop_assert_eq!(
                &got,
                &want,
                "column {} predicate {:?} rows {:?}",
                column,
                pred,
                rows
            );
        }
    }

    /// TLP 三分区：把命中集 T 与补集 F 分别构成新页喂回实现——
    /// `Q(P|T) = T`（全命中）且 `Q(P|F) = ∅`（全不命中）。
    /// Three-partition (TLP): feeding only T back must match every row, only F must
    /// match none. A sticky/complement bug shows up here even when the reference
    /// and the implementation happen to agree on the full page.
    #[test]
    fn tlp_partitions_are_consistent(rows in arb_page(), pred in arb_predicate()) {
        let page = build_page(&rows);
        for column in COLUMNS {
            // 整列缺席时页内无值可分类：TLP 不适用（实现按空掩码处理，另有断言覆盖）。
            // When the column is absent from the page there is nothing to classify:
            // TLP does not apply (the empty-mask contract is asserted elsewhere).
            let Some(values) = page.scalar_column(column).map(<[_]>::to_vec) else {
                prop_assert!(page.filter_to_bitset(column, &pred).is_empty());
                continue;
            };
            let truth: Vec<bool> = values
                .iter()
                .map(|v| reference_matches(&pred, v))
                .collect();

            // 分区页构造：**物化被测列的有效值**（含缺列占位）。若直接搬原行，T 分区
            // 可能只剩「本就缺该列」的行，整列随之消失——那是页布局工件，不是过滤语义，
            // 属性会因此空转（首版即栽在此处）。
            // Materialize the column's effective value (padding included) so the
            // partition keeps the page layout; copying raw rows can make the column
            // vanish entirely, which is a layout artifact rather than filter semantics.
            let partition_page = |keep: fn(bool) -> bool| {
                let partition: Vec<HashMap<String, ScalarValue>> = values
                    .iter()
                    .zip(&truth)
                    .filter(|(_, t)| keep(**t))
                    .map(|(v, _)| HashMap::from([(column.to_string(), v.clone())]))
                    .collect();
                (!partition.is_empty()).then(|| build_page(&partition))
            };

            // T 分区：仅保留命中行 → 全部命中。
            // EN: T partition: keep only matching rows → all rows match.
            if let Some(true_page) = partition_page(|t| t) {
                let got = true_page.filter_to_bitset(column, &pred);
                prop_assert_eq!(
                    got.len(),
                    true_page.header().row_count as usize,
                    "T-partition must be fully matched: column {} pred {:?}",
                    column,
                    pred
                );
            }

            // F 分区：仅保留未命中行 → 一条都不命中。
            // EN: F partition: keep only non-matching rows → nothing matches.
            if let Some(false_page) = partition_page(|t| !t) {
                let got = false_page.filter_to_bitset(column, &pred);
                prop_assert!(
                    got.is_empty(),
                    "F-partition must match nothing: column {} pred {:?} got {:?}",
                    column,
                    pred,
                    got
                );
            }

            // 分区并集守恒：|T| + |F| = 页内行数（防止参考实现漏行导致空转）。
            // EN: Partition union conservation: |T| + |F| = rows in the page (guards
            // against a reference implementation that drops rows, leaving a vacuous test).
            prop_assert_eq!(
                truth.iter().filter(|t| **t).count() + truth.iter().filter(|t| !**t).count(),
                values.len()
            );
            let _ = &page;
        }
    }

    /// 多列过滤 = 单列结果交集；未知列 ⇒ 空结果（不得被当作「无此条件」跳过）。
    /// Multi-column filter = intersection of single-column results; an unknown
    /// column yields the empty set (it must not be silently skipped).
    #[test]
    fn multi_column_filter_is_intersection(
        rows in arb_page(),
        pred_a in arb_predicate(),
        pred_b in arb_predicate(),
    ) {
        let page = build_page(&rows);
        let single_a = page.filter_to_bitset("a", &pred_a);
        let single_b = page.filter_to_bitset("b", &pred_b);

        let mut want: Vec<u64> = single_a
            .iter()
            .copied()
            .filter(|idx| single_b.contains(idx))
            .collect();
        want.sort_unstable();
        want.dedup();

        let mut got = page.filter_multi_column(&[("a", &pred_a), ("b", &pred_b)]);
        got.sort_unstable();
        got.dedup();
        prop_assert_eq!(&got, &want, "rows {:?}", rows);

        // 未知列：单列口径返回空 ⇒ 多列口径同样必须为空。
        // EN: Unknown column: the single-column path returns empty ⇒ the
        // multi-column path must be empty as well.
        let missing = page.filter_multi_column(&[("nope", &pred_a)]);
        prop_assert!(
            missing.is_empty(),
            "filter on an unknown column must match nothing, got {:?}",
            missing
        );
    }
}

// ── 确定性回归（oracle 首次抓到的真实缺陷）／Deterministic regressions ──────
// EN: Deterministic regressions (real defects first caught by this oracle).

/// 回归：`price >= 5` / `price <= 100` 曾命中「payload 里没有 price 的行」
/// （PAX 以 `JsonText("null")` 补齐缺列）与「price 是字符串的行」——范围比较在
/// 类型不可比时退化为 `Equal`，而 `GtEq`/`LtEq` 把 `Equal` 视为命中，于是过滤
/// 路径出现静默假阳性：客户端拿到从未满足条件的文档。
/// Regression: `>=`/`<=` used to match rows whose payload lacks the column (PAX
/// pads with `JsonText("null")`) and rows holding a string — the range compare
/// collapsed to `Equal` on incomparable types, which `GtEq`/`LtEq` accept.
#[test]
fn range_filter_never_matches_missing_or_cross_type_values() {
    let mut rows: Vec<HashMap<String, ScalarValue>> = Vec::new();
    let mut numeric = HashMap::new();
    numeric.insert("price".to_string(), ScalarValue::Int64(10));
    rows.push(numeric);
    rows.push(HashMap::new()); // 缺 price ⇒ 占位 JsonText("null")
    let mut text = HashMap::new();
    text.insert(
        "price".to_string(),
        ScalarValue::Text("unknown".to_string()),
    );
    rows.push(text);

    let page = build_page(&rows);
    for (op, pred) in [
        ("gte", ScalarPredicate::GtEq(ScalarValue::Int64(5))),
        ("lte", ScalarPredicate::LtEq(ScalarValue::Int64(100))),
        ("gt", ScalarPredicate::Gt(ScalarValue::Int64(5))),
        ("lt", ScalarPredicate::Lt(ScalarValue::Int64(100))),
    ] {
        assert_eq!(
            page.filter_to_bitset("price", &pred),
            vec![0],
            "price {op} must only match the numeric row"
        );
    }
}

/// 回归：Int64 与 Float64 混写同列时范围比较必须按数值语义（曾因 `_ => Equal`
/// 使 `price > 5.5` 漏掉 `price = 10`、`price < 5.5` 漏掉 `price = 3`）。
/// Regression: mixed Int64/Float64 columns must compare numerically.
#[test]
fn numeric_cross_type_range_uses_numeric_semantics() {
    let mut rows: Vec<HashMap<String, ScalarValue>> = Vec::new();
    for v in [
        ScalarValue::Int64(10),
        ScalarValue::Float64(3.0),
        ScalarValue::Int64(5),
    ] {
        let mut row = HashMap::new();
        row.insert("p".to_string(), v);
        rows.push(row);
    }
    let page = build_page(&rows);
    assert_eq!(
        page.filter_to_bitset("p", &ScalarPredicate::Gt(ScalarValue::Float64(5.5))),
        vec![0]
    );
    assert_eq!(
        page.filter_to_bitset("p", &ScalarPredicate::Lt(ScalarValue::Float64(5.5))),
        vec![1, 2]
    );
    assert_eq!(
        page.filter_to_bitset("p", &ScalarPredicate::GtEq(ScalarValue::Int64(5))),
        vec![0, 2]
    );
}

/// 回归：文本列的范围比较仍是字典序（收紧类型可比性不得误伤同类比较）。
/// Regression: same-type text ranges stay lexicographic.
#[test]
fn same_type_text_range_still_lexicographic() {
    let mut rows: Vec<HashMap<String, ScalarValue>> = Vec::new();
    for v in ["apple", "banana", "cherry"] {
        let mut row = HashMap::new();
        row.insert("t".to_string(), ScalarValue::Text(v.to_string()));
        rows.push(row);
    }
    let page = build_page(&rows);
    assert_eq!(
        page.filter_to_bitset("t", &ScalarPredicate::Gt(ScalarValue::Text("apple".into()))),
        vec![1, 2]
    );
}

/// 回归（OCR 批次二十六）：`eq` / `in` 必须与范围比较同口径地做数值提升，
/// 否则 `eq 10` 漏掉 `Float64(10.0)` 而 `gte 10` 命中——同一路径自相矛盾。
/// EN: Regression (OCR batch 26): `eq` / `in` must apply numeric promotion the same
/// way range comparisons do; otherwise `eq 10` misses `Float64(10.0)` while
/// `gte 10` matches — the same path contradicts itself.
#[test]
fn eq_and_in_promote_int_float() {
    let mut page = HouyiPage::new(4);
    for value in [
        ScalarValue::Float64(10.0),
        ScalarValue::Int64(10),
        ScalarValue::Int64(11),
    ] {
        let mut row = std::collections::HashMap::new();
        row.insert("p".to_string(), value);
        page.insert_row(row, None);
    }

    let eq_int = page.filter_to_bitset("p", &ScalarPredicate::Eq(ScalarValue::Int64(10)));
    assert_eq!(eq_int, vec![0, 1], "eq 10 must match both 10 and 10.0");
    let eq_float = page.filter_to_bitset("p", &ScalarPredicate::Eq(ScalarValue::Float64(10.0)));
    assert_eq!(eq_float, vec![0, 1], "eq 10.0 must match both forms");
    let in_both = page.filter_to_bitset("p", &ScalarPredicate::In(vec![ScalarValue::Int64(10)]));
    assert_eq!(in_both, vec![0, 1], "in [10] must promote numerically");
}
