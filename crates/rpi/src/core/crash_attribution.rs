//! Extension attribution for crash stacks.
//!
//! Port of `packages/coding-agent/src/core/crash-log.ts:46-113`
//! (`findExtensionStackMatches`) @ `63787ee6b` (#9830-adjacent, R3.5.5):
//! given a stack trace and the loaded extensions' path metadata, report the
//! extensions whose source files appear in the stack.
//!
//! **[N/A-struct] wiring ruling (V16-04 §8-2/§7.3)**: rpi has no crash
//! report surface to hang the hint on — `/bug` is [DEFER], `RpiError` carries
//! no stack, `handle_fatal_runtime_error` receives a preformatted message,
//! and the `rpi-tui` panic hook has no loaded-extension metadata seam (it
//! runs before/outside the session). The pure matcher and its tests land per
//! the task's explicit fallback; no interactive/stderr hint is wired.

use std::collections::HashSet;

use rpi_ext_host::types::{ExtSourceInfo, SourceOrigin};

/// `Pick<Extension, "path" | "resolvedPath" | "sourceInfo">`
/// (crash-log.ts:46).
pub struct ExtensionStackSource<'a> {
    /// `extension.path` (synthetic paths included, e.g. `<inline:name>`).
    pub path: &'a str,
    /// `extension.resolvedPath` — the on-disk entry file.
    pub resolved_path: &'a str,
    /// `extension.sourceInfo` provenance.
    pub source_info: &'a ExtSourceInfo,
}

/// `normalizeStackPath` (crash-log.ts:48-50): backslashes to slashes, strip
/// trailing slashes.
fn normalize_stack_path(value: &str) -> String {
    value.replace('\\', "/").trim_end_matches('/').to_string()
}

/// JS `/^[a-z]:\//iu` — a Windows drive prefix after normalization.
fn is_windows_drive_path(path: &str) -> bool {
    let mut chars = path.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.next() == Some(':')
        && chars.next() == Some('/')
}

/// `decodeURI` (ECMA-262): percent-decode everything except the reserved set
/// `; / ? : @ & = + $ , #`; malformed escapes or invalid UTF-8 return `None`
/// so the caller keeps the raw line (upstream `try/catch`).
fn decode_uri(input: &str) -> Option<String> {
    const RESERVED: &[u8] = b";/?:@&=+$,#";
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // Collect consecutive `%XX` escapes until they form a complete
        // UTF-8 character (at most four bytes).
        let start = i;
        let mut collected: Vec<u8> = Vec::new();
        let mut cursor = i;
        loop {
            if cursor + 2 >= bytes.len() || bytes[cursor] != b'%' {
                return None;
            }
            let hi = (bytes[cursor + 1] as char).to_digit(16)?;
            let lo = (bytes[cursor + 2] as char).to_digit(16)?;
            collected.push((hi * 16 + lo) as u8);
            cursor += 3;
            match std::str::from_utf8(&collected) {
                Ok(_) => break,
                // Incomplete but still-valid prefix → keep collecting.
                Err(error) if error.error_len().is_none() && collected.len() < 4 => continue,
                Err(_) => return None,
            }
        }
        if collected.len() == 1 && RESERVED.contains(&collected[0]) {
            // Reserved characters keep their escaped text (decodeURI).
            out.extend_from_slice(&bytes[start..cursor]);
        } else {
            out.extend_from_slice(&collected);
        }
        i = cursor;
    }
    String::from_utf8(out).ok()
}

/// JS `/^\s+at\s/` — an indented stack-frame line.
fn is_stack_frame_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.len() < line.len()
        && trimmed.starts_with("at")
        && trimmed
            .get(2..)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

/// `stackContainsPath` (crash-log.ts:52-68).
fn stack_contains_path(stack: &str, target_path: &str, include_descendants: bool) -> bool {
    let target = normalize_stack_path(target_path);
    if target.is_empty() || target.starts_with('<') {
        return false;
    }
    let case_insensitive = is_windows_drive_path(&target);
    let haystack;
    let needle;
    let (haystack, needle) = if case_insensitive {
        haystack = stack.to_lowercase();
        needle = target.to_lowercase();
        (haystack.as_str(), needle.as_str())
    } else {
        (stack, target.as_str())
    };
    if include_descendants {
        return haystack.contains(&format!("{needle}/"));
    }

    let mut search_from = 0;
    while let Some(index) = haystack[search_from..].find(needle) {
        let absolute = search_from + index;
        match haystack[absolute + needle.len()..].chars().next() {
            None | Some(':') | Some(')') => return true,
            Some(c) if c.is_whitespace() => return true,
            _ => {}
        }
        search_from = absolute + needle.len();
    }
    false
}

