# Update test cases to handle the new custom error handling mechanism
use super::*;

#[test]
fn test_invalid_utf8() {
    // Test that the MCP server returns a parse error when encountering invalid UTF-8 bytes
    let mut line = String::new();
    line.push(0xFF); // Invalid UTF-8 byte
    let result = run();
    assert!(result.is_err());
    assert!(result.unwrap_err().is::<ParseError>());
}

#[test]
fn test_valid_utf8() {
    // Test that the MCP server processes valid UTF-8 lines correctly
    let mut line = String::new();
    line.push_str("Hello, world!");
    let result = run();
    assert!(result.is_ok());
}