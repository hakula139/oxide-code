# ==================================================================================================
# oxide-code Development Flake
# ==================================================================================================

{
  description = "oxide-code packages and development environment";

  nixConfig = {
    extra-substituters = [ "https://hakula.cachix.org" ];
    extra-trusted-public-keys = [
      "hakula.cachix.org-1:7zwB3fhMfReHdOjh6DmnaLXgqbPDBcojvN9F+osZw0k="
    ];
  };

  # ------------------------------------------------------------------------------------------------
  # Inputs
  # ------------------------------------------------------------------------------------------------
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

  # ------------------------------------------------------------------------------------------------
  # Outputs
  # ------------------------------------------------------------------------------------------------
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

        # ------------------------------------------------------------------------------------------
        # Pre-commit Hooks
        # ------------------------------------------------------------------------------------------
        hooks = {
          end-of-file-fixer.enable = true;
          trim-trailing-whitespace.enable = true;
          check-added-large-files.enable = true;
          check-yaml.enable = true;
          taplo.enable = true;

          nixfmt.enable = true;
          deadnix.enable = true;
          statix.enable = true;

          rustfmt = {
            enable = true;
            packageOverrides = {
              cargo = rustToolchain;
              rustfmt = rustToolchain;
            };
          };
          module-docs = {
            enable = true;
            name = "Rust module docs";
            entry = "${pkgs.bash}/bin/bash scripts/check-module-docs.sh";
            files = "^crates/.*\\.rs$";
            extraPackages = [ pkgs.gnugrep ];
          };
        };

        # These hooks run the tools pinned in node_modules, which the Nix sandbox lacks. The flake
        # check leaves them to CI's Node Check job.
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
                  echo "node_modules is missing. Run pnpm install, then commit again." >&2
                  exit 1
                fi
                pnpm exec ${cmd} "$@"
              '';
            }
          );

        nodeHooks = {
          prettier-write = {
            enable = true;
            name = "prettier";
            entry = nodeHook "prettier-write" "prettier --write --ignore-unknown";
            files = "\\.(json|jsonc|md|ya?ml)$";
          };

          cspell = {
            enable = true;
            entry = nodeHook "cspell" "cspell --no-must-find-files --no-progress";
            types = [ "text" ];
          };
          markdownlint = {
            enable = true;
            name = "markdownlint-cli2";
            entry = nodeHook "markdownlint" "markdownlint-cli2 --fix";
            files = "\\.md$";
          };
        };

        preCommit = git-hooks-nix.lib.${system}.run {
          src = ./.;
          hooks = hooks // nodeHooks;
        };

        preCommitCheck = git-hooks-nix.lib.${system}.run {
          inherit hooks;
          src = ./.;
        };

        git = pkgs.lib.getExe pkgs.git;
      in
      {
        # ------------------------------------------------------------------------------------------
        # Dev Shell
        # ------------------------------------------------------------------------------------------
        devShells.default = pkgs.mkShell {
          name = "oxide-code-dev";

          packages =
            preCommit.enabledPackages
            ++ [ rustToolchain ]
            ++ (with pkgs; [
              cargo-deny
              cargo-llvm-cov
              cargo-mutants
              git-cliff
              nodejs_24
              pnpm
            ]);

          # git-hooks.nix stores a hooks path relative to the main checkout, which linked worktrees
          # can't resolve (cachix/git-hooks.nix#688), so commits there would skip the hooks.
          shellHook = ''
            ${preCommit.shellHook}
            if common=$(${git} rev-parse --path-format=absolute --git-common-dir 2>/dev/null); then
              ${git} config --local core.hooksPath "$common/hooks"
            fi
          '';

          env.RUST_BACKTRACE = "1";
        };

        # ------------------------------------------------------------------------------------------
        # Packages
        # ------------------------------------------------------------------------------------------
        packages = {
          default = oxide-code;
          inherit oxide-code;
        };

        # ------------------------------------------------------------------------------------------
        # Checks
        # ------------------------------------------------------------------------------------------
        checks.pre-commit = preCommitCheck;

        # ------------------------------------------------------------------------------------------
        # Formatter
        # ------------------------------------------------------------------------------------------
        formatter = pkgs.nixfmt-tree;
      }
    );
}
