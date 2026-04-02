{inputs, ...}: {
  perSystem = {system, ...}: {
    # Allow unfree packages (required for CUDA toolkit used by ollama-cuda)
    _module.args.pkgs = import inputs.nixpkgs {
      inherit system;
      config.allowUnfree = true;
    };
  };
}
