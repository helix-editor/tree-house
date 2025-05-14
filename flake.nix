{
  description = "A package manager for tree-sitter grammars";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, nixpkgs, rust-overlay }:
    let
      inherit (nixpkgs) lib;
      forEachSystem = lib.genAttrs lib.systems.flakeExposed;
    in
    {
      packages = forEachSystem (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };
          toolchain = pkgs.rust-bin.stable.latest.default;
        in {
          skidder-cli = pkgs.callPackage ./. { };
          default = self.packages.${system}.skidder-cli;
        });
    
      devShell = forEachSystem (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };
          toolchain = pkgs.rust-bin.stable.latest.default;
        in
        pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            (toolchain.override {
              extensions = [
                "rust-src"
                "clippy"
                "llvm-tools-preview"
              ];
            })
            rust-analyzer
            cargo-flamegraph
            cargo-llvm-cov
            valgrind
          ];
          RUST_BACKTRACE = "1";
        }
      );
    };
}
