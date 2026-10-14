# ==============================================================================
# oxide-code Development Flake
# ==============================================================================

{
  description = "oxide-code packages and development environment";

  nixConfig = {
    extra-substituters = [ "https://hakula.cachix.org" ];
    extra-trusted-public-keys = [ "hakula.cachix.org-1:7zwB3fhMfReHdOjh6DmnaLXgqbPDBcojvN9F+osZw0k=" ];
  };

  # ----------------------------------------------------------------------------
  # Inputs
  # ----------------------------------------------------------------------------
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

  # ----------------------------------------------------------------------------
  # Outputs
  # ----------------------------------------------------------------------------
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

        rustToolchain = (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
          extensions = [
            "llvm-tools-preview"
            "rust-analyzer"
            "rust-src"
          ];
        };

        oxide-code = pkgs.callPackage ./package.nix {
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };
        };

        # ----------------------------------------------------------------------
        # Node Hook Wrapper
        # ----------------------------------------------------------------------
        # Node hooks need the local dependencies, which the Nix sandbox excludes.
        # CI runs the equivalent checks directly with pnpm.
        nodeHook =
          name: cmd:
          pkgs.lib.getExe (
            pkgs.writeShellApplication {
              inherit name;
              runtimeInputs = with pkgs; [
                nodejs_24
                pnpm
              ];
              text = ''
                if [ ! -d node_modules ]; then
                  exit 0
                fi
                pnpm exec ${cmd} "$@"
              '';
            }
          );

        # ----------------------------------------------------------------------
        # Pre-commit Hooks
        # ----------------------------------------------------------------------
        preCommitCheck = git-hooks-nix.lib.${system}.run {
          src = ./.;
          hooks = {
            check-added-large-files.enable = true;
            check-yaml.enable = true;
            end-of-file-fixer.enable = true;
            # Preserve Markdown's two-trailing-space hard-break syntax.
            trim-trailing-whitespace = {
              enable = true;
              args = [ "--markdown-linebreak-ext=md" ];
            };

            nixfmt.enable = true;
            statix.enable = true;
            deadnix.enable = true;

            rustfmt = {
              enable = true;
              packageOverrides = {
                cargo = rustToolchain;
                rustfmt = rustToolchain;
              };
            };

            prettier-write = {
              enable = true;
              name = "prettier";
              entry = nodeHook "prettier-write" "prettier --write --ignore-unknown";
              files = "\\.(cjs|js|json|mjs|ya?ml)$";
            };

            dprint-write = {
              enable = true;
              name = "dprint";
              entry = nodeHook "dprint-write" "dprint fmt";
              files = "\\.md$";
            };

            taplo-write = {
              enable = true;
              name = "taplo";
              entry = nodeHook "taplo-write" "taplo format";
              files = "\\.toml$";
            };

            markdownlint = {
              enable = true;
              name = "markdownlint-cli2";
              entry = nodeHook "markdownlint" "markdownlint-cli2 --fix";
              files = "\\.md$";
            };

            cspell = {
              enable = true;
              entry = nodeHook "cspell" "cspell --no-must-find-files --no-progress";
              types = [ "text" ];
            };
          };
        };
      in
      {
        # ----------------------------------------------------------------------
        # Dev Shell
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

          inherit (preCommitCheck) shellHook;

          env.RUST_BACKTRACE = "1";
        };

        # ----------------------------------------------------------------------
        # Packages
        # ----------------------------------------------------------------------
        packages = {
          default = oxide-code;
          inherit oxide-code;
        };

        # ----------------------------------------------------------------------
        # Checks
        # ----------------------------------------------------------------------
        checks.pre-commit = preCommitCheck;

        # ----------------------------------------------------------------------
        # Formatter
        # ----------------------------------------------------------------------
        formatter = pkgs.nixfmt-tree;
      }
    );
}
