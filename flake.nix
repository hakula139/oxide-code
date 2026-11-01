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

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    git-hooks-nix = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    home-manager = {
      url = "github:nix-community/home-manager/release-26.05";
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
      rust-overlay,
      git-hooks-nix,
      home-manager,
      ...
    }:
    let
      inherit (nixpkgs) lib;

      # git-hooks.nix has dropped x86_64-darwin, which Nixpkgs supports only through 26.05.
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];

      eachSystem =
        f:
        let
          bySystem = lib.genAttrs systems f;
          outputs = builtins.attrNames bySystem.${builtins.head systems};
        in
        lib.genAttrs outputs (output: builtins.mapAttrs (_: attrs: attrs.${output}) bySystem);

      # The package builds with the Rust compiler of the given package set, so consumers that follow
      # their own nixpkgs never fetch the toolchain `rust-toolchain.toml` pins.
      mkOxideCode = pkgs: pkgs.callPackage ./nix/package.nix { };

      # A key lets the module system deduplicate the module when it is imported twice.
      homeModuleKey = "${self}/flake.nix#homeModules.default";
    in
    # ----------------------------------------------------------------------------------------------
    # Overlay and Home Manager Module
    # ----------------------------------------------------------------------------------------------
    {
      overlays.default = final: _: { oxide-code = mkOxideCode final; };

      homeModules.default = {
        _class = "homeManager";
        _file = homeModuleKey;
        key = homeModuleKey;
        imports = [ (import ./nix/home-manager.nix { inherit mkOxideCode; }) ];
      };
    }
    // eachSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        nodejs = pkgs.nodejs_26;

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

        # These hooks run tools pinned in node_modules, which the Nix sandbox lacks. The flake check
        # leaves them to CI's Lint job.
        nodeHook =
          name: cmd:
          pkgs.lib.getExe (
            pkgs.writeShellApplication {
              inherit name;
              runtimeInputs = with pkgs; [
                nodejs
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
            require_serial = true;
            types = [ "text" ];
          };

          cspell = {
            enable = true;
            package = null;
            entry = nodeHook "cspell" "cspell --no-must-find-files --no-progress";
            require_serial = true;
            types = [ "text" ];
          };

          markdownlint = {
            enable = true;
            name = "markdownlint-cli2";
            package = null;
            entry = nodeHook "markdownlint" "markdownlint-cli2 --fix";
            require_serial = true;
            files = "\\.md$";
          };
        };

        preCommit = git-hooks-nix.lib.${system}.run {
          hooks = hooks // nodeHooks;
          src = ./.;
        };

        # The flake check leaves rustfmt to CI's Rust job, so it never fetches the pinned toolchain.
        preCommitCheck = git-hooks-nix.lib.${system}.run {
          hooks = removeAttrs hooks [ "rustfmt" ];
          src = ./.;
        };

        flock = pkgs.lib.getExe pkgs.flock;
        git = pkgs.lib.getExe pkgs.git;
      in
      {
        # ------------------------------------------------------------------------------------------
        # Dev Shells
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
            nodejs
            pnpm
            sqlite
          ]);

          # Use shared hooks to avoid the relative path bug in linked worktrees (git-hooks.nix#688).
          # Serialize installation, and leave sibling repositories that borrow this shell untouched.
          shellHook = ''
            if common=$(${git} rev-parse --path-format=absolute --git-common-dir 2>/dev/null) &&
              top=$(${git} rev-parse --show-toplevel 2>/dev/null) &&
              [ -f "$top/crates/ox/Cargo.toml" ]; then
              (
                ${flock} 9
                ${preCommit.config.installationScript}
                ${git} config --local --unset-all core.hooksPath || true
              ) 9>"$common/git-hooks.lock"
            fi
          '';
        };

        # CI's Lint job runs in this shell, which leaves out the pinned toolchain so that every tool
        # comes from the nixpkgs binary cache. `cargo deny` needs only `cargo metadata`, which
        # nixpkgs' cargo serves for this workspace.
        devShells.lint = pkgs.mkShell {
          name = "oxide-code-lint";

          packages = with pkgs; [
            cargo
            cargo-deny
            nodejs
            nushell
            pnpm
          ];
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

          home-manager = import ./nix/home-manager-tests.nix {
            inherit home-manager pkgs;
            homeModule = self.homeModules.default;
          };

          overlay =
            assert (pkgs.extend self.overlays.default).oxide-code.drvPath == oxide-code.drvPath;
            pkgs.emptyFile;

          # Runs the test suite in the build sandbox, without the package's release build.
          tests = oxide-code.overrideAttrs {
            pname = "oxide-code-tests";
            buildPhase = "true";
            doCheck = true;
            installPhase = "touch $out";
            doInstallCheck = false;
          };
        };

        # ------------------------------------------------------------------------------------------
        # Formatter
        # ------------------------------------------------------------------------------------------
        formatter = pkgs.nixfmt-tree;
      }
    );
}
