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

    devkit = {
      url = "github:hakula139/devkit/v3";
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
      devkit,
      home-manager,
      ...
    }:
    let
      packageFor =
        pkgs:
        self.packages.${pkgs.stdenv.hostPlatform.system}.oxide-code
          or (pkgs.callPackage ./nix/package.nix { });

      # A key lets the module system deduplicate the module when it is imported twice.
      homeModuleKey = "${self}/flake.nix#homeModules.default";
    in
    # ----------------------------------------------------------------------------------------------
    # Overlay and Home Manager Module
    # ----------------------------------------------------------------------------------------------
    {
      overlays.default = final: _: { oxide-code = packageFor final; };

      homeModules.default = {
        _class = "homeManager";
        _file = homeModuleKey;
        key = homeModuleKey;
        imports = [ (import ./nix/home-manager.nix { inherit packageFor; }) ];
      };
    }
    // devkit.lib.eachSystem devkit.lib.systems (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        dk = devkit.lib.mkLib pkgs;

        rustBin = rust-overlay.lib.mkRustBin { } pkgs;
        rustToolchain = rustBin.fromRustupToolchainFile ./rust-toolchain.toml;
        rustDevToolchain = rustToolchain.override (prev: {
          extensions = prev.extensions ++ [
            "rust-analyzer"
            "rust-src"
          ];
        });

        oxide-code = pkgs.callPackage ./nix/package.nix { };

        # ------------------------------------------------------------------------------------------
        # Pre-commit Hooks
        # ------------------------------------------------------------------------------------------
        preCommit = dk.preCommit {
          src = ./.;
          hooks = {
            actionlint.enable = true;
            check-shebang-scripts-are-executable.enable = true;
            check-yaml.enable = true;
            cspell.enable = true;
            deadnix.enable = true;
            dprint.enable = true;
            markdownlint-cli2.enable = true;
            nixfmt.enable = true;
            prettier.enable = true;
            shellcheck.enable = true;
            shfmt.enable = true;
            sort-file-contents.enable = true;
            statix.enable = true;
            taplo.enable = true;
          };

          # CI checks formatting without fetching the pinned toolchain in the Nix sandbox.
          devHooks.rustfmt = {
            enable = true;
            packageOverrides = {
              cargo = rustToolchain;
              rustfmt = rustToolchain;
            };
          };
        };

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
          ++ (with pkgs; [
            cargo-deny
            cargo-insta
            cargo-llvm-cov
            cargo-mutants
            git-cliff
            sqlite
          ]);

          inputsFrom = [ preCommit.shell ];
        };

        # CI's Lint job runs in this shell, which leaves out the pinned toolchain so that every tool
        # comes from the nixpkgs binary cache. `cargo deny` needs only `cargo metadata`, which
        # nixpkgs' cargo serves for this workspace.
        devShells.lint = pkgs.mkShell {
          name = "oxide-code-lint";

          packages = with pkgs; [
            cargo
            cargo-deny
            nushell
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
          pre-commit = preCommit.check;

          home-manager = import ./nix/home-manager-tests.nix {
            inherit home-manager oxide-code pkgs;
            homeModule = self.homeModules.default;
          };

          overlay =
            let
              callerPkgs = pkgs // {
                callPackage = _: _: throw "consumer package reconstruction";
              };
            in
            assert (self.overlays.default callerPkgs pkgs).oxide-code.drvPath == oxide-code.drvPath;
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