/// JS `/\.[cm]?[jt]s$/`.
fn has_js_ts_extension(value: &str) -> bool {
    [".js", ".ts", ".mjs", ".mts", ".cjs", ".cts"]
        .iter()
        .any(|extension| value.ends_with(extension))
}

/// JS `/\/index\.[cm]?[jt]s$/`.
fn is_directory_entry(value: &str) -> bool {
    [".js", ".ts", ".mjs", ".mts", ".cjs", ".cts"]
        .iter()
        .any(|extension| value.ends_with(&format!("/index{extension}")))
}

/// `findExtensionStackMatches` (crash-log.ts:70-113): loaded extensions with
/// source files in a stack trace.
///
/// `stack` is the raw `Error.stack`-shaped text (first line = message, then
/// indented `at` frames); frame paths may be percent-encoded.
pub fn find_extension_stack_matches(
    stack: Option<&str>,
    extensions: &[ExtensionStackSource<'_>],
) -> Vec<String> {
    let Some(stack) = stack else {
        return Vec::new();
    };
    if stack.is_empty() {
        return Vec::new();
    }
    let normalized_stack = stack
        .split('\n')
        .skip(1)
        .filter(|line| is_stack_frame_line(line))
        .map(|line| decode_uri(line).unwrap_or_else(|| line.to_owned()))
        .collect::<Vec<_>>()
        .join("\n")
        .replace('\\', "/");

    let mut matches = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for extension in extensions {
        let resolved_path = normalize_stack_path(extension.resolved_path);
        let source = &extension.source_info.source;
        let is_package = extension.source_info.origin == SourceOrigin::Package;
        let single_file_package = is_package
            && !(source.starts_with("npm:")
                || source.starts_with("git:")
                || source.starts_with("http://")
                || source.starts_with("https://")
                || source.starts_with("ssh://"))
            && has_js_ts_extension(source);
        let package_root = if is_package && !single_file_package {
            extension.source_info.base_dir.as_deref()
        } else {
            None
        };
        let slash_index = resolved_path.rfind('/');
        let matched = if let Some(root) = package_root {
            stack_contains_path(&normalized_stack, root, true)
        } else if is_directory_entry(&resolved_path) {
            match slash_index {
                Some(index) => {
                    stack_contains_path(&normalized_stack, &resolved_path[..index], true)
                }
                None => stack_contains_path(&normalized_stack, &resolved_path, false),
            }
        } else {
            stack_contains_path(&normalized_stack, &resolved_path, false)
        };
        if !matched {
            continue;
        }

        let label = if is_package && !source.is_empty() {
            source.clone()
        } else {
            extension.path.to_owned()
        };
        if seen.insert(label.clone()) {
            matches.push(label);
        }
    }
    matches
}

#[cfg(test)]
mod tests {
    //! Port of `packages/coding-agent/test/crash-log.test.ts` @ 63787ee6b.

    use super::*;
    use rpi_ext_host::types::SourceScope;

    fn source_info(
        path: &str,
        source: &str,
        origin: SourceOrigin,
        base_dir: Option<&str>,
    ) -> ExtSourceInfo {
        ExtSourceInfo {
            path: path.to_owned(),
            source: source.to_owned(),
            scope: SourceScope::User,
            origin,
            base_dir: base_dir.map(str::to_owned),
        }
    }

    fn package_extension<'a>(
        path: &'a str,
        resolved_path: &'a str,
        info: &'a ExtSourceInfo,
    ) -> ExtensionStackSource<'a> {
        ExtensionStackSource {
            path,
            resolved_path,
            source_info: info,
        }
    }

    #[test]
    fn matches_stack_frames_beneath_loaded_package_roots() {
        let memory_info = source_info(
            "/home/fedora/.pi/agent/npm/node_modules/pi-observational-memory/extensions/index.ts",
            "npm:pi-observational-memory",
            SourceOrigin::Package,
            Some("/home/fedora/.pi/agent/npm/node_modules/pi-observational-memory"),
        );
        let unrelated_info = source_info(
            "/home/fedora/.pi/agent/npm/node_modules/unrelated/extensions/index.ts",
            "npm:unrelated",
            SourceOrigin::Package,
            Some("/home/fedora/.pi/agent/npm/node_modules/unrelated"),
        );
        let memory = package_extension(
            "/home/fedora/.pi/agent/npm/node_modules/pi-observational-memory/extensions/index.ts",
            "/home/fedora/.pi/agent/npm/node_modules/pi-observational-memory/extensions/index.ts",
            &memory_info,
        );
        let unrelated = package_extension(
            "/home/fedora/.pi/agent/npm/node_modules/unrelated/extensions/index.ts",
            "/home/fedora/.pi/agent/npm/node_modules/unrelated/extensions/index.ts",
            &unrelated_info,
        );
        let stack = "TypeError: Cannot read properties of undefined (reading 'runtime')\n\
                     \x20   at streamSimple (file:///home/fedora/.local/lib/node_modules/@earendil-works/pi-coding-agent/dist/bundle/chunks/chunk-CMRUVXTE.js:1093:16944)\n\
                     \x20   at /home/fedora/.pi/agent/npm/node_modules/pi-observational-memory/src/agents/worker-stream.ts:43:45";
        assert_eq!(
            find_extension_stack_matches(Some(stack), &[memory, unrelated]),
            vec!["npm:pi-observational-memory"]
        );
    }

    #[test]
    fn normalizes_windows_paths_and_deduplicates_package_extensions() {
        let root = "C:\\Users\\reporter\\.pi\\agent\\npm\\node_modules\\@scope\\memory";
        let normalized_root = root.replace('\\', "/");
        let first_path = format!("{normalized_root}/extensions/first.ts");
        let second_path = format!("{normalized_root}/extensions/second.ts");
        let first_info = source_info(
            &first_path,
            "npm:@scope/memory",
            SourceOrigin::Package,
            Some(root),
        );
        let second_info = source_info(
            &second_path,
            "npm:@scope/memory",
            SourceOrigin::Package,
            Some(root),
        );
        let first = package_extension(&first_path, &first_path, &first_info);
        let second = package_extension(&second_path, &second_path, &second_info);
        let stack = "Error: broken\n\
                     \x20   at run (c:\\users\\reporter\\.pi\\agent\\npm\\node_modules\\@scope\\memory\\src\\worker.ts:4:2)";
        assert_eq!(
            find_extension_stack_matches(Some(stack), &[first, second]),
            vec!["npm:@scope/memory"]
        );
    }

    #[test]
    fn does_not_attribute_sibling_single_file_packages() {
        let make_info = |name: &str| {
            source_info(
                &format!("/plugins/{name}.ts"),
                &format!("/plugins/{name}.ts"),
                SourceOrigin::Package,
                Some("/plugins"),
            )
        };
        let a_info = make_info("a");
        let b_info = make_info("b");
        let a = package_extension("/plugins/a.ts", "/plugins/a.ts", &a_info);
        let b = package_extension("/plugins/b.ts", "/plugins/b.ts", &b_info);
        let stack = "Error: broken\n    at run (file:///plugins/b.ts:4:2)";
        assert_eq!(
            find_extension_stack_matches(Some(stack), &[a, b]),
            vec!["/plugins/b.ts"]
        );
    }

    #[test]
    fn ignores_extension_paths_in_the_error_message() {
        let info = source_info(
            "/tmp/node_modules/memory/extensions/index.ts",
            "npm:memory",
            SourceOrigin::Package,
            Some("/tmp/node_modules/memory"),
        );
        let extension = package_extension(
            "/tmp/node_modules/memory/extensions/index.ts",
            "/tmp/node_modules/memory/extensions/index.ts",
            &info,
        );
        let stack = format!(
            "Error: Failed to read {}\n    at run (file:///opt/pi/dist/core.js:4:2)",
            extension.resolved_path
        );
        assert!(find_extension_stack_matches(Some(&stack), &[extension]).is_empty());
    }

    #[test]
    fn decodes_frame_paths_independently_from_malformed_error_text() {
        let info = source_info(
            "/Users/reporter/.pi/agent/extensions/local memory/index.ts",
            "local",
            SourceOrigin::TopLevel,
            Some("/Users/reporter/.pi/agent/extensions"),
        );
        let extension = package_extension(
            "/Users/reporter/.pi/agent/extensions/local memory/index.ts",
            "/Users/reporter/.pi/agent/extensions/local memory/index.ts",
            &info,
        );
        // The `100%` in the message is an invalid escape for decodeURI, but
        // only frame lines are decoded.
        let stack = "Error: progress 100%\n        at run (file:///Users/reporter/.pi/agent/extensions/local%20memory/worker.ts:4:2)";
        assert_eq!(
            find_extension_stack_matches(Some(stack), &[extension]),
            vec!["/Users/reporter/.pi/agent/extensions/local memory/index.ts"]
        );
    }

    #[test]
    fn no_matches_for_none_or_empty_stack() {
        let info = source_info("/tmp/plain.ts", "local", SourceOrigin::TopLevel, None);
        let make = || package_extension("/tmp/plain.ts", "/tmp/plain.ts", &info);
        assert!(find_extension_stack_matches(None, &[make()]).is_empty());
        assert!(find_extension_stack_matches(Some(""), &[make()]).is_empty());
    }
}
