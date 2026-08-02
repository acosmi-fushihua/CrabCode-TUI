//! Renderer-local MCP name formatting.
//!
//! This preserves the fixed title projection used by tool blocks and modal
//! chrome. It owns no MCP discovery, status, authentication, control, or wire
//! protocol behavior.

/// Delimiter used by the existing backend to qualify tool names as
/// `"<server>__<tool>"`.
pub const MCP_TOOL_NAME_DELIMITER: &str = "__";

/// Pretty-format one MCP server- or tool-name segment for display.
///
/// Splits on `_`, title-cases each word, and leaves camelCase and hyphens
/// otherwise intact.
pub fn mcp_titleize_segment(name: &str) -> String {
    name.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_segment_title_projection_is_stable() {
        assert_eq!(mcp_titleize_segment("list_issues"), "List Issues");
        assert_eq!(mcp_titleize_segment("server_name"), "Server Name");
        assert_eq!(mcp_titleize_segment("getMyTaskList"), "GetMyTaskList");
        assert_eq!(mcp_titleize_segment("notion-fetch"), "Notion-fetch");
        assert_eq!(mcp_titleize_segment(""), "");
        assert_eq!(MCP_TOOL_NAME_DELIMITER, "__");
    }
}
