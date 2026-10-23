{
  lib,
  git,
  rustPlatform,
  versionCheckHook,
}:

rustPlatform.buildRustPackage {
  inherit ((lib.importTOML ../Cargo.toml).workspace.package) version;
  pname = "oxide-code";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      # The config tests check that the guide's reference lists every key and variable.
      ../docs/guide/configuration.md
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # Tests build much faster without the release profile's LTO and single codegen unit.
  checkType = "debug";

  # Tests bind loopback ports, which the darwin sandbox denies by default.
  __darwinAllowLocalNetworking = true;

  nativeCheckInputs = [ git ];

  doInstallCheck = true;
  nativeInstallCheckInputs = [ versionCheckHook ];

  meta = {
    description = "Terminal-based AI coding assistant written in Rust";
    homepage = "https://github.com/hakula139/oxide-code";
    license = lib.licenses.mit;
    mainProgram = "ox";
  };
}
