# Update the MCP module to use the custom error handling function
pub mod server {
    use super::*;

    pub fn run() -> Result<(), io::Error> {
        server::run()
    }
}