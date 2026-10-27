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
      ../about.hbs
      ../about.toml
      ../crates
      # The config tests check that the guide's reference lists every key and variable.
      ../docs/guide/configuration.md
      ../licenses
      ../scripts/generate-notices.sh
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ cargo-about ];

  preBuild = ''
    bash scripts/generate-notices.sh \
      ${stdenv.hostPlatform.rust.rustcTarget} THIRD-PARTY-NOTICES --offline
  '';

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

  postInstallCheck = ''
    doc="$out/share/doc/oxide-code"
    cmp LICENSE "$doc/LICENSE"
    cmp THIRD-PARTY-NOTICES "$doc/THIRD-PARTY-NOTICES"
    grep -q '^- syntect ' "$doc/THIRD-PARTY-NOTICES"
    tail -c "$(wc -c < licenses/syntect-assets.txt)" "$doc/THIRD-PARTY-NOTICES" |
      cmp licenses/syntect-assets.txt -
    if grep -Eq '^- (insta|temp-env|wiremock) ' "$doc/THIRD-PARTY-NOTICES"; then
      echo "Third-party notices include test dependencies" >&2
      exit 1
    fi
  '';

  meta = {
    description = "Terminal-based AI coding assistant written in Rust";
    homepage = "https://github.com/hakula139/oxide-code";
    license = lib.licenses.mit;
    mainProgram = "ox";
  };
}
