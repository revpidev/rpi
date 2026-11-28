use sha2::Digest;

pub const MAX_TOOL_NAME_LENGTH: usize = 64;

pub fn create_mcp_tool_name(server: &str, tool: &str, is_taken: impl Fn(&str) -> bool) -> String {
    let name: String = format!("mcp__{server}__{tool}")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if name.len() <= MAX_TOOL_NAME_LENGTH && !is_taken(&name) {
        return name;
    }
    let digest = sha2::Sha256::digest(format!("{server}\0{tool}").as_bytes());
    let hash = format!("{digest:x}");
    let hash = &hash[..8];
    let keep = (MAX_TOOL_NAME_LENGTH - hash.len() - 1).min(name.len());
    format!("{}_{hash}", &name[..keep])
}

pub fn mcp_namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_sanitize_hash_and_collide_deterministically() {
        assert_eq!(
            create_mcp_tool_name("dev-radius", "read", |_| false),
            "mcp__dev_radius__read"
        );
        let taken = create_mcp_tool_name("s", "read", |_| true);
        assert!(taken.starts_with("mcp__s__read_"));
        assert_eq!(taken.len(), "mcp__s__read".len() + 9);
        let long = create_mcp_tool_name("s", &"x".repeat(100), |_| false);
        assert_eq!(long.len(), MAX_TOOL_NAME_LENGTH);
        assert_eq!(mcp_namespace("dev-radius"), "mcp__dev_radius");
    }
}
