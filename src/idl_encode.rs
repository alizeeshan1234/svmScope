//! Dynamic Borsh encoding for values described by an Anchor IDL.

use std::str::FromStr;

use serde_json::{Map, Value};
use solana_address::Address;

use crate::error::{Error, Result};

pub(crate) fn encode_arguments(
    idl: &Value,
    instruction: &Value,
    args: &Map<String, Value>,
    output: &mut Vec<u8>,
) -> Result<()> {
    for argument in instruction
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = argument
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::InvalidSpec("IDL argument is missing its name".into()))?;
        let ty = argument
            .get("type")
            .ok_or_else(|| Error::InvalidSpec(format!("IDL argument {name} has no type")))?;
        let value = args.get(name).ok_or_else(|| Error::MissingArgument {
            method: instruction
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>")
                .to_string(),
            argument: name.to_string(),
        })?;
        encode_value(idl, name, ty, value, output)?;
    }
    Ok(())
}

fn encode_value(
    idl: &Value,
    path: &str,
    ty: &Value,
    value: &Value,
    output: &mut Vec<u8>,
) -> Result<()> {
    if let Some(primitive) = ty.as_str() {
        return encode_primitive(path, primitive, value, output);
    }

    if let Some(inner) = ty.get("vec") {
        let values = value
            .as_array()
            .ok_or_else(|| encoding_error(path, "expected an array"))?;
        write_len(path, values.len(), output)?;
        for (index, value) in values.iter().enumerate() {
            encode_value(idl, &format!("{path}[{index}]"), inner, value, output)?;
        }
        return Ok(());
    }

    if let Some(inner) = ty.get("option") {
        if value.is_null() {
            output.push(0);
        } else {
            output.push(1);
            encode_value(idl, path, inner, value, output)?;
        }
        return Ok(());
    }

    if let Some(array) = ty.get("array").and_then(Value::as_array) {
        let inner = array
            .first()
            .ok_or_else(|| encoding_error(path, "array type is missing its element type"))?;
        let expected = array
            .get(1)
            .and_then(Value::as_u64)
            .and_then(|length| usize::try_from(length).ok())
            .ok_or_else(|| encoding_error(path, "array type has an invalid length"))?;
        let values = value
            .as_array()
            .ok_or_else(|| encoding_error(path, "expected an array"))?;
        if values.len() != expected {
            return Err(encoding_error(
                path,
                format!("expected {expected} elements, got {}", values.len()),
            ));
        }
        for (index, value) in values.iter().enumerate() {
            encode_value(idl, &format!("{path}[{index}]"), inner, value, output)?;
        }
        return Ok(());
    }

    if let Some(name) = defined_name(ty) {
        return encode_defined(idl, path, name, value, output);
    }

    Err(encoding_error(path, format!("unsupported IDL type {ty}")))
}

fn encode_primitive(path: &str, ty: &str, value: &Value, output: &mut Vec<u8>) -> Result<()> {
    macro_rules! unsigned {
        ($kind:ty) => {{
            let parsed = parse_u128(value)
                .and_then(|number| <$kind>::try_from(number).ok())
                .ok_or_else(|| encoding_error(path, format!("expected {ty}")))?;
            output.extend_from_slice(&parsed.to_le_bytes());
            Ok(())
        }};
    }
    macro_rules! signed {
        ($kind:ty) => {{
            let parsed = parse_i128(value)
                .and_then(|number| <$kind>::try_from(number).ok())
                .ok_or_else(|| encoding_error(path, format!("expected {ty}")))?;
            output.extend_from_slice(&parsed.to_le_bytes());
            Ok(())
        }};
    }

    match ty {
        "bool" => {
            output.push(
                value
                    .as_bool()
                    .ok_or_else(|| encoding_error(path, "expected bool"))? as u8,
            );
            Ok(())
        }
        "u8" => unsigned!(u8),
        "u16" => unsigned!(u16),
        "u32" => unsigned!(u32),
        "u64" => unsigned!(u64),
        "u128" => unsigned!(u128),
        "i8" => signed!(i8),
        "i16" => signed!(i16),
        "i32" => signed!(i32),
        "i64" => signed!(i64),
        "i128" => signed!(i128),
        "f32" => {
            let number = value
                .as_f64()
                .ok_or_else(|| encoding_error(path, "expected f32"))?
                as f32;
            output.extend_from_slice(&number.to_le_bytes());
            Ok(())
        }
        "f64" => {
            let number = value
                .as_f64()
                .ok_or_else(|| encoding_error(path, "expected f64"))?;
            output.extend_from_slice(&number.to_le_bytes());
            Ok(())
        }
        "pubkey" | "publicKey" => {
            let address = value
                .as_str()
                .and_then(|address| Address::from_str(address).ok())
                .ok_or_else(|| encoding_error(path, "expected a base58 public key"))?;
            output.extend_from_slice(address.as_ref());
            Ok(())
        }
        "string" => {
            let string = value
                .as_str()
                .ok_or_else(|| encoding_error(path, "expected string"))?;
            write_len(path, string.len(), output)?;
            output.extend_from_slice(string.as_bytes());
            Ok(())
        }
        "bytes" => {
            let bytes = json_bytes(path, value)?;
            write_len(path, bytes.len(), output)?;
            output.extend_from_slice(&bytes);
            Ok(())
        }
        _ => Err(encoding_error(path, format!("unsupported primitive {ty}"))),
    }
}

