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
  # Newer nixpkgs sets LIBSQLITE3_SYS_USE_PKG_CONFIG=1 for every Rust build,
  # overriding rusqlite's `bundled` feature: link the system SQLite there.
  # Older nixpkgs ignores these and keeps the bundled build.
  nativeBuildInputs = [ pkgs.pkg-config ];
  buildInputs = [ pkgs.sqlite ];
  # Tests create git repos/worktrees in a temp dir.
  nativeCheckInputs = [ pkgs.git ];
  meta.mainProgram = "workmap";
}
