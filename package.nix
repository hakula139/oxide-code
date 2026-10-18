{
  lib,
  cacert,
  git,
  rustPlatform,
  versionCheckHook,
}:

rustPlatform.buildRustPackage {
  inherit ((lib.importTOML ./Cargo.toml).workspace.package) version;
  pname = "oxide-code";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./crates
    ];
  };
  cargoLock.lockFile = ./Cargo.lock;

  # Tests bind loopback ports, which the darwin sandbox denies by default.
  __darwinAllowLocalNetworking = true;

  # TLS clients load the platform trust store, which the sandbox leaves empty without cacert.
  nativeCheckInputs = [
    cacert
    git
  ];

  doInstallCheck = true;
  nativeInstallCheckInputs = [ versionCheckHook ];

  meta = {
    description = "Terminal-based AI coding assistant written in Rust";
    homepage = "https://github.com/hakula139/oxide-code";
    license = lib.licenses.mit;
    mainProgram = "ox";
  };
}
