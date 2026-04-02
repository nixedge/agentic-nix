// Shared library used by both the mcp-server and ingest binaries.
//
// Exposes the ingest sub-modules so the MCP server can call indexing
// functions directly without shelling out to the ingest binary.

pub mod ingest {
    pub mod code;
    pub mod crates;
    pub mod docs;
    pub mod embed;
    pub mod hackage;
    pub mod repo_index;
    pub mod symbols;
}
