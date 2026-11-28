//! The environment block of a launched process (design §11.6).
//!
//! `CreateProcessW` takes a UTF-16 environment block: `NAME=value` entries,
//! each NUL-terminated, the block terminated by a second NUL. The trampoline
//! launches the new instance with the trampoline process's environment plus
//! `RPI_UPDATE_TRAMPOLINE_PID`, without touching its own environment (no
//! `set_var` while other threads may read it). Building the block is pure
//! string handling, so it is compiled and tested on every platform.

use std::ffi::{OsStr, OsString};

/// Build the UTF-16 environment block for a child: `vars` (the parent's
/// environment) with `extra` applied on top — an entry in `extra` replaces
/// every same-named variable (ASCII case-insensitive, as Windows compares
/// environment names). Entries whose name is empty, or that contain a NUL
/// or an `=` in the name, are skipped; a `extra` entry is never dropped
/// for a `=` in its value.
pub fn environment_block(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    extra: &[(String, String)],
) -> Vec<u16> {
    let mut entries: Vec<(String, OsString)> = Vec::new();
    for (name, value) in vars {
        let Some(name) = name.to_str() else {
            // A non-UTF-8 name cannot be overridden or matched; keep it as is.
            entries.push((String::new(), format_env_pair(&name, &value)));
            continue;
        };
        if name.is_empty() || name.contains(['\0', '=']) {
            continue;
        }
        let overridden = extra
            .iter()
            .any(|(extra_name, _)| extra_name.eq_ignore_ascii_case(name));
        if overridden {
            continue;
        }
        entries.push((
            name.to_owned(),
            format_env_pair(&OsString::from(name), &value),
        ));
    }
    for (name, value) in extra {
        if name.is_empty() || name.contains(['\0', '=']) || value.contains('\0') {
            continue;
        }
        entries.push((
            name.clone(),
            format_env_pair(&OsString::from(name), &OsString::from(value)),
        ));
    }
    let mut block: Vec<u16> = Vec::new();
    for (_, pair) in entries {
        block.extend(wide(&pair));
        block.push(0);
    }
    // The block terminator after the last entry's own NUL — two NULs even
    // for an empty environment.
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    block
}

/// UTF-16 units of an OS string (the exact encoding on Windows; the
/// lossy conversion elsewhere only serves the tests).
fn wide(text: &OsStr) -> Vec<u16> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        text.encode_wide().collect()
    }
    #[cfg(not(windows))]
    {
        text.to_string_lossy().encode_utf16().collect()
    }
}

fn format_env_pair(name: &OsString, value: &OsString) -> OsString {
    let mut pair = name.clone();
    pair.push("=");
    pair.push(value);
    pair
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(block: &[u16]) -> Vec<String> {
        assert!(
            block.ends_with(&[0, 0]),
            "the block ends with the last entry's NUL plus the terminator: {block:?}"
        );
        let text: String = String::from_utf16(&block[..block.len() - 2]).unwrap();
        if text.is_empty() {
            Vec::new()
        } else {
            text.split('\0').map(str::to_owned).collect()
        }
    }

    #[test]
    fn the_block_carries_the_parent_environment_plus_the_extra_entry() {
        let vars = vec![
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (OsString::from("HOME"), OsString::from("/home/me")),
        ];
        let block = environment_block(
            vars,
            &[("RPI_UPDATE_TRAMPOLINE_PID".to_owned(), "4242".to_owned())],
        );
        assert_eq!(
            decode(&block),
            vec![
                "PATH=/usr/bin",
                "HOME=/home/me",
                "RPI_UPDATE_TRAMPOLINE_PID=4242"
            ]
        );
        assert!(block.ends_with(&[0, 0]));
    }

    #[test]
    fn an_extra_entry_replaces_a_same_named_variable_case_insensitively() {
        let vars = vec![
            (
                OsString::from("RPI_UPDATE_TRAMPOLINE_PID"),
                OsString::from("1"),
            ),
            (OsString::from("path"), OsString::from("/old")),
        ];
        let block = environment_block(
            vars,
            &[
                ("rpi_update_trampoline_pid".to_owned(), "77".to_owned()),
                ("PATH".to_owned(), "/new".to_owned()),
            ],
        );
        let entries = decode(&block);
        assert_eq!(entries, vec!["rpi_update_trampoline_pid=77", "PATH=/new"]);
    }

    #[test]
    fn malformed_entries_are_skipped_and_an_empty_block_is_terminated() {
        let vars = vec![
            (OsString::from(""), OsString::from("x")),
            (OsString::from("A=B"), OsString::from("x")),
            (OsString::from("KEEP"), OsString::from("=v=")),
        ];
        let block = environment_block(
            vars,
            &[
                ("".to_owned(), "x".to_owned()),
                ("BAD\0".to_owned(), "x".to_owned()),
            ],
        );
        assert_eq!(decode(&block), vec!["KEEP==v="]);
        assert_eq!(environment_block(vec![], &[]), vec![0, 0]);
    }
}
