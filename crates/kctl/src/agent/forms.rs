//! Forms the tools put in front of the operator, and the checks on what comes back.

use serde_json::{json, Map, Value};

pub type FormContent = Map<String, Value>;

pub struct Choice {
    pub value: String,
    pub label: String,
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

pub fn choices_of(property: &Value) -> Option<Vec<Choice>> {
    if let Some(items) = property["oneOf"].as_array() {
        return Some(
            items
                .iter()
                .map(|item| Choice {
                    value: text_of(&item["const"]),
                    label: item["title"]
                        .as_str()
                        .map_or_else(|| text_of(&item["const"]), str::to_string),
                })
                .collect(),
        );
    }
    let values = property["enum"].as_array()?;
    let names = property["enumNames"].as_array();
    Some(
        values
            .iter()
            .enumerate()
            .map(|(i, value)| Choice {
                value: text_of(value),
                label: names
                    .and_then(|n| n.get(i))
                    .and_then(Value::as_str)
                    .map_or_else(|| text_of(value), str::to_string),
            })
            .collect(),
    )
}

/// An approval is a form whose only field is a yes/no `confirm`.
pub fn is_approval(schema: &Value) -> bool {
    let Some(properties) = schema["properties"].as_object() else {
        return false;
    };
    if properties.len() != 1 {
        return false;
    }
    let Some(confirm) = properties.get("confirm") else {
        return false;
    };
    confirm["enum"].as_array().is_some_and(|choices| {
        choices.iter().any(|c| c == "yes") && choices.iter().any(|c| c == "no")
    })
}

fn number_value(number: f64) -> Value {
    if number.fract() == 0.0 && number.abs() < 9.0e15 {
        json!(number as i64)
    } else {
        json!(number)
    }
}

pub fn validate_form(schema: &Value, input: &Value) -> Result<FormContent, String> {
    let Some(given) = input.as_object() else {
        return Err("form content must be an object".to_string());
    };
    let empty = Map::new();
    let properties = schema["properties"].as_object().unwrap_or(&empty);
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut content = FormContent::new();

    for name in given.keys() {
        if !properties.contains_key(name) {
            return Err(format!("unknown field {name}"));
        }
    }
    for (name, property) in properties {
        let label = property["title"].as_str().unwrap_or(name);
        let value = given.get(name);
        let blank = match value {
            None | Some(Value::Null) => true,
            Some(Value::String(text)) => text.trim().is_empty(),
            _ => false,
        };
        if blank {
            if required.contains(&name.as_str()) {
                return Err(format!("{label} is required"));
            }
            continue;
        }
        let value = value.expect("not blank");
        match property["type"].as_str() {
            Some(kind @ ("number" | "integer")) => {
                let number = match value {
                    Value::Number(n) => n.as_f64(),
                    Value::String(text) => text.trim().parse::<f64>().ok(),
                    _ => None,
                }
                .filter(|n| n.is_finite())
                .ok_or_else(|| format!("{label} must be a number"))?;
                if kind == "integer" && number.fract() != 0.0 {
                    return Err(format!("{label} must be a whole number"));
                }
                if let Some(min) = property["minimum"].as_f64() {
                    if number < min {
                        return Err(format!(
                            "{label} must be at least {}",
                            text_of(&number_value(min))
                        ));
                    }
                }
                if let Some(max) = property["maximum"].as_f64() {
                    if number > max {
                        return Err(format!(
                            "{label} must be at most {}",
                            text_of(&number_value(max))
                        ));
                    }
                }
                content.insert(name.clone(), number_value(number));
            }
            Some("boolean") => {
                if !value.is_boolean() {
                    return Err(format!("{label} must be true or false"));
                }
                content.insert(name.clone(), value.clone());
            }
            _ => {
                let Some(text) = value.as_str() else {
                    return Err(format!("{label} must be text"));
                };
                if let Some(choices) = choices_of(property) {
                    if !choices.iter().any(|c| c.value == text) {
                        let wanted: Vec<&str> = choices.iter().map(|c| c.value.as_str()).collect();
                        return Err(format!("{label} must be one of: {}", wanted.join(", ")));
                    }
                }
                content.insert(name.clone(), value.clone());
            }
        }
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_validation_enforces_required_enum_and_number_rules() {
        let schema = json!({
            "type": "object",
            "properties": { "name": { "type": "string", "title": "Name" }, "size": { "type": "number", "minimum": 1 }, "net": { "type": "string", "enum": ["a", "b"] } },
            "required": ["name"],
        });
        assert!(validate_form(&schema, &json!({})).is_err());
        assert!(validate_form(&schema, &json!({ "name": "x", "net": "c" })).is_err());
        assert!(validate_form(&schema, &json!({ "name": "x", "size": 0 })).is_err());
        assert!(validate_form(&schema, &json!({ "name": "x", "extra": 1 })).is_err());
        assert!(validate_form(&schema, &json!([])).is_err());
        let ok = validate_form(&schema, &json!({ "name": "x", "size": "4", "net": "b" })).unwrap();
        assert_eq!(
            Value::Object(ok),
            json!({ "name": "x", "size": 4, "net": "b" })
        );
        assert!(is_approval(
            &json!({ "properties": { "confirm": { "enum": ["yes", "no"] } } })
        ));
        assert!(!is_approval(
            &json!({ "properties": { "confirm": { "enum": ["yes", "no"] }, "other": {} } })
        ));
    }

    #[test]
    fn integers_reject_fractions_and_blank_optionals_are_dropped() {
        let schema = json!({ "properties": { "keep": { "type": "integer", "title": "Keep" }, "note": { "type": "string" } } });
        assert_eq!(
            validate_form(&schema, &json!({ "keep": 1.5 })).unwrap_err(),
            "Keep must be a whole number"
        );
        let ok = validate_form(&schema, &json!({ "keep": 3, "note": "  " })).unwrap();
        assert_eq!(Value::Object(ok), json!({ "keep": 3 }));
    }

    #[test]
    fn labelled_choices_validate_against_their_values() {
        let schema = json!({ "properties": {
            "net": { "type": "string", "enum": ["lan", "dmz"], "enumNames": ["lan (10.0.0.1)", "dmz"] },
            "node": { "type": "string", "oneOf": [{ "const": "n1", "title": "n1 (host-a)" }] },
        } });
        assert!(validate_form(&schema, &json!({ "net": "lan" })).is_ok());
        assert!(validate_form(&schema, &json!({ "net": "lan (10.0.0.1)" })).is_err());
        assert!(validate_form(&schema, &json!({ "node": "n1" })).is_ok());
        assert!(validate_form(&schema, &json!({ "node": "n2" })).is_err());
    }
}
