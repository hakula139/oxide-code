# ==================================================================================================
# oxide-code Home Manager Module Tests
# ==================================================================================================

{
  home-manager,
  homeModule,
  pkgs,
}:

let
  inherit (pkgs) lib;

  # Mirrors Home Manager's modules/default.nix with the caller's pkgs, as useGlobalPkgs does,
  # and without its assertion check, which throws before a test can read the failed messages.
  evalHome =
    module:
    let
      hmLib = import "${home-manager}/modules/lib/stdlib-extended.nix" lib;
    in
    (hmLib.evalModules {
      class = "homeManager";
      specialArgs.modulesPath = "${home-manager}/modules";
      modules =
        import "${home-manager}/modules/modules.nix" {
          inherit pkgs;
          lib = hmLib;
          useNixpkgsModule = false;
        }
        ++ [
          homeModule
          {
            home = {
              homeDirectory = "/home/ox-test";
              username = "ox-test";
              stateVersion = "26.05";
            };
          }
          module
        ];
    }).config;

  oxTargets = hm: builtins.filter (lib.hasPrefix "ox/") (builtins.attrNames hm.xdg.configFile);
  failedAssertions = hm: map (a: a.message) (builtins.filter (a: !a.assertion) hm.assertions);
  complaints = hm: failedAssertions hm ++ hm.warnings;

  stub = pkgs.writeShellScriptBin "ox" "exit 0";
  contextPackage = pkgs.writeText "ox-test-AGENTS.md" "From a package.\n";

  # ------------------------------------------------------------------------------------------------
  # Cases
  # ------------------------------------------------------------------------------------------------
  full = evalHome {
    programs.oxide-code = {
      enable = true;
      package = stub;

      settings = {
        permission.mode = "yolo";
        tui.status_line = [
          "model"
          "run-state"
        ];
      };

      context = "Be terse.\n";
    };
  };

  defaults = evalHome { programs.oxide-code.enable = true; };

  contextFile = evalHome {
    programs.oxide-code = {
      enable = true;
      package = stub;
      context = ./fixtures/AGENTS.md;
    };
  };

  contextFromPackage = evalHome {
    programs.oxide-code = {
      enable = true;
      package = stub;
      context = contextPackage;
    };
  };

  noPackage = evalHome {
    programs.oxide-code = {
      enable = true;
      package = null;
      settings.tui.show_thinking = true;
    };
  };

  disabled = evalHome { programs.oxide-code.settings.tui.show_thinking = true; };

  apiKey = evalHome {
    programs.oxide-code = {
      enable = true;
      package = stub;
      settings.client.api_key = "sk-test";
    };
  };

  # ------------------------------------------------------------------------------------------------
  # Evaluation Tests
  # ------------------------------------------------------------------------------------------------
  failures = lib.runTests {
    testComplaints = {
      expr = lib.mapAttrs (_: complaints) {
        inherit
          full
          defaults
          contextFile
          contextFromPackage
          noPackage
          disabled
          ;
      };

      expected = {
        full = [ ];
        defaults = [ ];
        contextFile = [ ];
        contextFromPackage = [ ];
        noPackage = [ ];
        disabled = [ ];
      };
    };

    testFullTargets = {
      expr = oxTargets full;
      expected = [
        "ox/AGENTS.md"
        "ox/config.toml"
      ];
    };

    testFullInstallsThePackage = {
      expr = builtins.elem stub full.home.packages;
      expected = true;
    };

    testDefaultsWriteNothing = {
      expr = oxTargets defaults;
      expected = [ ];
    };

    testDefaultsBuildTheFlakePackage = {
      expr = defaults.programs.oxide-code.package.pname;
      expected = "oxide-code";
    };

    testContextFromPackageLinksIt = {
      expr = toString contextFromPackage.xdg.configFile."ox/AGENTS.md".source;
      expected = toString contextPackage;
    };

    testNoPackageStillWritesTheConfig = {
      expr = oxTargets noPackage;
      expected = [ "ox/config.toml" ];
    };

    testNoPackageInstallsNothing = {
      expr = map toString noPackage.home.packages;
      expected = map toString (evalHome { }).home.packages;
    };

    testDisabledWritesNothing = {
      expr = oxTargets disabled;
      expected = [ ];
    };

    testApiKeyFails = {
      expr = failedAssertions apiKey;
      expected = [
        (
          "programs.oxide-code.settings.client.api_key would write the key into the Nix store, "
          + "which every local user can read. Export ANTHROPIC_API_KEY from your secret manager "
          + "instead."
        )
      ];
    };
  };
in
assert lib.assertMsg (failures == [ ]) "failed tests: ${builtins.toJSON failures}";
pkgs.runCommand "ox-home-manager-module"
  {
    nativeBuildInputs = [
      pkgs.jq
      pkgs.remarshal
    ];
  }
  ''
    toml2json < ${full.xdg.configFile."ox/config.toml".source} | jq -S . > actual.json
    jq -S . > expected.json <<'EOF'
    {"permission":{"mode":"yolo"},"tui":{"status_line":["model","run-state"]}}
    EOF
    diff -u expected.json actual.json

    diff -u ${pkgs.writeText "expected-AGENTS.md" "Be terse.\n"} \
      ${full.xdg.configFile."ox/AGENTS.md".source}
    diff -u ${./fixtures/AGENTS.md} ${contextFile.xdg.configFile."ox/AGENTS.md".source}

    touch $out
  ''
