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

    home-manager = {
      url = "github:nix-community/home-manager/release-26.05";
      inputs.nixpkgs.follows = "nixpkgs";
    };

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
      self,
      nixpkgs,
      flake-utils,
      home-manager,
      rust-overlay,
      git-hooks-nix,
      ...
    }:
    let
      # git-hooks.nix has dropped x86_64-darwin, which Nixpkgs supports only through 26.05.
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];

      # Builds ox against pkgs with the toolchain rust-toolchain.toml pins.
      mkOxideCode =
        pkgs:
        let
          toolchain =
            (rust-overlay.lib.mkRustBin { } pkgs.buildPackages).fromRustupToolchainFile
              ./rust-toolchain.toml;
        in
        pkgs.callPackage ./nix/package.nix {
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
        };
    in
    # ----------------------------------------------------------------------------------------------
    # Overlay and Home Manager Module
    # ----------------------------------------------------------------------------------------------
    {
      overlays.default = final: _: { oxide-code = mkOxideCode final; };

      homeModules.default = {
        _class = "homeManager";
        _file = "${self}/flake.nix#homeModules.default";
        imports = [ (import ./nix/home-manager.nix { inherit mkOxideCode; }) ];
      };
    }
    // flake-utils.lib.eachSystem systems (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        rustBin = rust-overlay.lib.mkRustBin { } pkgs;
        rustToolchain = rustBin.fromRustupToolchainFile ./rust-toolchain.toml;

        rustDevToolchain = rustToolchain.override (prev: {
          extensions = prev.extensions ++ [
            "rust-analyzer"
            "rust-src"
          ];
        });

        oxide-code = mkOxideCode pkgs;

        # ------------------------------------------------------------------------------------------
        # Pre-commit Hooks
        # ------------------------------------------------------------------------------------------
        hooks = {
          end-of-file-fixer.enable = true;
          trim-trailing-whitespace.enable = true;
          check-added-large-files.enable = true;
          check-shebang-scripts-are-executable.enable = true;
          check-yaml.enable = true;
          taplo.enable = true;

          nixfmt.enable = true;
          deadnix.enable = true;
          statix.enable = true;

          shellcheck.enable = true;

          rustfmt = {
            enable = true;
            packageOverrides = {
              cargo = rustToolchain;
              rustfmt = rustToolchain;
            };
          };
        };

        # These hooks run the tools pinned in node_modules, which the Nix sandbox lacks. The flake
        # check leaves them to CI's Lint job.
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
                pnpm exec ${cmd} "$@"
              '';
            }
          );

        nodeHooks = {
          prettier-write = {
            enable = true;
            name = "prettier";
            entry = nodeHook "prettier-write" "prettier --write --ignore-unknown";
            types = [ "text" ];
          };

          cspell = {
            enable = true;
            package = null;
            entry = nodeHook "cspell" "cspell --no-must-find-files --no-progress";
            types = [ "text" ];
          };

          markdownlint = {
            enable = true;
            name = "markdownlint-cli2";
            package = null;
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

        flock = pkgs.lib.getExe pkgs.flock;
        git = pkgs.lib.getExe pkgs.git;
      in
      {
        # ------------------------------------------------------------------------------------------
        # Dev Shell
        # ------------------------------------------------------------------------------------------
        devShells.default = pkgs.mkShell {
          name = "oxide-code-dev";

          # The rustfmt hook puts the plain toolchain on PATH, so the dev toolchain must come first.
          packages = [
            rustDevToolchain
          ]
          ++ preCommit.enabledPackages
          ++ [ preCommit.config.package ]
          ++ (with pkgs; [
            cargo-deny
            cargo-insta
            cargo-llvm-cov
            cargo-mutants
            git-cliff
            nodejs_24
            pnpm
          ]);

          # git-hooks.nix stores a hooks path relative to the main checkout, which linked worktrees
          # can't resolve (cachix/git-hooks.nix#688), so unset it to fall back to the shared hooks
          # directory. The lock keeps a concurrent load from setting it while `pre-commit install`
          # runs, since pre-commit refuses to install with the path set.
          shellHook = ''
            if common=$(${git} rev-parse --path-format=absolute --git-common-dir 2>/dev/null); then
              (
                ${flock} 9
                ${preCommit.config.installationScript}
                ${git} config --local --unset-all core.hooksPath || true
              ) 9>"$common/git-hooks.lock"
            fi
          '';
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
        checks = {
          pre-commit = preCommitCheck;

          home-manager = import ./nix/tests/home-manager.nix {
            inherit home-manager pkgs;
            homeModule = self.homeModules.default;
          };
        };

        # ------------------------------------------------------------------------------------------
        # Formatter
        # ------------------------------------------------------------------------------------------
        formatter = pkgs.nixfmt-tree;
      }
    );
}
