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
      url = "github:hakulab/devkit/v3";
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
    }:
    let
      platformKey = p: {
        inherit (p) config isStatic;
      };

      packageFor =
        pkgs:
        let
          inherit (pkgs) stdenv;
          locked = self.packages.${stdenv.hostPlatform.system}.oxide-code or null;
        in
        if
          locked != null
          && platformKey stdenv.buildPlatform == platformKey locked.stdenv.buildPlatform
          && platformKey stdenv.hostPlatform == platformKey locked.stdenv.hostPlatform
          && stdenv.hostPlatform == stdenv.targetPlatform
        then
          locked
        else
          pkgs.callPackage ./nix/package.nix { };

      # A key lets the module system deduplicate the module when it is imported twice.
      homeModuleKey = "${self}/flake.nix#homeModules.default";
    in
    # ----------------------------------------------------------------------------------------------
    # Overlay and Home Manager Module
    # ----------------------------------------------------------------------------------------------
    {
      overlays.default = _: prev: { oxide-code = packageFor prev; };

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

        crossPkgs =
          if pkgs.stdenv.hostPlatform.isAarch64 then
            pkgs.pkgsCross.gnu64
          else
            pkgs.pkgsCross.aarch64-multiplatform;

        rustVersion = (pkgs.lib.importTOML ./Cargo.toml).workspace.package.rust-version;
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
            check-shebang-scripts-are-executable.enable = true;

            clippy = {
              enable = true;
              packageOverrides = {
                cargo = rustDevToolchain;
                clippy = rustDevToolchain;
              };
            };

            prettier.enable = true;

            rustfmt = {
              enable = true;
              packageOverrides = {
                cargo = rustDevToolchain;
                rustfmt = rustDevToolchain;
              };
            };

            shellcheck.enable = true;
            shfmt.enable = true;
            taplo.enable = true;
          };
        };

      in
      {
        # ------------------------------------------------------------------------------------------
        # Dev Shells
        # ------------------------------------------------------------------------------------------
        devShells = {
          default = pkgs.mkShell {
            name = "oxide-code-dev";

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

            inputsFrom = [ preCommit ];
          };

          # Use nixpkgs' cargo for cargo-deny's metadata so CI needs no pinned toolchain build.
          lint = pkgs.mkShell {
            name = "oxide-code-lint";

            packages = [
              pkgs.cargo
              pkgs.cargo-deny
            ];
          };

          msrv = pkgs.mkShell {
            name = "oxide-code-msrv";

            packages = [
              rustBin.stable.${pkgs.lib.versions.pad 3 rustVersion}.minimal
            ];
          };
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
          package = oxide-code;

          home-manager = import ./nix/home-manager-tests.nix {
            inherit
              pkgs
              crossPkgs
              home-manager
              oxide-code
              ;
            homeModule = self.homeModules.default;
          };

          overlay =
            let
              consumerPkgs =
                (pkgs.extend (
                  _: prev: {
                    cargo-about = prev.cargo-about.overrideAttrs {
                      pname = "cargo-about-consumer";
                    };
                  }
                )).extend
                  self.overlays.default;

              staticOxideCode = (pkgs.pkgsStatic.extend self.overlays.default).oxide-code;
              muslOxideCode = (pkgs.pkgsMusl.extend self.overlays.default).oxide-code;
              crossOxideCode = (crossPkgs.extend self.overlays.default).oxide-code;
            in
            assert consumerPkgs.oxide-code.drvPath == oxide-code.drvPath;
            assert staticOxideCode.drvPath != oxide-code.drvPath;
            assert staticOxideCode.stdenv.hostPlatform.isStatic;
            assert pkgs.stdenv.hostPlatform.isLinux -> muslOxideCode.drvPath != oxide-code.drvPath;
            assert pkgs.stdenv.hostPlatform.isLinux -> muslOxideCode.stdenv.hostPlatform.isMusl;
            assert crossOxideCode.drvPath != oxide-code.drvPath;
            assert !crossOxideCode.doInstallCheck;
            pkgs.emptyFile;
        };

        # ------------------------------------------------------------------------------------------
        # Formatter
        # ------------------------------------------------------------------------------------------
        formatter = pkgs.nixfmt-tree;
      }
    );
}
