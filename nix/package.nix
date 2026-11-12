{
  lib,
  cargo-about,
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

  # CI's Rust jobs run the test suite, so builds of the package skip it.
  doCheck = false;

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
