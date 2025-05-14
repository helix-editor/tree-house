{
  lib,
  rustPlatform,

  gitRev ? null,
}:
let
  fs = lib.fileset;

  files = fs.difference (fs.gitTracked ./.) (
    fs.unions [
      ./.github
      ./.envrc
      ./flake.lock
      (fs.fileFilter (file: lib.strings.hasInfix ".git" file.name) ./.)
      (fs.fileFilter (file: file.hasExt "md") ./.)
      (fs.fileFilter (file: file.hasExt "nix") ./.)
    ]
  );
in
rustPlatform.buildRustPackage {
  strictDeps = true;
  pname = with builtins; (fromTOML (readFile ./cli/Cargo.toml)).package.name;
  version = with builtins; (fromTOML (readFile ./cli/Cargo.toml)).package.version;

  src = fs.toSource {
    root = ./.;
    fileset = files;
  };

  cargoLock = {
    lockFile = ./Cargo.lock;
    allowBuiltinFetchGit = true;
  };

  cargoBuildFlags = [ "-p skidder-cli" ];

  doCheck = false;
  env.GIT_HASH = gitRev;

  meta.mainProgram = "skidder-cli";
}
