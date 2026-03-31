{inputs, ...}: {
  perSystem = {
    config,
    pkgs,
    inputs',
    ...
  }: let
    toolchain = inputs'.fenix.packages.stable.toolchain;
  in {
    devShells.default = pkgs.mkShell {
      packages = with pkgs; [
        # Rust toolchain (stable from fenix)
        toolchain
        pkg-config
        onnxruntime
        openssl
        # PostgreSQL client (matches server version)
        postgresql_17
        # Ollama
        ollama
        # Python + uv for ingest and MCP server scripts
        uv
        # Utilities
        jq
        just
        # Formatter
        config.treefmt.build.wrapper
        # Lightweight hybrid BM25+semantic search CLI + MCP server (zero-config)
        inputs'.llm-agents.packages.ck
        # Claude Code usage analytics
        inputs'.llm-agents.packages.ccusage
      ];

      shellHook = ''
        export ORT_DYLIB_PATH="${pkgs.onnxruntime}/lib/libonnxruntime.so"
        export LD_LIBRARY_PATH="${pkgs.openssl.out}/lib:${pkgs.onnxruntime}/lib:$LD_LIBRARY_PATH"
        echo "Agentic RAG Stack"
        echo ""
        echo "Services:"
        echo "  nix run .#dev              # Start PostgreSQL (ParadeDB) + Ollama"
        echo ""
        echo "Indexing:"
        echo "  just index /path/to/repo   # Index a codebase"
        echo ""
        echo "Database:"
        echo "  psql postgres://127.0.0.1:5432/codebase"
        echo ""
        echo "MCP server:"
        echo "  just build                 # Build the Rust binary"
        echo "  just mcp                   # Run the MCP server"
        echo "  nix build .#mcp-server     # Build with Nix (requires Cargo.lock)"
        echo ""
        echo "Quick search (no services needed):"
        echo "  ck search 'query'              # ad-hoc hybrid search"
        echo "  ck --serve                     # lightweight MCP server"
        echo "  ccusage                        # Claude Code usage stats"
      '';
    };
  };
}