fn encode_defined(
    idl: &Value,
    path: &str,
    name: &str,
    value: &Value,
    output: &mut Vec<u8>,
) -> Result<()> {
    let definition = idl
        .get("types")
        .and_then(Value::as_array)
        .and_then(|types| {
            types
                .iter()
                .find(|definition| definition.get("name").and_then(Value::as_str) == Some(name))
        })
        .ok_or_else(|| encoding_error(path, format!("defined type {name} was not found")))?;
    let body = definition
        .get("type")
        .ok_or_else(|| encoding_error(path, format!("defined type {name} has no body")))?;

    match body.get("kind").and_then(Value::as_str) {
        Some("struct") => encode_struct(idl, path, body, value, output),
        Some("enum") => encode_enum(idl, path, body, value, output),
        Some(kind) => Err(encoding_error(
            path,
            format!("unsupported defined type kind {kind}"),
        )),
        None => Err(encoding_error(
            path,
            format!("defined type {name} has no kind"),
        )),
    }
}

fn encode_struct(
    idl: &Value,
    path: &str,
    definition: &Value,
    value: &Value,
    output: &mut Vec<u8>,
) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| encoding_error(path, "expected an object"))?;
    for field in definition
        .get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = field
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| encoding_error(path, "struct field has no name"))?;
        let ty = field
            .get("type")
            .ok_or_else(|| encoding_error(path, format!("struct field {name} has no type")))?;
        let value = object
            .get(name)
            .ok_or_else(|| encoding_error(&format!("{path}.{name}"), "missing field"))?;
        encode_value(idl, &format!("{path}.{name}"), ty, value, output)?;
    }
    Ok(())
}

fn encode_enum(
    idl: &Value,
    path: &str,
    definition: &Value,
    value: &Value,
    output: &mut Vec<u8>,
) -> Result<()> {
    let (variant_name, payload) = if let Some(name) = value.as_str() {
        (name, None)
    } else {
        let object = value
            .as_object()
            .ok_or_else(|| encoding_error(path, "expected a variant name or object"))?;
        if object.len() != 1 {
            return Err(encoding_error(
                path,
                "enum objects must contain exactly one variant",
            ));
        }
        let (name, payload) = object.iter().next().expect("length checked");
        (name.as_str(), Some(payload))
    };
    let variants = definition
        .get("variants")
        .and_then(Value::as_array)
        .ok_or_else(|| encoding_error(path, "enum has no variants"))?;
    let (index, variant) = variants
        .iter()
        .enumerate()
        .find(|(_, variant)| variant.get("name").and_then(Value::as_str) == Some(variant_name))
        .ok_or_else(|| encoding_error(path, format!("unknown enum variant {variant_name}")))?;
    output.push(
        u8::try_from(index).map_err(|_| encoding_error(path, "enum has more than 256 variants"))?,
    );

    let Some(fields) = variant.get("fields").and_then(Value::as_array) else {
        return Ok(());
    };
    if fields.is_empty() {
        return Ok(());
    }
    let payload = payload.ok_or_else(|| encoding_error(path, "enum variant needs a payload"))?;
    if fields.iter().all(|field| field.get("name").is_some()) {
        let object = payload
            .as_object()
            .ok_or_else(|| encoding_error(path, "expected an object variant payload"))?;
        for field in fields {
            let name = field
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| encoding_error(path, "named enum variant field has no name"))?;
            let ty = field
                .get("type")
                .ok_or_else(|| encoding_error(path, format!("enum field {name} has no type")))?;
            let value = object
                .get(name)
                .ok_or_else(|| encoding_error(&format!("{path}.{name}"), "missing field"))?;
            encode_value(idl, &format!("{path}.{name}"), ty, value, output)?;
        }
    } else {
        let values = payload
            .as_array()
            .ok_or_else(|| encoding_error(path, "expected an array variant payload"))?;
        if values.len() != fields.len() {
            return Err(encoding_error(
                path,
                format!(
                    "expected {} tuple fields, got {}",
                    fields.len(),
                    values.len()
                ),
            ));
        }
        for (index, (field, value)) in fields.iter().zip(values).enumerate() {
            let ty = field.get("type").unwrap_or(field);
            encode_value(idl, &format!("{path}[{index}]"), ty, value, output)?;
        }
    }
    Ok(())
}

fn defined_name(ty: &Value) -> Option<&str> {
    let defined = ty.get("defined")?;
    defined
        .as_str()
        .or_else(|| defined.get("name").and_then(Value::as_str))
}

fn parse_u128(value: &Value) -> Option<u128> {
    value
        .as_u64()
        .map(u128::from)
        .or_else(|| value.as_str()?.parse().ok())
}

fn parse_i128(value: &Value) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
        .or_else(|| value.as_str()?.parse().ok())
}

