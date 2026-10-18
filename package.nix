{
  lib,
  cacert,
  git,
  rustPlatform,
  stdenv,
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

  # Tests run `git init`, which needs a writable HOME, and build TLS clients that load the
  # platform trust store, which the sandbox leaves empty without cacert.
  nativeCheckInputs = [
    cacert
    git
  ];
  preCheck = ''
    export HOME=$TMPDIR
  '';

  # The darwin sandbox blocks trustd, so every Security framework trust evaluation fails with
  # errSecInternalComponent. These tests run only under `cargo test` on macOS.
  checkFlags = lib.optionals stdenv.hostPlatform.isDarwin [
    "--skip=util::tls::tests::apply_extra_ca_certs_connects_to_a_long_lived_leaf_from_the_bundle"
    "--skip=util::tls::tests::exempt_anchors_"
    "--skip=util::tls::tests::extra_ca_verifier_new_"
    "--skip=util::tls::tests::verify_server_cert_"
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
