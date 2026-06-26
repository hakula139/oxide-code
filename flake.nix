# oxide-code flake — source-builds `ox` for any Rust-supported platform.
#
#   nix run github:hakula139/oxide-code           # one-shot
#   nix profile install github:hakula139/oxide-code
#   nix develop                                   # dev shell + pre-commit hooks
#   nix flake check                               # run pre-commit hooks

{
  description = "oxide-code — terminal-based AI coding assistant";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    git-hooks-nix = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      git-hooks-nix,
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
        # llvm-tools-preview backs `cargo llvm-cov`; rust-analyzer / rust-src aid editors.
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "llvm-tools-preview"
            "rust-analyzer"
            "rust-src"
          ];
        };

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

        # ----------------------------------------------------------------------
        # Node Hook Wrapper
        # ----------------------------------------------------------------------
        # `pnpm exec` needs node + pnpm on PATH and the project's `node_modules`
        # materialised. The Nix sandbox lacks the latter, so `nix flake check`
        # skips these hooks; the equivalent checks run in CI via direct `pnpm`
        # scripts (the `node-check` job).
        nodeHook =
          name: cmd:
          let
            wrapper = pkgs.writeShellApplication {
              inherit name;
              runtimeInputs = [
                pkgs.nodejs_24
                pkgs.pnpm
              ];
              text = ''
                if [ ! -d node_modules ]; then
                  exit 0
                fi
                pnpm exec ${cmd} "$@"
              '';
            };
          in
          "${wrapper}/bin/${name}";

        # ----------------------------------------------------------------------
        # Pre-commit Hooks
        # ----------------------------------------------------------------------
        # Mirrors the compile-free CI checks. Clippy, tests, and coverage stay
        # in CI, where their build cost does not gate every commit.
        preCommitCheck = git-hooks-nix.lib.${system}.run {
          src = ./.;
          hooks = {
            nixfmt.enable = true;

            # Clippy stays in CI; the bare hook would recompile on every commit.
            rustfmt = {
              enable = true;
              packageOverrides = {
                cargo = rustToolchain;
                rustfmt = rustToolchain;
              };
            };

            markdownlint = {
              enable = true;
              name = "markdownlint-cli2";
              entry = nodeHook "markdownlint" "markdownlint-cli2 --fix";
              files = "\\.md$";
              pass_filenames = true;
            };

            cspell = {
              enable = true;
              entry = nodeHook "cspell" "cspell --no-must-find-files --no-progress";
              types = [ "text" ];
              pass_filenames = true;
            };
          };
        };
      in
      {
        # ----------------------------------------------------------------------
        # Dev Shell (`nix develop`) — provisions the hook toolchain and installs
        # the git hook via the generated `shellHook`.
        # ----------------------------------------------------------------------
        devShells.default = pkgs.mkShell {
          name = "oxide-code-dev";

          packages =
            preCommitCheck.enabledPackages
            ++ [ rustToolchain ]
            ++ (with pkgs; [
              cargo-llvm-cov
              git-cliff
              nodejs_24
              pnpm
            ]);

          shellHook = preCommitCheck.shellHook;

          env.RUST_BACKTRACE = "1";
        };

        packages = {
          default = oxide-code;
          inherit oxide-code;
        };

        # ----------------------------------------------------------------------
        # Checks (`nix flake check`) — runs the same hooks CI gates on.
        # ----------------------------------------------------------------------
        checks = {
          pre-commit = preCommitCheck;
        };

        formatter = pkgs.nixfmt;
      }
    );
}
