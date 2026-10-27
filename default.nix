# Reproducible build: nix-build -I nixpkgs=flake:nixpkgs   (or: nix-build --arg pkgs 'import <nixpkgs> {}')
# The Nix source has no .git, so pass the revision for `workmap --version`:
#   nix-build -I nixpkgs=flake:nixpkgs --argstr rev "$(git rev-parse --short=12 HEAD)"
{ pkgs ? import <nixpkgs> { }, rev ? "unknown" }:
pkgs.rustPlatform.buildRustPackage {
  pname = "workmap";
  version = "0.5.0";
  src = pkgs.lib.cleanSourceWith {
    src = ./.;
    filter = path: type: !(pkgs.lib.hasInfix "/target" path) && baseNameOf path != ".git";
  };
  cargoLock.lockFile = ./Cargo.lock;
  # Read by build.rs when the source tree is not a Git checkout.
  WORKMAP_BUILD_REV = rev;
  # Tests create git repos/worktrees in a temp dir.
  nativeCheckInputs = [ pkgs.git ];
  meta.mainProgram = "workmap";
}
