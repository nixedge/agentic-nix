{inputs, ...}: {
  perSystem = {
    pkgs,
    ...
  }: {
    process-compose.dev = {
      imports = [inputs.services-flake.processComposeModules.default];

      # ── PostgreSQL 17 with ParadeDB extensions ────────────────────────────
      services.postgres."pg" = {
        enable = true;
        # pg_search (BM25) + pgvector are in postgresql17Packages, not postgresql_17.pkgs,
        # so we call withPackages directly rather than using the extensions option.
        package = pkgs.postgresql_17.withPackages (ps: [
          pkgs.postgresql17Packages.pg_search
          pkgs.postgresql17Packages.pgvector
        ]);

        # pg_search must be preloaded
        settings.shared_preload_libraries = "pg_search";

        initialDatabases = [
          {
            name = "codebase";
            schemas = [../scripts/schema.sql];
          }
        ];
      };

      # ── Ollama (embeddings + optional LLM) ───────────────────────────────
      services.ollama."llm" = {
        enable = true;
        acceleration = "cuda";
        # jina-code-embeddings-1.5b: code-specific, Qwen2.5-Coder base, 32k ctx.
        # Ollama returns 1536-dim vectors.
        models = ["hf.co/jinaai/jina-code-embeddings-1.5b-GGUF:Q8_0"];
      };
    };
  };
}
