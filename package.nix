{
  cacert,
  git,
  lib,
  rustPlatform,
}:

let
  fs = lib.fileset;
in
rustPlatform.buildRustPackage {
  pname = "oxide-code";
  inherit ((lib.importTOML ./Cargo.toml).workspace.package) version;

  src = fs.toSource {
    root = ./.;
    fileset = fs.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./crates
    ];
  };
  cargoLock.lockFile = ./Cargo.lock;

  # The wiremock tests bind 127.0.0.1, which the darwin sandbox denies by default.
  __darwinAllowLocalNetworking = true;

  # Tests run `git init`, which needs a writable HOME, and build TLS clients that load the
  # platform trust store, which the sandbox leaves empty without cacert.
  nativeCheckInputs = [
    cacert
    git
  ];
  preCheck = ''
    export HOME=$TMPDIR
  '';

  meta = {
    description = "Terminal-based AI coding assistant written in Rust";
    homepage = "https://github.com/hakula139/oxide-code";
    license = lib.licenses.mit;
    mainProgram = "ox";
  };
}
