# oxide-code flake — source-builds `ox` for any Rust-supported platform.
#
#   nix run github:hakula139/oxide-code           # one-shot
#   nix profile install github:hakula139/oxide-code

{
  description = "oxide-code — terminal-based AI coding assistant";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        # Track the workspace's MSRV via rust-overlay; nixpkgs' stable rustc may lag.
        rustToolchain = pkgs.rust-bin.stable.latest.default;

        cargoToml = fromTOML (builtins.readFile ./Cargo.toml);

        # Whitelist cargo's actual inputs so docs / CI / editor metadata stay out of
        # the build context (cache stays warm; new top-level files don't sneak in).
        included = [
          "Cargo.lock"
          "Cargo.toml"
          "crates"
          "rust-toolchain.toml"
        ];

        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter =
            path: _type:
            let
              rel = pkgs.lib.removePrefix (toString ./. + "/") (toString path);
              firstSegment = pkgs.lib.head (pkgs.lib.splitString "/" rel);
            in
            pkgs.lib.elem firstSegment included;
        };

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        oxide-code = rustPlatform.buildRustPackage {
          pname = "oxide-code";
          inherit (cargoToml.workspace.package) version;
          inherit src;
          cargoLock.lockFile = ./Cargo.lock;

          # Several test modules shell out to `git`; HOME must be writable for `git init`.
          nativeCheckInputs = [ pkgs.git ];
          preCheck = ''
            export HOME=$TMPDIR
          '';

          meta = {
            description = "Terminal-based AI coding assistant written in Rust";
            homepage = "https://github.com/hakula139/oxide-code";
            license = pkgs.lib.licenses.mit;
            mainProgram = "ox";
          };
        };
      in
      {
        packages = {
          default = oxide-code;
          inherit oxide-code;
        };

        formatter = pkgs.nixfmt;
      }
    );
}
