use serde_json::Value;

/// Full account list for a transaction: static keys, then accounts loaded
/// from Address Lookup Tables (writable, then readonly).
///
/// Missing `loadedAddresses` is normal (legacy / simple transactions have no
/// lookup tables), so we simply skip it — it is not an error.
pub fn resolve_account_keys(tx: &Value) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();

    let mut push_all = |v: &Value| {
        if let Some(arr) = v.as_array() {
            keys.extend(arr.iter().filter_map(|k| k.as_str().map(String::from)));
        }
    };
    push_all(&tx["transaction"]["message"]["accountKeys"]);
    push_all(&tx["meta"]["loadedAddresses"]["writable"]);
    push_all(&tx["meta"]["loadedAddresses"]["readonly"]);

    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn static_then_writable_then_readonly() {
        let tx = json!({
            "transaction": { "message": { "accountKeys": ["A", "B"] } },
            "meta": { "loadedAddresses": { "writable": ["W"], "readonly": ["R"] } }
        });
        assert_eq!(resolve_account_keys(&tx), vec!["A", "B", "W", "R"]);
    }

    #[test]
    fn legacy_transaction_without_lookup_tables() {
        let tx = json!({
            "transaction": { "message": { "accountKeys": ["A"] } },
            "meta": {}
        });
        assert_eq!(resolve_account_keys(&tx), vec!["A"]);
    }

    #[test]
    fn malformed_input_yields_empty_list() {
        assert!(resolve_account_keys(&json!({})).is_empty());
    }
}