fn write_len(path: &str, length: usize, output: &mut Vec<u8>) -> Result<()> {
    let length = u32::try_from(length)
        .map_err(|_| encoding_error(path, "length exceeds Borsh u32 limit"))?;
    output.extend_from_slice(&length.to_le_bytes());
    Ok(())
}

fn json_bytes(path: &str, value: &Value) -> Result<Vec<u8>> {
    if let Some(values) = value.as_array() {
        return values
            .iter()
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .ok_or_else(|| encoding_error(path, "byte values must be between 0 and 255"))
            })
            .collect();
    }
    if let Some(hex) = value.as_str().and_then(|value| value.strip_prefix("0x")) {
        // Guard ASCII before byte-slicing: a multi-byte char would otherwise
        // pass the even-length check and panic on a non-char-boundary slice.
        if hex.len() % 2 != 0 || !hex.is_ascii() {
            return Err(encoding_error(path, "hex bytes must have an even length"));
        }
        let bytes = hex.as_bytes();
        return (0..bytes.len())
            .step_by(2)
            .map(|index| {
                let pair =
                    std::str::from_utf8(&bytes[index..index + 2]).expect("ascii checked above");
                u8::from_str_radix(pair, 16).map_err(|_| encoding_error(path, "invalid hex bytes"))
            })
            .collect();
    }
    Err(encoding_error(
        path,
        "expected a byte array or 0x-prefixed hex string",
    ))
}

fn encoding_error(path: &str, reason: impl Into<String>) -> Error {
    Error::ArgumentEncoding {
        argument: path.to_string(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn encodes_nested_anchor_types_as_borsh() {
        let idl = json!({
            "types": [
                {
                    "name": "Config",
                    "type": {
                        "kind": "struct",
                        "fields": [
                            { "name": "enabled", "type": "bool" },
                            { "name": "limits", "type": { "array": ["u16", 2] } },
                            { "name": "mode", "type": { "defined": { "name": "Mode" } } }
                        ]
                    }
                },
                {
                    "name": "Mode",
                    "type": {
                        "kind": "enum",
                        "variants": [
                            { "name": "Off" },
                            { "name": "Fixed", "fields": ["u64"] },
                            {
                                "name": "Range",
                                "fields": [
                                    { "name": "min", "type": "i32" },
                                    { "name": "max", "type": "i32" }
                                ]
                            }
                        ]
                    }
                }
            ]
        });
        let instruction = json!({
            "name": "configure",
            "args": [
                { "name": "config", "type": { "defined": "Config" } },
                { "name": "memo", "type": { "option": "string" } },
                { "name": "payload", "type": "bytes" },
                { "name": "amounts", "type": { "vec": "u8" } }
            ]
        });
        let args = json!({
            "config": {
                "enabled": true,
                "limits": [10, 20],
                "mode": { "Range": { "min": -5, "max": 9 } }
            },
            "memo": "ok",
            "payload": "0xdeadbeef",
            "amounts": [7, 8, 9]
        });
        let mut output = Vec::new();
        encode_arguments(&idl, &instruction, args.as_object().unwrap(), &mut output).unwrap();

        let mut expected = vec![1];
        expected.extend_from_slice(&10_u16.to_le_bytes());
        expected.extend_from_slice(&20_u16.to_le_bytes());
        expected.push(2);
        expected.extend_from_slice(&(-5_i32).to_le_bytes());
        expected.extend_from_slice(&9_i32.to_le_bytes());
        expected.push(1);
        expected.extend_from_slice(&2_u32.to_le_bytes());
        expected.extend_from_slice(b"ok");
        expected.extend_from_slice(&4_u32.to_le_bytes());
        expected.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        expected.extend_from_slice(&3_u32.to_le_bytes());
        expected.extend_from_slice(&[7, 8, 9]);
        assert_eq!(output, expected);
    }

    #[test]
    fn accepts_large_integers_as_decimal_strings() {
        let instruction = json!({
            "name": "large",
            "args": [
                { "name": "unsigned", "type": "u128" },
                { "name": "signed", "type": "i128" }
            ]
        });
        let args = json!({
            "unsigned": "340282366920938463463374607431768211455",
            "signed": "-170141183460469231731687303715884105728"
        });
        let mut output = Vec::new();
        encode_arguments(
            &json!({}),
            &instruction,
            args.as_object().unwrap(),
            &mut output,
        )
        .unwrap();

        assert_eq!(&output[..16], &u128::MAX.to_le_bytes());
        assert_eq!(&output[16..], &i128::MIN.to_le_bytes());
    }

    #[test]
    fn reports_argument_path_for_bad_nested_value() {
        let idl = json!({
            "types": [{
                "name": "Config",
                "type": {
                    "kind": "struct",
                    "fields": [{ "name": "count", "type": "u8" }]
                }
            }]
        });
        let instruction = json!({
            "name": "configure",
            "args": [{ "name": "config", "type": { "defined": "Config" } }]
        });
        let args = json!({ "config": { "count": 300 } });
        let error = encode_arguments(
            &idl,
            &instruction,
            args.as_object().unwrap(),
            &mut Vec::new(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            Error::ArgumentEncoding { argument, .. } if argument == "config.count"
        ));
    }
}
