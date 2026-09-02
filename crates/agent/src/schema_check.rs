//! JSON Schema 子集校验器（task 工具 `output_schema` 用，移植 omp `ArkType` 校验语义）。
//!
//! 支持的关键字子集（足以约束模型输出结构）：
//! `type`（string/number/integer/boolean/object/array/null，可为数组）、
//! `enum` / `const`、`properties` / `required` / `additionalProperties` /
//! `minProperties` / `maxProperties`、`items` / `minItems` / `maxItems` / `uniqueItems`、
//! `minLength` / `maxLength` / `pattern`、`minimum` / `maximum` /
//! `exclusiveMinimum` / `exclusiveMaximum`、`allOf` / `anyOf` / `oneOf`。
//!
//! 不支持 `$ref` / `not` / `if-then-else` / `format` / `prefixItems`（v1 边界，校验时忽略）。
//! 错误信息带实例路径（`$.items[0].name`），供重试反馈给子 Agent。

use serde_json::Value;

/// 校验 `instance` 是否满足 `schema`。`Ok(())` 通过；`Err` 为带路径的错误清单
/// （分号分隔，前缀 `$.`）。
///
/// # Errors
/// 任一约束不满足时返回聚合错误文本。
pub fn validate_json_schema(instance: &Value, schema: &Value) -> Result<(), String> {
    let mut errors = Vec::new();
    validate_at(instance, schema, "$", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// 递归校验（`path` 以 `$` 开头）。
fn validate_at(instance: &Value, schema: &Value, path: &str, errors: &mut Vec<String>) {
    let Some(obj) = schema.as_object() else {
        // 非 object 的 schema（true/缺失）视为无约束。
        return;
    };

    // allOf / anyOf / oneOf 组合子。
    if let Some(Value::Array(all)) = obj.get("allOf") {
        for s in all {
            validate_at(instance, s, path, errors);
        }
    }
    if let Some(Value::Array(any)) = obj.get("anyOf") {
        let ok = any.iter().any(|s| {
            let mut tmp = Vec::new();
            validate_at(instance, s, path, &mut tmp);
            tmp.is_empty()
        });
        if !ok {
            errors.push(format!("{path}: 不满足 anyOf 任一分支"));
        }
    }
    if let Some(Value::Array(one)) = obj.get("oneOf") {
        let n = one
            .iter()
            .filter(|s| {
                let mut tmp = Vec::new();
                validate_at(instance, s, path, &mut tmp);
                tmp.is_empty()
            })
            .count();
        if n != 1 {
            errors.push(format!("{path}: oneOf 恰一分支不成立（命中 {n} 个）"));
        }
    }

    // type：字符串或数组。
    if let Some(ty) = obj.get("type") {
        let allowed: Vec<String> = match ty {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        if !allowed.is_empty() && !allowed.iter().any(|t| type_matches(instance, t)) {
            errors.push(format!(
                "{path}: 类型应为 {}，实际为 {}",
                allowed.join("|"),
                actual_type(instance)
            ));
        }
    }

    // enum / const。
    if let Some(Value::Array(opts)) = obj.get("enum") {
        if !opts.contains(instance) {
            errors.push(format!(
                "{path}: 值 {:?} 不在 enum {} 中",
                instance,
                Value::Array(opts.clone())
            ));
        }
    }
    if let Some(c) = obj.get("const") {
        if c != instance {
            errors.push(format!("{path}: 值 {instance:?} ≠ const {c:?}"));
        }
    }

    // 数值界限（integer/number）。
    if let Some(n) = instance.as_f64() {
        check_bound(obj.get("minimum"), n, false, path, errors);
        check_bound(obj.get("maximum"), n, true, path, errors);
        if let Some(Value::Number(x)) = obj.get("exclusiveMinimum") {
            if n <= x.as_f64().unwrap_or(f64::NEG_INFINITY) {
                errors.push(format!("{path}: {n} 未严格大于 exclusiveMinimum"));
            }
        }
        if let Some(Value::Number(x)) = obj.get("exclusiveMaximum") {
            if n >= x.as_f64().unwrap_or(f64::INFINITY) {
                errors.push(format!("{path}: {n} 未严格小于 exclusiveMaximum"));
            }
        }
    }

    // 字符串长度与模式。
    if let Some(s) = instance.as_str() {
        if let Some(Value::Number(min)) = obj.get("minLength") {
            if (s.chars().count() as u64) < min.as_u64().unwrap_or(0) {
                errors.push(format!(
                    "{path}: 长度 {} < minLength {}",
                    s.chars().count(),
                    min
                ));
            }
        }
        if let Some(Value::Number(max)) = obj.get("maxLength") {
            if let Some(m) = max.as_u64() {
                if (s.chars().count() as u64) > m {
                    errors.push(format!(
                        "{path}: 长度 {} > maxLength {m}",
                        s.chars().count()
                    ));
                }
            }
        }
        if let Some(Value::String(p)) = obj.get("pattern") {
            if let Ok(re) = regex::Regex::new(p) {
                if !re.is_match(s) {
                    errors.push(format!("{path}: 不匹配 pattern {p:?}"));
                }
            }
        }
    }

    // 数组。
    if let Some(arr) = instance.as_array() {
        if let Some(item_schema) = obj.get("items") {
            for (i, v) in arr.iter().enumerate() {
                validate_at(v, item_schema, &format!("{path}[{i}]"), errors);
            }
        }
        if let Some(Value::Number(min)) = obj.get("minItems") {
            if (arr.len() as u64) < min.as_u64().unwrap_or(0) {
                errors.push(format!("{path}: 项数 {} < minItems {}", arr.len(), min));
            }
        }
        if let Some(Value::Number(max)) = obj.get("maxItems") {
            if let Some(m) = max.as_u64() {
                if (arr.len() as u64) > m {
                    errors.push(format!("{path}: 项数 {} > maxItems {m}", arr.len()));
                }
            }
        }
        if obj
            .get("uniqueItems")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && arr
                .iter()
                .any(|v| arr.iter().filter(|o| *o == v).count() > 1)
        {
            errors.push(format!("{path}: 存在重复元素（uniqueItems）"));
        }
    }

    // 对象。
    if let Some(map) = instance.as_object() {
        if let Some(Value::Array(req)) = obj.get("required") {
            for r in req.iter().filter_map(Value::as_str) {
                if !map.contains_key(r) {
                    errors.push(format!("{path}.{r}: 缺少必填属性"));
                }
            }
        }
        if let Some(Value::Object(props)) = obj.get("properties") {
            for (k, v) in map {
                if let Some(ps) = props.get(k) {
                    validate_at(v, ps, &format!("{path}.{k}"), errors);
                }
            }
        }
        if let Some(ap) = obj.get("additionalProperties") {
            let declared = obj
                .get("properties")
                .and_then(Value::as_object)
                .map(|p| p.keys().cloned().collect::<Vec<String>>())
                .unwrap_or_default();
            match ap {
                Value::Bool(false) => {
                    for k in map.keys() {
                        if !declared.contains(k) {
                            errors.push(format!(
                                "{path}: 未声明的属性 `{k}`（additionalProperties=false）"
                            ));
                        }
                    }
                }
                Value::Object(_) => {
                    for (k, v) in map {
                        if !declared.contains(k) {
                            if let Some(s) = ap.as_object() {
                                let sub = Value::Object(s.clone());
                                validate_at(v, &sub, &format!("{path}.{k}"), errors);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(Value::Number(min)) = obj.get("minProperties") {
            if (map.len() as u64) < min.as_u64().unwrap_or(0) {
                errors.push(format!("{path}: 属性数 {} < minProperties", map.len()));
            }
        }
        if let Some(Value::Number(max)) = obj.get("maxProperties") {
            if let Some(m) = max.as_u64() {
                if (map.len() as u64) > m {
                    errors.push(format!("{path}: 属性数 {} > maxProperties {m}", map.len()));
                }
            }
        }
    }
}

/// 数值界限（minimum/maximum；draft-06 数值形式；draft-04 布尔形式忽略）。
fn check_bound(bound: Option<&Value>, n: f64, is_max: bool, path: &str, errors: &mut Vec<String>) {
    if let Some(Value::Number(b)) = bound {
        let b = b.as_f64().unwrap_or(f64::NAN);
        let violated = if is_max { n > b } else { n < b };
        if violated {
            let kw = if is_max { "maximum" } else { "minimum" };
            errors.push(format!("{path}: {n} 越界（{kw}={b}）"));
        }
    }
}

/// 实例类型是否匹配 schema 类型名。
fn type_matches(v: &Value, t: &str) -> bool {
    match t {
        "string" => v.is_string(),
        "number" => v.is_number(),
        // JSON Schema 的 integer 接受无小数部分的 number。
        "integer" => v
            .as_f64()
            .is_some_and(|n| n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "null" => v.is_null(),
        _ => true, // 未知类型名不约束（宽容）。
    }
}

/// 实例类型的人类可读名（错误信息用）。
fn actual_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() && n.as_f64().is_some_and(|f| f.fract() != 0.0) => "number",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// 从子 Agent 文本输出中提取 JSON 值：剥 markdown 围栏 → 直接解析 →
/// 回退「首个 `{`/`[` 到匹配末段」子串解析。全部失败返回 `None`。
#[must_use]
pub fn extract_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    // 1) ```json … ``` / ``` … ``` 围栏。
    let stripped = strip_code_fence(trimmed).unwrap_or(trimmed);
    if let Ok(v) = serde_json::from_str::<Value>(stripped.trim()) {
        return Some(v);
    }
    // 2) 回退：首 { 或 [ 到最后一个 } 或 ]。
    let (open, close) = match (stripped.find('{'), stripped.find('[')) {
        (Some(b), Some(a)) if b < a => ('{', '}'),
        (Some(_), Some(_)) => ('[', ']'),
        (Some(_), None) => ('{', '}'),
        (None, Some(_)) => ('[', ']'),
        (None, None) => return None,
    };
    let start = stripped.find(open)?;
    let end = stripped.rfind(close)?;
    if end > start {
        serde_json::from_str(stripped[start..=end].trim()).ok()
    } else {
        None
    }
}

/// 剥掉整段包裹的 markdown 代码围栏（仅当首行以 ``` 开头时）。
fn strip_code_fence(text: &str) -> Option<&str> {
    let first_line = text.lines().next()?;
    if !first_line.trim_start().starts_with("```") {
        return None;
    }
    let body = &text[first_line.len()..];
    let last_fence = body.rfind("```")?;
    Some(&body[..last_fence])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_object_shape() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "minLength": 1},
                "count": {"type": "integer", "minimum": 0},
                "tags": {"type": "array", "items": {"type": "string"}, "uniqueItems": true}
            },
            "required": ["name", "count"],
            "additionalProperties": false
        });
        assert!(
            validate_json_schema(
                &json!({"name": "x", "count": 3, "tags": ["a", "b"]}),
                &schema
            )
            .is_ok()
        );
        // 缺必填 + 越界 + 重复。
        let err = validate_json_schema(
            &json!({"name": "", "count": -1, "tags": ["a", "a"], "extra": 1}),
            &schema,
        )
        .unwrap_err();
        assert!(err.contains("minLength") || err.contains("长度"), "{err}");
        assert!(err.contains("minimum") || err.contains("越界"), "{err}");
        assert!(err.contains("uniqueItems"), "{err}");
        assert!(err.contains("extra"), "{err}");
        // 必填缺失。
        let err = validate_json_schema(&json!({"name": "x"}), &schema).unwrap_err();
        assert!(err.contains("count"), "{err}");
    }

    #[test]
    fn validates_type_array_and_enum() {
        let schema = json!({"type": ["string", "null"], "enum": ["a", "b", null]});
        assert!(validate_json_schema(&json!("a"), &schema).is_ok());
        assert!(validate_json_schema(&json!(null), &schema).is_ok());
        assert!(validate_json_schema(&json!("c"), &schema).is_err());
        assert!(validate_json_schema(&json!(1), &schema).is_err());
    }

    #[test]
    fn integer_accepts_whole_numbers() {
        let schema = json!({"type": "integer"});
        assert!(validate_json_schema(&json!(5), &schema).is_ok());
        assert!(validate_json_schema(&json!(5.0), &schema).is_ok());
        assert!(validate_json_schema(&json!(5.5), &schema).is_err());
    }

    #[test]
    fn validates_nested_paths() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {"type": "array", "items": {"type": "object",
                    "properties": {"id": {"type": "string"}}, "required": ["id"]}}
            }
        });
        let err =
            validate_json_schema(&json!({"items": [{"id": "1"}, {"x": 2}]}), &schema).unwrap_err();
        assert!(err.contains("$.items[1].id"), "{err}");
    }

    #[test]
    fn combinator_keywords() {
        let schema = json!({"anyOf": [{"type": "string"}, {"type": "number"}]});
        assert!(validate_json_schema(&json!("s"), &schema).is_ok());
        assert!(validate_json_schema(&json!(true), &schema).is_err());
        let one = json!({"oneOf": [{"type": "number"}, {"type": "integer"}]});
        // 5.5 只满足 number；5.0 同时满足两者 → oneOf 失败。
        assert!(validate_json_schema(&json!(5.5), &one).is_ok());
        assert!(validate_json_schema(&json!(5.0), &one).is_err());
    }

    #[test]
    fn pattern_matching() {
        let schema = json!({"type": "string", "pattern": "^[a-z]+$"});
        assert!(validate_json_schema(&json!("abc"), &schema).is_ok());
        assert!(validate_json_schema(&json!("Abc"), &schema).is_err());
    }

    #[test]
    fn extracts_fenced_and_embedded_json() {
        assert_eq!(
            extract_json("```json\n{\"a\": 1}\n```"),
            Some(json!({"a": 1}))
        );
        assert_eq!(
            extract_json("前置说明\n``` \n[1, 2]\n```"),
            Some(json!([1, 2]))
        );
        assert_eq!(
            extract_json("结果如下：{\"a\": {\"b\": 2}} 以上。"),
            Some(json!({"a": {"b": 2}}))
        );
        // 嵌套花括号取首 { 到末 }。
        assert_eq!(extract_json("x {\"a\":1} y {"), Some(json!({"a": 1})));
        assert_eq!(extract_json("没有 json"), None);
    }

    #[test]
    fn non_object_schema_is_noop() {
        assert!(validate_json_schema(&json!(1), &json!(null)).is_ok());
        assert!(validate_json_schema(&json!(1), &json!(true)).is_ok());
    }
}
