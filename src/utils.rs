use serde_json::Value;

/// Full account list for a transaction: static keys, then accounts loaded
/// from Address Lookup Tables (writable, then readonly).
pub fn resolve_account_keys(tx: &Value) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for k in tx["transaction"]["message"]["accountKeys"].as_array().unwrap() {
        keys.push(k.as_str().unwrap().to_string());
    }
    if let Some(w) = tx["meta"]["loadedAddresses"]["writable"].as_array() {
        for k in w {
            keys.push(k.as_str().unwrap().to_string());
        }
    }
    if let Some(r) = tx["meta"]["loadedAddresses"]["readonly"].as_array() {
        for k in r {
            keys.push(k.as_str().unwrap().to_string());
        }
    }
    keys
}