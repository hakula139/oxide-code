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

  contextPackage = pkgs.writeText "ox-test-AGENTS.md" "From a package.\n";

  stub = pkgs.writeShellScriptBin "ox" ''
    set -eu
    fail() {
      echo "stub: $1" >&2
      exit 1
    }
    [ "$*" = "config check" ] || fail "expected config check"
    [ -d "$XDG_CONFIG_HOME/ox" ] || fail "expected the config directory"
    [ -z "$(ls -A "$PWD")" ] || fail "expected an empty working directory"
    [ -d "$HOME" ] && [ -w "$HOME" ] && [ -z "$(ls -A "$HOME")" ] ||
      fail "expected an empty writable home directory"
    [ -n "$XDG_DATA_HOME" ] && [ -n "$XDG_STATE_HOME" ] || fail "expected data and state dirs"
    if grep -q invalid "$XDG_CONFIG_HOME/ox/config.toml"; then
      fail "invalid config"
    fi
  '';

  hintLines = [
    "ox config check rejected the config.toml generated from programs.oxide-code.settings."
    "If it names a path the build cannot read, set programs.oxide-code.checkConfig = false."
  ];

  # ------------------------------------------------------------------------------------------------
  # Cases
  # ------------------------------------------------------------------------------------------------
  checked = evalHome {
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

  rejected = evalHome {
    programs.oxide-code = {
      enable = true;
      package = stub;
      settings.invalid = true;
    };
  };

  rejectedFailure = pkgs.testers.testBuildFailure rejected.programs.oxide-code.configDir;

  unchecked = evalHome {
    programs.oxide-code = {
      enable = true;
      package = stub;
      checkConfig = false;
      settings.invalid = true;
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

  checkNeedsPackage = evalHome {
    programs.oxide-code = {
      enable = true;
      package = null;
      checkConfig = true;
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
          checked
          unchecked
          defaults
          contextFile
          contextFromPackage
          noPackage
          disabled
          ;
      };

      expected = {
        checked = [ ];
        unchecked = [ ];
        defaults = [ ];
        contextFile = [ ];
        contextFromPackage = [ ];
        noPackage = [ ];
        disabled = [ ];
      };
    };

    testCheckedTargets = {
      expr = oxTargets checked;
      expected = [
        "ox/AGENTS.md"
        "ox/config.toml"
      ];
    };

    testCheckedLinksTheConfigFromTheCheckedDirectory = {
      expr = checked.xdg.configFile."ox/config.toml".source;
      expected = "${checked.programs.oxide-code.configDir}/ox/config.toml";
    };

    testCheckedInstallsThePackage = {
      expr = builtins.elem stub checked.home.packages;
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

    testCheckNeedsPackageFails = {
      expr = failedAssertions checkNeedsPackage;
      expected = [
        (
          "programs.oxide-code.checkConfig runs ox from programs.oxide-code.package, "
          + "which is null. Set a package or checkConfig = false."
        )
      ];
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
    toml2json < ${checked.xdg.configFile."ox/config.toml".source} | jq -S . > actual.json
    jq -S . > expected.json <<'EOF'
    {"permission":{"mode":"yolo"},"tui":{"status_line":["model","run-state"]}}
    EOF
    diff -u expected.json actual.json

    diff -u ${pkgs.writeText "expected-AGENTS.md" "Be terse.\n"} \
      ${checked.xdg.configFile."ox/AGENTS.md".source}
    diff -u ${./fixtures/AGENTS.md} ${contextFile.xdg.configFile."ox/AGENTS.md".source}

    test "$(cat ${rejectedFailure}/testBuildFailure.exit)" = 1
    grep -Fx "stub: invalid config" ${rejectedFailure}/testBuildFailure.log
    ${lib.concatMapStrings (line: ''
      grep -Fx ${lib.escapeShellArg line} ${rejectedFailure}/testBuildFailure.log
    '') hintLines}

    toml2json < ${unchecked.programs.oxide-code.configDir}/ox/config.toml | jq -S . > actual.json
    echo '{"invalid":true}' | jq -S . > expected.json
    diff -u expected.json actual.json

    test -d ${noPackage.programs.oxide-code.configDir}/ox

    touch $out
  ''
