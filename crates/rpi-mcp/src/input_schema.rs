use serde_json::{Map, Value};

pub fn normalize_mcp_input_schema(schema: &Map<String, Value>) -> Map<String, Value> {
    let mut normalized = schema.clone();
    normalized
        .entry("type")
        .or_insert_with(|| Value::String("object".to_owned()));
    normalized
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_only_missing_object_fields() {
        let normalized = normalize_mcp_input_schema(&Map::new());
        assert_eq!(normalized["type"], "object");
        assert_eq!(normalized["properties"], serde_json::json!({}));
        let custom = normalize_mcp_input_schema(&Map::from_iter([
            ("type".to_owned(), Value::String("string".to_owned())),
            ("properties".to_owned(), Value::Null),
        ]));
        assert_eq!(custom["type"], "string");
        assert_eq!(custom["properties"], Value::Null);
    }
}
