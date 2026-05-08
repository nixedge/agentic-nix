{inputs, ...}: {
  perSystem = {
    inputs',
    lib,
    pkgs,
    ...
  }: let
    toolchain = inputs'.fenix.packages.stable.toolchain;
    craneLib = (inputs.crane.mkLib pkgs).overrideToolchain toolchain;

    src = lib.fileset.toSource {
      root = ./..;
      fileset = lib.fileset.unions (
        [
          ../Cargo.toml
          ../src
        ]
        ++ lib.optional (lib.pathExists ../Cargo.lock) ../Cargo.lock
      );
    };

    commonArgs = {
      inherit src;
      strictDeps = true;
      nativeBuildInputs = [pkgs.pkg-config pkgs.autoPatchelfHook];
      buildInputs = [pkgs.onnxruntime pkgs.openssl];
      # Tell ort (ONNX Runtime Rust binding) to use the Nix-provided shared library
      ORT_DYLIB_PATH = "${pkgs.onnxruntime}/lib/libonnxruntime.so";
    };

    cargoArtifacts = craneLib.buildDepsOnly commonArgs;

    mcpServerPkg = craneLib.buildPackage (commonArgs
      // {
        inherit cargoArtifacts;
        cargoExtraArgs = "--bin mcp-server";
      });

    ingestPkg = craneLib.buildPackage (commonArgs
      // {
        inherit cargoArtifacts;
        cargoExtraArgs = "--bin ingest";
      });

    claudeJailUnwrapped = craneLib.buildPackage (commonArgs
      // {
        inherit cargoArtifacts;
        cargoExtraArgs = "--bin claude-jail";
      });

    # Merged ~/bin for the jail. buildEnv with pathsToLink=["/bin"] handles
    # multi-binary packages (coreutils, findutils) correctly; all symlinks
    # point into /nix/store which is bind-mounted read-only inside the jail.
    claudeJailBinDir = pkgs.buildEnv {
      name = "claude-jail-tools";
      pathsToLink = ["/bin"];
      paths = [
        inputs'.llm-agents.packages.claude-code
        pkgs.nix
        pkgs.git
        pkgs.curl
        pkgs.bash
        pkgs.python3
        pkgs.direnv
        pkgs.coreutils
        pkgs.findutils
        pkgs.jq
        ingestPkg
        mcpServerPkg
      ];
    };
  in {
    packages.mcp-server = mcpServerPkg;

    packages.ingest = ingestPkg;

    # Wrapper script sets env vars the Rust binary reads, then execs it.
    packages.claude-jail = pkgs.writeShellScriptBin "claude-jail" ''
      # buildEnv puts merged symlinks under bin/
      export CLAUDE_JAIL_BIN_DIR="${claudeJailBinDir}/bin"
      export CLAUDE_JAIL_BWRAP="${pkgs.bubblewrap}/bin/bwrap"
      # Nix store cacert bundle — works even if /etc/ssl is absent on the host
      export NIX_SSL_CERT_FILE="${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
      # Enable nix-command and flakes inside the jail
      export NIX_CONFIG="extra-experimental-features = nix-command flakes"
      exec ${claudeJailUnwrapped}/bin/claude-jail "$@"
    '';
  };
}
