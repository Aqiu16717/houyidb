//! JSON → PAX 标量值转换：payload 写入与混合检索过滤字面值共用同一套规则。
//! JSON-to-PAX scalar conversion shared by payload writes and hybrid-search filter literals.
//!
//! 为什么独立成模块：转换规则是「写入侧」与「过滤侧」共同的契约——两侧必须对同一个
//! JSON 值得出完全相同的 [`ScalarValue`]，否则过滤永不命中（历史上曾出现过滤侧把
//! `"red"` 包成 `Text("\"red\"")` 的引号 bug）。放在独立模块可避免 types 层（DTO）
//! 反向依赖 http_handler 层（处理器）的层级倒置。
//! EN: This is the shared contract between the write path and the filter path — both
//! must map a JSON value to the exact same `ScalarValue`, or filters silently miss
//! everything (the old filter path produced `Text("\"red\"")`). A standalone module
//! also keeps the DTO layer (types) from depending downward on the handler layer.
//!
//! [`filter_predicate`] 在转换之外再承担「过滤操作符契约」：op 白名单、字面值形状
//! 校验与谓词构造只有这一个入口，HTTP 边界的预锁校验与持锁应用共用同一函数。

use houyi_core::{ScalarPredicate, ScalarValue};

/// 把 JSON 值转换为标量列值（payload 写入与混合检索过滤字面值共用）。
/// Convert a JSON value into the PAX scalar type used by both payload writes and
/// hybrid-search filter literals.
///
/// - i64 范围内的整数 → [`ScalarValue::Int64`]，其余有限数字 → [`ScalarValue::Float64`]
///   （超出 ±2^53 的整数经 f64 会有精度损失——JSON 数字本身无法无损承载）
/// - 布尔 → [`ScalarValue::Bool`]；字符串 → [`ScalarValue::Text`]（保留裸字符串，**不额外加引号**）
/// - null/数组/对象 → [`ScalarValue::JsonText`]（整体序列化为 JSON 文本）
///
/// EN: integers within i64 become Int64, every other finite number Float64
/// (integers beyond ±2^53 lose f64 precision — JSON numbers cannot carry them
/// losslessly); booleans Bool; strings Text with their raw content (no extra
/// quoting — this is what lets a JSON `"red"` match a `Text("red")` payload
/// value); null/array/object become JsonText.
pub(crate) fn json_value_to_scalar(value: &serde_json::Value) -> ScalarValue {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ScalarValue::Int64(i)
            } else if let Some(f) = n.as_f64() {
                ScalarValue::Float64(f)
            } else {
                // 防御性兜底：默认 serde_json（未开 arbitrary_precision）下任何 Number
                // 必为 i64 或有限 f64，此分支当前不可达；保留以防依赖特性变更。
                // Defensive fallback: with default serde_json every Number is i64 or
                // finite f64, so this is unreachable today; kept against future feature changes.
                ScalarValue::JsonText(n.to_string())
            }
        }
        serde_json::Value::Bool(b) => ScalarValue::Bool(*b),
        serde_json::Value::String(s) => ScalarValue::Text(s.clone()),
        serde_json::Value::Null | serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            ScalarValue::JsonText(value.to_string())
        }
    }
}

