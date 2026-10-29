//! SQLite schema-prefix validation (port of `src/sqlite/schema.ts` @
//! `4a080252f`).

use crate::errors::{StorageError, StorageResult};

/// Validate a prefix used for every durable table and index in a database.
///
/// The prefix must be one SQLite identifier fragment and must not use SQLite's
/// reserved `sqlite_` namespace. An empty string is invalid (upstream
/// `validateSqliteSchemaPrefix`).
pub fn validate_sqlite_schema_prefix(prefix: &str) -> StorageResult<()> {
    let mut chars = prefix.chars();
    let valid = match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {
            chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
        }
        _ => false,
    };
    if !valid {
        return Err(StorageError::message(format!(
            "Invalid SQLite schema prefix {}",
            serde_json::Value::String(prefix.to_string())
        )));
    }
    if prefix.to_ascii_lowercase().starts_with("sqlite_") {
        return Err(StorageError::message(format!(
            "SQLite schema prefix {} is reserved",
            serde_json::Value::String(prefix.to_string())
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_sqlite_schema_prefix;

    #[test]
    fn accepts_identifier_fragments_only() {
        for prefix in ["a", "A", "_", "rpi_", "_x9"] {
            validate_sqlite_schema_prefix(prefix).expect("valid prefix");
        }
    }

    #[test]
    fn rejects_empty_and_non_identifier_prefixes() {
        for prefix in ["", "1a", "bad-prefix", "a b", "a.b", "a\"b"] {
            let error = validate_sqlite_schema_prefix(prefix).expect_err("invalid prefix");
            assert!(
                error
                    .to_string()
                    .starts_with("Invalid SQLite schema prefix"),
                "{error}"
            );
        }
    }

    #[test]
    fn rejects_reserved_sqlite_namespace_case_insensitively() {
        for prefix in ["sqlite_", "SQLITE_", "Sqlite_x"] {
            let error = validate_sqlite_schema_prefix(prefix).expect_err("reserved prefix");
            assert!(error.to_string().ends_with("is reserved"), "{error}");
        }
    }
}
