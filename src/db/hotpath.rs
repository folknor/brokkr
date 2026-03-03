//! Hotpath JSON report parsing.

use super::{HotpathData, HotpathFunction, HotpathThread, KvPair};

/// Convert a hotpath JSON report (from the hotpath crate) into a `HotpathData` struct.
///
/// Used by `run_hotpath_capture()` and the v2→v3 migration.
pub fn hotpath_data_from_json(extra: &serde_json::Value) -> Option<HotpathData> {
    let obj = extra.as_object()?;

    let has_timing = obj.contains_key("functions_timing");
    let has_alloc = obj.contains_key("functions_alloc");
    let has_threads = obj.contains_key("threads");

    if !has_timing && !has_alloc && !has_threads {
        return None;
    }

    let mut functions = Vec::new();

    if let Some(timing) = obj.get("functions_timing") {
        parse_functions_section(timing, "timing", &mut functions);
    }
    if let Some(alloc) = obj.get("functions_alloc") {
        parse_functions_section(alloc, "alloc", &mut functions);
    }

    let mut threads = Vec::new();
    let mut thread_summary = Vec::new();

    if let Some(threads_val) = obj.get("threads")
        && let Some(threads_obj) = threads_val.as_object()
    {
        for key in &["rss_bytes", "total_alloc_bytes", "total_dealloc_bytes", "alloc_dealloc_diff"] {
            if let Some(v) = threads_obj.get(*key).and_then(|v| v.as_str()) {
                thread_summary.push(KvPair::text(format!("threads.{key}"), v));
            }
        }
        if let Some(data) = threads_obj.get("data").and_then(|v| v.as_array()) {
            for entry in data {
                let s = |k: &str| entry.get(k).and_then(|v| v.as_str()).map(String::from);
                threads.push(HotpathThread {
                    name: s("name").unwrap_or_default(),
                    status: s("status"),
                    cpu_percent: s("cpu_percent"),
                    cpu_percent_max: s("cpu_percent_max"),
                    cpu_user: s("cpu_user"),
                    cpu_sys: s("cpu_sys"),
                    cpu_total: s("cpu_total"),
                    alloc_bytes: s("alloc_bytes"),
                    dealloc_bytes: s("dealloc_bytes"),
                    mem_diff: s("mem_diff"),
                });
            }
        }
    }

    if functions.is_empty() && threads.is_empty() {
        return None;
    }

    Some(HotpathData { functions, threads, thread_summary })
}

fn parse_functions_section(value: &serde_json::Value, section: &str, out: &mut Vec<HotpathFunction>) {
    let Some(obj) = value.as_object() else { return };
    let description = obj.get("description").and_then(|v| v.as_str()).map(String::from);
    let Some(data) = obj.get("data").and_then(|v| v.as_array()) else { return };

    for (ordinal, entry) in data.iter().enumerate() {
        let s = |k: &str| entry.get(k).and_then(|v| v.as_str()).map(String::from);
        #[allow(clippy::cast_possible_wrap)]
        let ord = ordinal as i64;
        out.push(HotpathFunction {
            section: section.to_owned(),
            description: description.clone(),
            ordinal: ord,
            name: s("name").unwrap_or_default(),
            calls: entry.get("calls").and_then(serde_json::Value::as_i64),
            avg: s("avg"),
            total: s("total"),
            percent_total: s("percent_total"),
            p50: s("p50"),
            p95: s("p95"),
            p99: s("p99"),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::hotpath_data_from_json;

    #[test]
    fn returns_none_when_no_hotpath_sections_exist() {
        let json = serde_json::json!({
            "unrelated": "value",
        });
        assert!(hotpath_data_from_json(&json).is_none());
    }

    #[test]
    fn parses_timing_and_alloc_and_threads() {
        let json = serde_json::json!({
            "functions_timing": {
                "description": "timing section",
                "data": [
                    {
                        "name": "decode",
                        "calls": 12,
                        "avg": "2 ms",
                        "total": "24 ms",
                        "percent_total": "40%",
                        "p50": "1.8 ms",
                        "p95": "3.0 ms",
                        "p99": "3.8 ms"
                    }
                ]
            },
            "functions_alloc": {
                "description": "alloc section",
                "data": [
                    {
                        "name": "decode",
                        "calls": 12,
                        "avg": "1 KB",
                        "total": "12 KB",
                        "percent_total": "30%",
                        "p50": "0.8 KB",
                        "p95": "1.5 KB",
                        "p99": "2.0 KB"
                    }
                ]
            },
            "threads": {
                "rss_bytes": "1000",
                "alloc_dealloc_diff": "300",
                "data": [
                    {
                        "name": "worker-0",
                        "status": "running",
                        "cpu_percent": "55%",
                        "cpu_total": "12 s",
                        "alloc_bytes": "5 MB",
                        "dealloc_bytes": "4 MB",
                        "mem_diff": "1 MB"
                    }
                ]
            }
        });

        let parsed = hotpath_data_from_json(&json).expect("hotpath");
        assert_eq!(parsed.functions.len(), 2);
        assert_eq!(parsed.functions[0].section, "timing");
        assert_eq!(parsed.functions[1].section, "alloc");
        assert_eq!(parsed.threads.len(), 1);
        assert_eq!(parsed.threads[0].name, "worker-0");
        assert!(parsed.thread_summary.iter().any(|kv| kv.key == "threads.rss_bytes"));
        assert!(parsed.thread_summary.iter().any(|kv| kv.key == "threads.alloc_dealloc_diff"));
    }

    #[test]
    fn returns_none_when_sections_are_present_but_empty() {
        let json = serde_json::json!({
            "functions_timing": { "data": [] },
            "threads": { "data": [] }
        });
        assert!(hotpath_data_from_json(&json).is_none());
    }
}
