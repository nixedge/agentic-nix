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
  in {
    packages.mcp-server = craneLib.buildPackage (commonArgs
      // {
        inherit cargoArtifacts;
        cargoExtraArgs = "--bin mcp-server";
      });

    packages.ingest = craneLib.buildPackage (commonArgs
      // {
        inherit cargoArtifacts;
        cargoExtraArgs = "--bin ingest";
      });
  };
}