/// 标量列值 → JSON 的反向转换（payload 回读 / `include_payload` 检索响应专用）。
/// Reverse conversion (ScalarValue → JSON) for payload read-back in search responses.
///
/// 与 [`json_value_to_scalar`] 构成尽量无损的往返：Int64/Float64/Bool/Text 直转；
/// Timestamp 按 Unix 秒数字返回；JsonText 解析回原始 JSON 值（数组/对象/null 复原），
/// 解析失败退化为裸字符串（防御性：页内 JsonText 必由本模块序列化产生，正常必可解析）。
/// EN: Best-effort lossless round trip with `json_value_to_scalar`: direct mapping for
/// scalars; Timestamp becomes a Unix-seconds number; JsonText is parsed back into the
/// original JSON value, falling back to a plain string if parsing ever fails.
pub(crate) fn scalar_to_json(value: &ScalarValue) -> serde_json::Value {
    match value {
        ScalarValue::Int64(i) => serde_json::Value::Number((*i).into()),
        ScalarValue::Float64(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        ScalarValue::Bool(b) => serde_json::Value::Bool(*b),
        ScalarValue::Text(s) => serde_json::Value::String(s.clone()),
        ScalarValue::Timestamp(t) => serde_json::Value::Number((*t).into()),
        ScalarValue::JsonText(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::String(s.clone()))
        }
    }
}

/// 由 label + payload 原始 JSON 构建一行标量（collection upsert 写入侧与
/// collections WAL 重放侧共用——两侧必须走同一函数，重放才是逐字节语义等价）。
/// Build one scalar row from the original label + payload JSON. Shared by the
/// collection-upsert write path and the collections-WAL replay path: both sides
/// must run this exact function for replay to be byte-for-byte equivalent.
///
/// payload 先填、label 后写；保留键 "label" 的冲突在 HTTP 边界已被 400 拦截
/// （重放记录由写入侧产生，天然不含冲突）。
/// EN: payload first, label second; the reserved "label" collision is rejected
/// with 400 at the HTTP edge (replay records originate from validated writes).
pub(crate) fn build_collection_scalar_row(
    label: Option<&str>,
    payload: Option<&serde_json::Map<String, serde_json::Value>>,
) -> std::collections::HashMap<String, ScalarValue> {
    let mut row = std::collections::HashMap::new();
    if let Some(payload) = payload {
        for (column, value) in payload {
            row.insert(column.clone(), json_value_to_scalar(value));
        }
    }
    if let Some(label) = label {
        row.insert("label".to_string(), ScalarValue::Text(label.to_string()));
    }
    row
}

