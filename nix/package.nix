{
  lib,
  cargo-about,
  git,
  rustPlatform,
  stdenv,
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
      ../LICENSE
      ../about.toml
      ../crates
      # The config tests check that the guide's reference lists every key and variable.
      ../docs/guide/configuration.md
      ../licenses
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ cargo-about ];

  preBuild = ''
    cargo about generate \
      --locked \
      --manifest-path crates/ox/Cargo.toml \
      --target ${stdenv.hostPlatform.rust.rustcTarget} \
      --name about licenses \
      -o THIRD-PARTY-NOTICES \
      --offline
  '';

  # The flake's `tests` check runs the suite, so builds of the package skip it.
  doCheck = false;

  # Tests build much faster without the release profile's LTO and single codegen unit.
  checkType = "debug";

  # Tests bind loopback ports, which the darwin sandbox denies by default.
  __darwinAllowLocalNetworking = true;

  nativeCheckInputs = [ git ];

  postInstall = ''
    install -Dm644 -t "$out/share/doc/oxide-code" LICENSE THIRD-PARTY-NOTICES
  '';

  doInstallCheck = true;
  nativeInstallCheckInputs = [ versionCheckHook ];

  meta = {
    description = "Terminal-based AI coding assistant written in Rust";
    homepage = "https://github.com/hakula139/oxide-code";
    license = lib.licenses.mit;
    mainProgram = "ox";
  };
}
