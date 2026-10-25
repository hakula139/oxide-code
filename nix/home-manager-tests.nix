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

  evalHome =
    options:
    home-manager.lib.homeManagerConfiguration {
      inherit pkgs;
      modules = [
        homeModule
        {
          home = {
            homeDirectory = "/home/ox-test";
            username = "ox-test";
            stateVersion = "26.05";
          };

          programs.oxide-code = options;
        }
      ];
    };

  oxTargets = hm: builtins.filter (lib.hasPrefix "ox/") (builtins.attrNames hm.xdg.configFile);

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

  settings = {
    client.auth = {
      type = "bearer";
      file = "/run/secrets/ox-token";
    };

    permission.mode = "yolo";
    tui.status_line = [
      "model"
      "run-state"
    ];
  };

  checked =
    (evalHome {
      enable = true;
      package = stub;
      inherit settings;
      context = "Be terse.\n";
    }).config;

  rejected =
    (evalHome {
      enable = true;
      package = stub;
      settings.invalid = true;
    }).config;

  rejectedFailure = pkgs.testers.testBuildFailure rejected.programs.oxide-code.configDir;

  unchecked =
    (evalHome {
      enable = true;
      package = stub;
      checkConfig = false;
      settings.invalid = true;
    }).config;

  noPackage =
    (evalHome {
      enable = true;
      package = null;
      settings.invalid = true;
    }).config;

  disabled =
    (evalHome {
      package = stub;
      inherit settings;
      context = "Disabled.\n";
    }).config;

  defaults = (evalHome { enable = true; }).config;

  linkedContexts = {
    file = ../AGENTS.md;
    package = pkgs.writeText "ox-test-AGENTS.md" "From a package.\n";
  };

  linkedContextSources = lib.mapAttrs (
    _: context:
    toString
      (evalHome {
        enable = true;
        package = null;
        inherit context;
      }).config.xdg.configFile."ox/AGENTS.md".source
  ) linkedContexts;

  failures = lib.runTests {
    testEnabledInstallsTheSelectedPackageAndLinksCheckedFiles = {
      expr = {
        installsPackage = builtins.elem stub checked.home.packages;
        checksConfig = checked.programs.oxide-code.checkConfig;
        targets = oxTargets checked;
        configSource = checked.xdg.configFile."ox/config.toml".source;
      };

      expected = {
        installsPackage = true;
        checksConfig = true;
        targets = [
          "ox/AGENTS.md"
          "ox/config.toml"
        ];
        configSource = "${checked.programs.oxide-code.configDir}/ox/config.toml";
      };
    };

    testDefaultsInstallAPackageWithoutWritingConfig = {
      expr = {
        installsPackage = builtins.elem defaults.programs.oxide-code.package defaults.home.packages;
        targets = oxTargets defaults;
      };

      expected = {
        installsPackage = true;
        targets = [ ];
      };
    };

    testConfigOnlyWritesSettingsWithoutInstallingOrChecking = {
      expr = {
        packages = map toString noPackage.home.packages;
        checksConfig = noPackage.programs.oxide-code.checkConfig;
        targets = oxTargets noPackage;
      };

      expected = {
        packages = map toString disabled.home.packages;
        checksConfig = false;
        targets = [ "ox/config.toml" ];
      };
    };

    testDisabledDoesNotInstallOrWriteConfig = {
      expr = {
        installsPackage = builtins.elem stub disabled.home.packages;
        targets = oxTargets disabled;
      };

      expected = {
        installsPackage = false;
        targets = [ ];
      };
    };

    testLinkedContextKeepsItsSource = {
      expr = linkedContextSources;
      expected = lib.mapAttrs (_: toString) linkedContexts;
    };

    testCheckingRequiresAPackage = {
      expr =
        (builtins.tryEval
          (evalHome {
            enable = true;
            package = null;
            checkConfig = true;
          }).activationPackage.drvPath
        ).success;
      expected = false;
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
    jq -S . ${pkgs.writeText "expected-config.json" (builtins.toJSON settings)} > expected.json
    diff -u expected.json actual.json
    diff -u ${pkgs.writeText "expected-AGENTS.md" "Be terse.\n"} \
      ${checked.xdg.configFile."ox/AGENTS.md".source}
    test ! -e ${checked.programs.oxide-code.configDir}/ox/AGENTS.md

    test "$(cat ${rejectedFailure}/testBuildFailure.exit)" = 1
    grep -Fx "stub: invalid config" ${rejectedFailure}/testBuildFailure.log
    grep -F "programs.oxide-code.checkConfig = false" ${rejectedFailure}/testBuildFailure.log

    ${lib.concatMapAttrsStringSep "\n" (_: hm: ''
      toml2json < ${hm.xdg.configFile."ox/config.toml".source} | jq -e '. == {"invalid": true}'
    '') { inherit unchecked noPackage; }}

    touch "$out"
  ''