/// 返回 JSON 值的类型名，用于 400 诊断信息（不回显用户内容，避免泄漏/注入）。
/// Type label for 400 diagnostics (the value itself is never echoed).
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// 由操作符与字面值构造结构化过滤谓词：**操作符白名单 + 字面值形状校验 +
/// JSON→Scalar 转换 + 谓词构造的唯一入口**。持锁前的边界校验与持锁后的过滤
/// 应用都必须经由本函数，调用方不得再解释一遍操作符。
/// Build a structured-filter predicate. This is the single construction point:
/// op whitelist + literal-shape validation + JSON→scalar conversion + predicate
/// construction. Both the pre-lock validation and the under-lock application go
/// through it; callers must never interpret the op a second time.
///
/// 为什么收敛成一个函数（WHY）：旧实现先在持锁前用一个 match 校验形状，再在
/// 持锁后用第二个 match 构造谓词并以 `unreachable!` 兜底——两份操作符解释一旦
/// 漂移（新增操作符只改一处），运行期就是 panic。
/// EN: The old code validated ops in one match and built predicates in another,
/// bailing out with `unreachable!`. Two interpretations of the op set drift apart
/// silently and become a runtime panic; this function makes them the same code.
///
/// 形状规则 / Shape rules：
/// - `in` 的右值必须是 JSON 数组，且元素只能是布尔/数字/字符串；
///   数组、对象、null 元素一律拒绝（`Err` → 调用方返回 400）。
/// - `eq`/`gt`/`lt`/`gte`/`lte` 的右值不得是数组/对象/null。
/// - 其余操作符按未知操作符拒绝。
pub(crate) fn filter_predicate(
    column: &str,
    op: &str,
    value: &serde_json::Value,
) -> Result<ScalarPredicate, String> {
    // in 单独成支：右值必须是数组；元素与标量操作符遵循同一形状规则。
    // `in` is its own arm: the literal must be an array whose elements obey
    // the same scalar-shape rule as the comparison operators.
    if op == "in" {
        let array = match value {
            serde_json::Value::Array(array) => array,
            _ => {
                return Err(format!(
                    "filter 'in' on column '{column}' expects a JSON array, got {}",
                    json_type_name(value)
                ))
            }
        };
        for (index, element) in array.iter().enumerate() {
            if matches!(
                element,
                serde_json::Value::Array(_)
                    | serde_json::Value::Object(_)
                    | serde_json::Value::Null
            ) {
                return Err(format!(
                    "filter 'in' on column '{column}' expects scalar array elements \
                     (boolean/number/string), got {} element at index {index}",
                    json_type_name(element)
                ));
            }
        }
        return Ok(ScalarPredicate::In(
            array.iter().map(json_value_to_scalar).collect(),
        ));
    }

    // 标量操作符：先做形状校验并转换字面值；未知操作符在此拦截（与旧实现诊断一致）。
    // Scalar ops: validate shape and convert the literal; unknown ops are rejected
    // here with the same diagnostic as the old code.
    let scalar = match op {
        "eq" | "gt" | "lt" | "gte" | "lte" => {
            if matches!(
                value,
                serde_json::Value::Array(_)
                    | serde_json::Value::Object(_)
                    | serde_json::Value::Null
            ) {
                return Err(format!(
                    "filter '{op}' on column '{column}' expects a scalar value, got {}",
                    json_type_name(value)
                ));
            }
            json_value_to_scalar(value)
        }
        other => return Err(format!("unknown filter op: {other}")),
    };

    // 白名单已在上面收敛；兜底分支返回错误而非 panic（不保留 unreachable!）。
    // The whitelist was applied above; the fallback returns an error instead of
    // panicking — there is deliberately no `unreachable!` in this path.
    let predicate = match op {
        "eq" => ScalarPredicate::Eq(scalar),
        "gt" => ScalarPredicate::Gt(scalar),
        "lt" => ScalarPredicate::Lt(scalar),
        "gte" => ScalarPredicate::GtEq(scalar),
        "lte" => ScalarPredicate::LtEq(scalar),
        other => return Err(format!("unknown filter op: {other}")),
    };
    Ok(predicate)
}

#[cfg(test)]
mod tests {
    use super::{filter_predicate, json_value_to_scalar};
    use houyi_core::{ScalarPredicate, ScalarValue};
    use serde_json::json;

    #[test]
    fn integer_json_becomes_int64() {
        assert_eq!(json_value_to_scalar(&json!(10)), ScalarValue::Int64(10));
        assert_eq!(json_value_to_scalar(&json!(-7)), ScalarValue::Int64(-7));
    }

    #[test]
    fn float_json_becomes_float64() {
        assert_eq!(json_value_to_scalar(&json!(1.5)), ScalarValue::Float64(1.5));
        assert_eq!(
            json_value_to_scalar(&json!(0.25)),
            ScalarValue::Float64(0.25)
        );
    }

    #[test]
    fn bool_json_becomes_bool() {
        assert_eq!(json_value_to_scalar(&json!(true)), ScalarValue::Bool(true));
        assert_eq!(
            json_value_to_scalar(&json!(false)),
            ScalarValue::Bool(false)
        );
    }

    #[test]
    fn string_json_is_raw_text_without_extra_quotes() {
        // 回归：旧过滤路径把 JSON 字符串序列化成 Text("\"red\"")，eq 永不命中。
        // Regression: the old filter path wrapped strings as Text("\"red\"").
        assert_eq!(
            json_value_to_scalar(&json!("red")),
            ScalarValue::Text("red".to_string())
        );
    }

    #[test]
    fn nested_object_becomes_json_text() {
        match json_value_to_scalar(&json!({"a": {"b": 1}})) {
            ScalarValue::JsonText(s) => assert!(s.contains("\"b\":1") || s.contains("\"b\": 1")),
            other => panic!("expected JsonText, got {other:?}"),
        }
    }

