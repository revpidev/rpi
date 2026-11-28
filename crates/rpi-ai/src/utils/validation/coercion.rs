use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::{CompiledSchema, get_schema_types, matches_json_type};

pub(super) fn coerce(compiled: &CompiledSchema, value: Value) -> Value {
    let mut coercer = Coercer {
        compiled,
        entered: Vec::new(),
        cuts: 0,
        memo: HashMap::new(),
    };
    let value = coercer.drop_optional_nulls(value, "#", "#");
    let value = coercer.convert(value, "#", "#");
    coercer.coerce_types(value, "#", "#")
}

struct Coercer<'a> {
    compiled: &'a CompiledSchema,
    entered: Vec<(String, String)>,
    cuts: usize,
    memo: HashMap<(String, String), Value>,
}

impl Coercer<'_> {
    fn schema(&self, path: &str) -> Option<&Value> {
        let pointer = path.strip_prefix('#').unwrap_or(path);
        self.compiled.schema.pointer(pointer)
    }

    fn passes(&self, schema_path: &str, value: &Value) -> Option<bool> {
        self.compiled
            .validators
            .get(schema_path)
            .map(|validator| validator.is_valid(value))
    }

    fn convert(&mut self, value: Value, schema_path: &str, value_path: &str) -> Value {
        let Some(reference) = self
            .schema(schema_path)
            .and_then(|schema| schema.get("$ref"))
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return self.convert_node(value, schema_path, value_path);
        };
        let Some(target) = self.resolve_reference(&reference) else {
            return value;
        };
        self.reenter(value, &target, value_path, true)
    }

    fn reenter(
        &mut self,
        value: Value,
        schema_path: &str,
        value_path: &str,
        memoize: bool,
    ) -> Value {
        let key = (schema_path.to_owned(), value_path.to_owned());
        if self.entered.contains(&key) {
            self.cuts += 1;
            return value;
        }
        if memoize && let Some(cached) = self.memo.get(&key) {
            return cached.clone();
        }
        let cuts = self.cuts;
        self.entered.push(key.clone());
        let result = self.convert(value, schema_path, value_path);
        self.entered.pop();
        if memoize && self.cuts == cuts {
            self.memo.insert(key, result.clone());
        }
        result
    }

    fn resolve_reference(&self, reference: &str) -> Option<String> {
        if reference == "#" {
            return Some("#".to_owned());
        }
        if let Some(pointer) = reference.strip_prefix("#/") {
            let mut path = String::new();
            for segment in pointer.split('/') {
                let decoded = decode_uri_component(segment)?;
                let key = decoded.replace("~1", "/").replace("~0", "~");
                path.push('/');
                path.push_str(&key.replace('~', "~0").replace('/', "~1"));
            }
            return (self.compiled.schema.pointer(&path).is_some()).then(|| format!("#{path}"));
        }
        find_named_schema(&self.compiled.schema, reference, "#")
    }

    fn convert_node(&mut self, value: Value, schema_path: &str, value_path: &str) -> Value {
        let Some(schema) = self.schema(schema_path) else {
            return value;
        };
        if let Some(literal) = schema.get("const") {
            return convert_literal(literal, &value).unwrap_or(value);
        }
        if let Some(members) = schema.get("enum").and_then(Value::as_array) {
            if members.iter().any(|member| strict_eq(member, &value)) {
                return value;
            }
            for member in members {
                if let Some(candidate) = convert_literal(member, &value)
                    && self.passes(schema_path, &candidate) == Some(true)
                {
                    return candidate;
                }
            }
            return value;
        }
        for keyword in ["anyOf", "oneOf"] {
            if schema.get(keyword).and_then(Value::as_array).is_some() {
                return self.convert_union(value, schema_path, value_path, keyword);
            }
        }
        if let Some(members) = schema.get("allOf").and_then(Value::as_array) {
            let mut next = value;
            for index in 0..members.len() {
                let path = child_path(&child_path(schema_path, "allOf"), &index.to_string());
                next = self.convert(next, &path, value_path);
            }
            return self.convert_members(next, schema_path, value_path);
        }
        let has_explicit_type = schema.get("type").is_some();
        match schema.get("type").and_then(Value::as_str) {
            Some("string") => try_string(&value).unwrap_or(value),
            Some("number") => try_number(&value, false).unwrap_or(value),
            Some("integer") => try_number(&value, true).unwrap_or(value),
            Some("boolean") => try_boolean(&value).unwrap_or(value),
            Some("null") => try_null(&value).unwrap_or(value),
            Some("array") => self.convert_array(value, schema_path, value_path),
            Some("object") => self.convert_object(value, schema_path, value_path),
            Some(_) => value,
            None if has_explicit_type => value,
            None if schema.get("properties").is_some()
                || schema.get("patternProperties").is_some() =>
            {
                self.convert_object(value, schema_path, value_path)
            }
            None if schema.get("items").is_some() || schema.get("prefixItems").is_some() => {
                self.convert_array(value, schema_path, value_path)
            }
            None => value,
        }
    }

    fn convert_union(
        &mut self,
        value: Value,
        schema_path: &str,
        value_path: &str,
        keyword: &str,
    ) -> Value {
        let branches_path = child_path(schema_path, keyword);
        let branch_count = self
            .schema(&branches_path)
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        for index in 0..branch_count {
            let branch = child_path(&branches_path, &index.to_string());
            if self.passes(&branch, &value) == Some(true) {
                return self.convert_members(value, schema_path, value_path);
            }
        }
        for index in 0..branch_count {
            let branch = child_path(&branches_path, &index.to_string());
            let candidate = self.convert(value.clone(), &branch, value_path);
            let candidate = self.convert_members(candidate, schema_path, value_path);
            if self.passes(schema_path, &candidate) == Some(true) {
                return candidate;
            }
        }
        self.convert_members(value, schema_path, value_path)
    }

    fn convert_members(&mut self, value: Value, schema_path: &str, value_path: &str) -> Value {
        let declares = self.schema(schema_path).is_some_and(|schema| {
            schema.get("properties").is_some()
                || schema.get("patternProperties").is_some()
                || schema.get("additionalProperties").is_some()
        });
        if declares {
            self.convert_object(value, schema_path, value_path)
        } else {
            value
        }
    }

    fn convert_array(&mut self, value: Value, schema_path: &str, value_path: &str) -> Value {
        let items_is_tuple = self
            .schema(&child_path(schema_path, "items"))
            .is_some_and(Value::is_array);
        let prefix_len = if items_is_tuple {
            self.schema(&child_path(schema_path, "items"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        } else {
            self.schema(&child_path(schema_path, "prefixItems"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        };
        let prefix_is_tuple = self
            .schema(&child_path(schema_path, "prefixItems"))
            .is_some_and(Value::is_array);
        let is_tuple = items_is_tuple || prefix_is_tuple;
        let Value::Array(mut items) = value else {
            if is_tuple {
                return value;
            }
            let item_path = child_path(schema_path, "items");
            let item = if self.schema(&item_path).is_some_and(Value::is_object) {
                self.reenter(value, &item_path, value_path, false)
            } else {
                value
            };
            return Value::Array(vec![item]);
        };
        for (index, item) in items.iter_mut().enumerate() {
            let Some(item_schema) =
                self.item_schema(schema_path, index, items_is_tuple, prefix_len)
            else {
                continue;
            };
            let next = self.convert(
                item.clone(),
                &item_schema,
                &child_path(value_path, &index.to_string()),
            );
            *item = next;
        }
        Value::Array(items)
    }

    fn item_schema(
        &self,
        schema_path: &str,
        index: usize,
        items_is_tuple: bool,
        prefix_len: usize,
    ) -> Option<String> {
        let items = child_path(schema_path, "items");
        if items_is_tuple {
            return (index < prefix_len).then(|| child_path(&items, &index.to_string()));
        }
        let prefix = child_path(schema_path, "prefixItems");
        if index < prefix_len {
            return Some(child_path(&prefix, &index.to_string()));
        }
        self.schema(&items).map(|_| items)
    }

    fn convert_object(&mut self, value: Value, schema_path: &str, value_path: &str) -> Value {
        let Value::Object(mut object) = value else {
            return value;
        };
        let patterns = self.patterns(schema_path);
        let additional = child_path(schema_path, "additionalProperties");
        let keys: Vec<String> = object.keys().cloned().collect();
        for key in keys {
            let property = child_path(&child_path(schema_path, "properties"), &key);
            let mut matched = self.schema(&property).is_some();
            let child_value_path = child_path(value_path, &key);
            let mut next = object.get(&key).cloned().unwrap_or(Value::Null);
            if matched {
                next = self.convert(next, &property, &child_value_path);
            }
            for (pattern, pattern_schema) in &patterns {
                if !pattern.is_match(&key) {
                    continue;
                }
                matched = true;
                next = self.convert(next, pattern_schema, &child_value_path);
            }
            if !matched && self.schema(&additional).is_some_and(Value::is_object) {
                next = self.convert(next, &additional, &child_value_path);
            }
            object.insert(key, next);
        }
        Value::Object(object)
    }

    fn patterns(&self, schema_path: &str) -> Vec<(regex::Regex, String)> {
        let patterns_path = child_path(schema_path, "patternProperties");
        self.schema(&patterns_path)
            .and_then(Value::as_object)
            .map(|patterns| {
                patterns
                    .iter()
                    .filter_map(|(pattern, _)| {
                        regex::Regex::new(pattern)
                            .ok()
                            .map(|regex| (regex, child_path(&patterns_path, pattern)))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn drop_optional_nulls(&mut self, value: Value, schema_path: &str, value_path: &str) -> Value {
        if self.schema(schema_path).is_none() {
            return value;
        }
        if let Value::Array(mut items) = value {
            let items_is_tuple = self
                .schema(&child_path(schema_path, "items"))
                .is_some_and(Value::is_array);
            let prefix_len = if items_is_tuple {
                self.schema(&child_path(schema_path, "items"))
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len)
            } else {
                self.schema(&child_path(schema_path, "prefixItems"))
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len)
            };
            for (index, item) in items.iter_mut().enumerate() {
                if let Some(item_schema) =
                    self.item_schema(schema_path, index, items_is_tuple, prefix_len)
                {
                    *item = self.drop_optional_nulls(
                        item.clone(),
                        &item_schema,
                        &child_path(value_path, &index.to_string()),
                    );
                }
            }
            return Value::Array(items);
        }
        let Value::Object(mut object) = value else {
            return value;
        };
        let required: HashSet<String> = self
            .schema(schema_path)
            .and_then(|schema| schema.get("required"))
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let properties_path = child_path(schema_path, "properties");
        let property_keys: Vec<String> = self
            .schema(&properties_path)
            .and_then(Value::as_object)
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default();
        for key in property_keys {
            let Some(entry) = object.get_mut(&key) else {
                continue;
            };
            let property = child_path(&properties_path, &key);
            let is_ref = self
                .schema(&property)
                .and_then(|property| property.get("$ref"))
                .is_some_and(Value::is_string);
            if entry.is_null()
                && !required.contains(key.as_str())
                && !is_ref
                && self.passes(&property, &Value::Null) == Some(false)
            {
                object.remove(&key);
            } else {
                *entry = self.drop_optional_nulls(
                    entry.clone(),
                    &property,
                    &child_path(value_path, &key),
                );
            }
        }
        let patterns = self.patterns(schema_path);
        if patterns.is_empty()
            && !self
                .schema(&child_path(schema_path, "additionalProperties"))
                .is_some_and(Value::is_object)
        {
            return Value::Object(object);
        }
        let additional = child_path(schema_path, "additionalProperties");
        let keys: Vec<String> = object.keys().cloned().collect();
        for key in keys {
            let mut matched = self.schema(&child_path(&properties_path, &key)).is_some();
            let child_value_path = child_path(value_path, &key);
            for (pattern, pattern_schema) in &patterns {
                if !pattern.is_match(&key) {
                    continue;
                }
                matched = true;
                if let Some(entry) = object.get_mut(&key) {
                    *entry =
                        self.drop_optional_nulls(entry.clone(), pattern_schema, &child_value_path);
                }
            }
            if !matched
                && self.schema(&additional).is_some_and(Value::is_object)
                && let Some(entry) = object.get_mut(&key)
            {
                *entry = self.drop_optional_nulls(entry.clone(), &additional, &child_value_path);
            }
        }
        Value::Object(object)
    }

    fn coerce_types(&mut self, value: Value, schema_path: &str, value_path: &str) -> Value {
        let Some(schema) = self.schema(schema_path) else {
            return value;
        };
        let all_of_count = schema
            .get("allOf")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let union_keywords: Vec<&str> = ["anyOf", "oneOf"]
            .into_iter()
            .filter(|keyword| schema.get(*keyword).and_then(Value::as_array).is_some())
            .collect();
        let schema_types: Vec<String> = get_schema_types(schema)
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut next = value;
        for index in 0..all_of_count {
            let path = child_path(&child_path(schema_path, "allOf"), &index.to_string());
            next = self.coerce_types(next, &path, value_path);
        }
        for keyword in union_keywords {
            next = self.coerce_union_types(next, schema_path, value_path, keyword);
        }
        let matches_union_member = schema_types.len() > 1
            && schema_types
                .iter()
                .any(|kind| matches_json_type(&next, kind));
        if !schema_types.is_empty() && !matches_union_member {
            for kind in &schema_types {
                let candidate = coerce_primitive(&next, kind);
                if candidate != next {
                    next = candidate;
                    break;
                }
            }
        }
        if schema_types.iter().any(|kind| kind == "object") {
            next = self.coerce_object_types(next, schema_path, value_path);
        }
        if schema_types.iter().any(|kind| kind == "array") {
            next = self.coerce_array_types(next, schema_path, value_path);
        }
        next
    }

    fn coerce_union_types(
        &mut self,
        value: Value,
        schema_path: &str,
        value_path: &str,
        keyword: &str,
    ) -> Value {
        let branches_path = child_path(schema_path, keyword);
        let branch_count = self
            .schema(&branches_path)
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        for index in 0..branch_count {
            let branch = child_path(&branches_path, &index.to_string());
            if self.passes(&branch, &value) == Some(true) {
                return value;
            }
        }
        for index in 0..branch_count {
            let branch = child_path(&branches_path, &index.to_string());
            let candidate = self.coerce_types(value.clone(), &branch, value_path);
            if self.passes(&branch, &candidate) == Some(true) {
                return candidate;
            }
        }
        value
    }

    fn coerce_object_types(&mut self, value: Value, schema_path: &str, value_path: &str) -> Value {
        let Value::Object(mut object) = value else {
            return value;
        };
        let properties_path = child_path(schema_path, "properties");
        let property_keys: Vec<String> = self
            .schema(&properties_path)
            .and_then(Value::as_object)
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default();
        for key in property_keys {
            if let Some(entry) = object.get_mut(&key) {
                *entry = self.coerce_types(
                    entry.clone(),
                    &child_path(&properties_path, &key),
                    &child_path(value_path, &key),
                );
            }
        }
        let patterns = self.patterns(schema_path);
        let additional = child_path(schema_path, "additionalProperties");
        let keys: Vec<String> = object.keys().cloned().collect();
        for key in keys {
            let mut matched = self.schema(&child_path(&properties_path, &key)).is_some();
            let child_value_path = child_path(value_path, &key);
            for (pattern, pattern_schema) in &patterns {
                if !pattern.is_match(&key) {
                    continue;
                }
                matched = true;
                if let Some(entry) = object.get_mut(&key) {
                    *entry = self.coerce_types(entry.clone(), pattern_schema, &child_value_path);
                }
            }
            if !matched
                && self.schema(&additional).is_some_and(Value::is_object)
                && let Some(entry) = object.get_mut(&key)
            {
                *entry = self.coerce_types(entry.clone(), &additional, &child_value_path);
            }
        }
        Value::Object(object)
    }

    fn coerce_array_types(&mut self, value: Value, schema_path: &str, value_path: &str) -> Value {
        let Value::Array(mut items) = value else {
            return value;
        };
        let items_is_tuple = self
            .schema(&child_path(schema_path, "items"))
            .is_some_and(Value::is_array);
        let prefix_len = if items_is_tuple {
            self.schema(&child_path(schema_path, "items"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        } else {
            self.schema(&child_path(schema_path, "prefixItems"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        };
        for (index, item) in items.iter_mut().enumerate() {
            if let Some(item_schema) =
                self.item_schema(schema_path, index, items_is_tuple, prefix_len)
            {
                *item = self.coerce_types(
                    item.clone(),
                    &item_schema,
                    &child_path(value_path, &index.to_string()),
                );
            }
        }
        Value::Array(items)
    }
}

fn child_path(base: &str, segment: &str) -> String {
    let escaped = segment.replace('~', "~0").replace('/', "~1");
    if base == "#" {
        format!("#/{escaped}")
    } else {
        format!("{base}/{escaped}")
    }
}

fn decode_uri_component(segment: &str) -> Option<String> {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let (Some(high), Some(low)) = (
                bytes.get(index + 1).copied().and_then(hex),
                bytes.get(index + 2).copied().and_then(hex),
            ) else {
                return None;
            };
            decoded.push(high * 16 + low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn find_named_schema(schema: &Value, reference: &str, path: &str) -> Option<String> {
    match schema {
        Value::Object(map) => {
            for key in ["$id", "$anchor"] {
                if map.get(key).and_then(Value::as_str).is_some_and(|id| {
                    id == reference
                        || id.ends_with(&format!("/{reference}"))
                        || id.ends_with(&format!("#{reference}"))
                }) {
                    return Some(path.to_owned());
                }
            }
            for (key, child) in map {
                if matches!(
                    key.as_str(),
                    "const" | "enum" | "default" | "example" | "examples"
                ) {
                    continue;
                }
                if let Some(found) = find_named_schema(child, reference, &child_path(path, key)) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items.iter().enumerate().find_map(|(index, item)| {
            find_named_schema(item, reference, &child_path(path, &index.to_string()))
        }),
        _ => None,
    }
}

fn strict_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        _ => left == right,
    }
}

fn convert_literal(literal: &Value, value: &Value) -> Option<Value> {
    if strict_eq(literal, value) {
        return Some(value.clone());
    }
    let candidate = match literal {
        Value::String(_) => try_string(value),
        Value::Number(_) => try_number(value, false),
        Value::Bool(_) => try_boolean(value),
        _ => None,
    }?;
    strict_eq(&candidate, literal).then_some(candidate)
}

fn js_number_to_string(number: &serde_json::Number) -> String {
    if let Some(value) = number.as_f64()
        && value.fract() == 0.0
        && value.is_finite()
        && value.abs() < 1e15
    {
        return format!("{value:.0}");
    }
    number.to_string()
}

fn number_value(value: f64, truncate: bool) -> Option<Value> {
    if !value.is_finite() {
        return None;
    }
    let value = if truncate { value.trunc() } else { value };
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        return Some(Value::from(value as i64));
    }
    serde_json::Number::from_f64(value).map(Value::Number)
}

fn parse_js_number(text: &str, typebox_extensions: bool) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return Some(0.0);
    }
    let lowercase = text.to_ascii_lowercase();
    if typebox_extensions {
        match lowercase.as_str() {
            "true" => return Some(1.0),
            "false" => return Some(0.0),
            _ => {}
        }
    }
    if let Some(hex) = lowercase.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16).ok().map(|value| value as f64);
    }
    if let Some(binary) = lowercase.strip_prefix("0b") {
        return u64::from_str_radix(binary, 2)
            .ok()
            .map(|value| value as f64);
    }
    if let Some(octal) = lowercase.strip_prefix("0o") {
        return u64::from_str_radix(octal, 8).ok().map(|value| value as f64);
    }
    if let Ok(number) = lowercase.parse::<f64>()
        && number.is_finite()
    {
        return Some(number);
    }
    if typebox_extensions {
        big_int_of_string(text)
    } else {
        None
    }
}

fn big_int_of_string(text: &str) -> Option<f64> {
    let (negative, unsigned) = text
        .strip_prefix('-')
        .map_or((false, text), |rest| (true, rest));
    let (unsigned, has_n_suffix) = unsigned
        .strip_suffix('n')
        .map_or((unsigned, false), |rest| (rest, true));
    let (digits, fraction) = unsigned
        .split_once('.')
        .map_or((unsigned, None), |(digits, fraction)| {
            (digits, Some(fraction))
        });
    if has_n_suffix && fraction.is_some()
        || digits.is_empty()
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
        || digits.len() > 1 && digits.starts_with('0')
        || fraction.is_some_and(|fraction| {
            fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return None;
    }
    let mut value = digits.parse::<i128>().ok()?;
    if negative {
        value = -value;
    }
    const MAX_SAFE_INTEGER: i128 = 9_007_199_254_740_991;
    (value.abs() <= MAX_SAFE_INTEGER).then_some(value as f64)
}

fn try_number(value: &Value, truncate: bool) -> Option<Value> {
    match value {
        Value::Number(number) => number_value(number.as_f64()?, truncate),
        Value::Bool(value) => number_value(if *value { 1.0 } else { 0.0 }, truncate),
        Value::Null => number_value(0.0, truncate),
        Value::String(text) => number_value(parse_js_number(text, true)?, truncate),
        _ => None,
    }
}

fn try_string(value: &Value) -> Option<Value> {
    match value {
        Value::String(_) => Some(value.clone()),
        Value::Number(number) => Some(Value::from(js_number_to_string(number))),
        Value::Bool(value) => Some(Value::from(value.to_string())),
        Value::Null => Some(Value::from("null")),
        _ => None,
    }
}

fn try_boolean(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(_) => Some(value.clone()),
        Value::Number(number) => match number.as_f64()? {
            0.0 => Some(Value::from(false)),
            1.0 => Some(Value::from(true)),
            _ => None,
        },
        Value::Null => Some(Value::from(false)),
        Value::String(text) => match text.to_ascii_lowercase().as_str() {
            "false" | "0" => Some(Value::from(false)),
            "true" | "1" => Some(Value::from(true)),
            _ => None,
        },
        _ => None,
    }
}

fn try_null(value: &Value) -> Option<Value> {
    match value {
        Value::Null => Some(Value::Null),
        Value::Bool(false) => Some(Value::Null),
        Value::Number(number) if number.as_f64() == Some(0.0) => Some(Value::Null),
        Value::String(text)
            if matches!(
                text.to_ascii_lowercase().as_str(),
                "undefined" | "null" | "" | "0"
            ) =>
        {
            Some(Value::Null)
        }
        _ => None,
    }
}

fn coerce_primitive(value: &Value, kind: &str) -> Value {
    match kind {
        "number" | "integer" => {
            if value.is_null() {
                return Value::from(0);
            }
            if let Value::String(text) = value
                && !text.trim().is_empty()
                && let Some(parsed) = parse_js_number(text, false)
                && (kind == "number" || parsed.fract() == 0.0)
            {
                return number_value(parsed, false).unwrap_or_else(|| value.clone());
            }
            if let Value::Bool(value) = value {
                return Value::from(if *value { 1 } else { 0 });
            }
            value.clone()
        }
        "boolean" => {
            if value.is_null() {
                return Value::from(false);
            }
            if let Value::String(text) = value {
                if text == "true" {
                    return Value::from(true);
                }
                if text == "false" {
                    return Value::from(false);
                }
            }
            if let Some(number) = value.as_f64() {
                if number == 1.0 {
                    return Value::from(true);
                }
                if number == 0.0 {
                    return Value::from(false);
                }
            }
            value.clone()
        }
        "string" => {
            if value.is_null() {
                return Value::from(String::new());
            }
            match value {
                Value::Number(number) => Value::from(js_number_to_string(number)),
                Value::Bool(value) => Value::from(value.to_string()),
                _ => value.clone(),
            }
        }
        "null" => match value {
            Value::String(text) if text.is_empty() => Value::Null,
            Value::Number(number) if number.as_f64() == Some(0.0) => Value::Null,
            Value::Bool(false) => Value::Null,
            _ => value.clone(),
        },
        _ => value.clone(),
    }
}
