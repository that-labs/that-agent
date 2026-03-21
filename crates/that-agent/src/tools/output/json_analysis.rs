use serde::Serialize;

use super::tokenizer::count_tokens;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ValueStats {
    pub(crate) arrays: usize,
    pub(crate) strings: usize,
    pub(crate) max_array_len: usize,
    pub(crate) max_string_len: usize,
}

pub(crate) fn collect_value_stats(value: &serde_json::Value, stats: &mut ValueStats) {
    match value {
        serde_json::Value::String(s) => {
            stats.strings += 1;
            stats.max_string_len = stats.max_string_len.max(s.len());
        }
        serde_json::Value::Array(arr) => {
            stats.arrays += 1;
            stats.max_array_len = stats.max_array_len.max(arr.len());
            for v in arr {
                collect_value_stats(v, stats);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_value_stats(v, stats);
            }
        }
        _ => {}
    }
}

fn reduction_plan(stats: ValueStats) -> &'static [(usize, usize)] {
    // (max_string_len, max_array_len), ordered from least to most aggressive.
    const ARRAY_HEAVY: &[(usize, usize)] = &[
        (2000, 30),
        (1200, 20),
        (800, 15),
        (500, 10),
        (300, 7),
        (200, 5),
        (150, 3),
        (100, 2),
        (80, 1),
        (50, 1),
        (30, 1),
    ];
    const STRING_HEAVY: &[(usize, usize)] = &[
        (1200, 50),
        (800, 30),
        (500, 20),
        (300, 15),
        (220, 10),
        (180, 7),
        (140, 5),
        (100, 3),
        (80, 2),
        (50, 1),
        (30, 1),
        (20, 1),
    ];
    const BALANCED: &[(usize, usize)] = &[
        (1500, 30),
        (900, 20),
        (600, 15),
        (400, 10),
        (280, 7),
        (220, 5),
        (170, 3),
        (120, 2),
        (90, 1),
        (60, 1),
        (40, 1),
        (30, 1),
    ];

    if stats.max_array_len >= 100 || stats.arrays > stats.strings.saturating_mul(2) {
        ARRAY_HEAVY
    } else if stats.max_string_len >= 2000 || stats.strings > stats.arrays.saturating_mul(2) {
        STRING_HEAVY
    } else {
        BALANCED
    }
}

/// Structurally truncate a JSON value to fit within a token budget.
///
/// Strategy: parse to serde_json::Value, then progressively reduce with a
/// shape-aware reduction plan to avoid excessive trial serializations.
///
/// Always produces valid JSON.
pub(crate) fn truncate_json_value<T: Serialize>(value: &T, budget: usize) -> String {
    let mut json_value: serde_json::Value =
        serde_json::to_value(value).unwrap_or(serde_json::Value::Null);

    let mut stats = ValueStats::default();
    collect_value_stats(&json_value, &mut stats);

    for &(max_string_len, max_array_len) in reduction_plan(stats) {
        let reduced = reduce_value(&json_value, max_string_len, max_array_len);
        let serialized = serde_json::to_string(&reduced).unwrap_or_else(|_| "{}".to_string());
        let tokens = count_tokens(&serialized);
        if tokens <= budget {
            return serialized;
        }
    }

    // Last resort: aggressively reduce to skeleton.
    json_value = reduce_value(&json_value, 20, 1);
    let serialized = serde_json::to_string(&json_value).unwrap_or_else(|_| "{}".to_string());

    if count_tokens(&serialized) > budget {
        let skeleton = extract_skeleton(&json_value);
        let skeleton_str = serde_json::to_string(&skeleton).unwrap_or_else(|_| "{}".to_string());
        if count_tokens(&skeleton_str) <= budget {
            return skeleton_str;
        }
        return r#"{"budget_exhausted":true}"#.to_string();
    }

    serialized
}

/// Recursively reduce a JSON value by shortening strings and truncating arrays.
pub(crate) fn reduce_value(
    value: &serde_json::Value,
    max_string_len: usize,
    max_array_len: usize,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            if s.len() > max_string_len {
                // Truncate at a valid UTF-8 boundary without scanning chars one by one.
                let mut end = max_string_len.min(s.len());
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                let truncated = &s[..end];
                serde_json::Value::String(format!("{}...[truncated]", truncated))
            } else {
                value.clone()
            }
        }
        serde_json::Value::Array(arr) => {
            let mut reduced: Vec<serde_json::Value> = arr
                .iter()
                .take(max_array_len)
                .map(|v| reduce_value(v, max_string_len, max_array_len))
                .collect();
            if arr.len() > max_array_len {
                reduced.push(
                    serde_json::json!({"_truncated": true, "remaining": arr.len() - max_array_len}),
                );
            }
            serde_json::Value::Array(reduced)
        }
        serde_json::Value::Object(map) => {
            let reduced = map
                .iter()
                .map(|(k, v)| (k.clone(), reduce_value(v, max_string_len, max_array_len)))
                .collect();
            serde_json::Value::Object(reduced)
        }
        _ => value.clone(),
    }
}

/// Extract a skeleton from a JSON value: keep only top-level scalars
/// (numbers, bools, strings <=50 chars), drop arrays and nested objects,
/// and add a "truncated" marker.
pub(crate) fn extract_skeleton(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut skeleton = serde_json::Map::new();
            for (k, v) in map {
                match v {
                    serde_json::Value::Number(_) | serde_json::Value::Bool(_) => {
                        skeleton.insert(k.clone(), v.clone());
                    }
                    serde_json::Value::String(s) if s.len() <= 50 => {
                        skeleton.insert(k.clone(), v.clone());
                    }
                    _ => {}
                }
            }
            skeleton.insert("truncated".to_string(), serde_json::Value::Bool(true));
            serde_json::Value::Object(skeleton)
        }
        _ => serde_json::json!({"truncated": true}),
    }
}
