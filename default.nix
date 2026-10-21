# Reproducible build: nix-build -I nixpkgs=flake:nixpkgs   (or: nix-build --arg pkgs 'import <nixpkgs> {}')
{ pkgs ? import <nixpkgs> { } }:
pkgs.rustPlatform.buildRustPackage {
  pname = "workmap";
  version = "0.2.0";
  src = pkgs.lib.cleanSourceWith {
    src = ./.;
    filter = path: type: !(pkgs.lib.hasInfix "/target" path);
  };
  cargoLock.lockFile = ./Cargo.lock;
  # Tests create git repos/worktrees in a temp dir.
  nativeCheckInputs = [ pkgs.git ];
  meta.mainProgram = "workmap";
}