    #[test]
    fn array_becomes_json_text() {
        assert_eq!(
            json_value_to_scalar(&json!([1, 2, 3])),
            ScalarValue::JsonText("[1,2,3]".to_string())
        );
    }

    #[test]
    fn null_becomes_json_text_null() {
        assert_eq!(
            json_value_to_scalar(&json!(null)),
            ScalarValue::JsonText("null".to_string())
        );
    }

    #[test]
    fn scalar_to_json_round_trips_write_side_values() {
        use super::scalar_to_json;
        for v in [
            json!(10),
            json!(-7),
            json!(1.5),
            json!(true),
            json!("red"),
            json!([1, 2, 3]),
            json!({"a": 1}),
            json!(null),
        ] {
            assert_eq!(
                scalar_to_json(&json_value_to_scalar(&v)),
                v,
                "round trip must preserve {v}"
            );
        }
        // Timestamp 无写入侧来源（保留类型），按数字透出。
        // EN: Timestamp has no write-side source (type retained); emitted as a number.
        assert_eq!(
            scalar_to_json(&ScalarValue::Timestamp(1_700_000_000)),
            json!(1_700_000_000)
        );
    }

    #[test]
    fn filter_predicate_builds_scalar_and_in_predicates() {
        assert!(matches!(
            filter_predicate("c", "eq", &json!("red")),
            Ok(ScalarPredicate::Eq(ScalarValue::Text(s))) if s == "red"
        ));
        assert!(matches!(
            filter_predicate("c", "gte", &json!(3)),
            Ok(ScalarPredicate::GtEq(ScalarValue::Int64(3)))
        ));
        // in 数组元素共享 json_value_to_scalar 契约：裸字符串不加引号。
        // in elements share the conversion contract: raw strings stay unquoted.
        match filter_predicate("c", "in", &json!(["red", 7, true])) {
            Ok(ScalarPredicate::In(list)) => assert_eq!(
                list,
                vec![
                    ScalarValue::Text("red".to_string()),
                    ScalarValue::Int64(7),
                    ScalarValue::Bool(true),
                ]
            ),
            other => panic!("expected In predicate, got {other:?}"),
        }
    }

    #[test]
    fn filter_predicate_rejects_unknown_op_without_panic() {
        let err = filter_predicate("c", "regex", &json!("red.*")).expect_err("unknown op");
        assert!(err.contains("regex"), "diagnostic must name the op: {err}");
    }

    #[test]
    fn filter_predicate_rejects_bad_shapes() {
        // in 右值必须是数组。
        // EN: The right-hand side of `in` must be an array.
        assert!(filter_predicate("c", "in", &json!("red")).is_err());
        // in 元素不得是数组/对象/null（OCR 边界穷尽）。
        // EN: `in` elements must not be arrays/objects/null (OCR exhaustive boundaries).
        assert!(filter_predicate("c", "in", &json!([[1, 2]])).is_err());
        assert!(filter_predicate("c", "in", &json!([{"a": 1}])).is_err());
        assert!(filter_predicate("c", "in", &json!([null])).is_err());
        // 合法元素与非法元素混排也要拒绝，并定位到下标 1。
        // EN: Mixed valid/invalid elements are rejected, with the index pointing at 1.
        let err = filter_predicate("c", "in", &json!(["red", null]))
            .expect_err("null element must be rejected");
        assert!(
            err.contains("index 1"),
            "diagnostic must name the index: {err}"
        );
        // 标量操作符拒绝数组/对象/null。
        // EN: Scalar operators reject arrays/objects/null.
        assert!(filter_predicate("c", "gt", &json!([1])).is_err());
        assert!(filter_predicate("c", "eq", &json!({"a": 1})).is_err());
        assert!(filter_predicate("c", "lt", &json!(null)).is_err());
    }
}
