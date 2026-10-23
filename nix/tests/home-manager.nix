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

  homeDirectory = if pkgs.stdenv.hostPlatform.isDarwin then "/Users/ox-test" else "/home/ox-test";

  # Mirrors Home Manager's modules/default.nix without its assertion check, which throws before a
  # test can read the failed messages.
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
              inherit homeDirectory;
              username = "ox-test";
              stateVersion = "26.05";
            };
          }
          module
        ];
    }).config;

  oxTargets = hm: builtins.filter (lib.hasPrefix "ox/") (builtins.attrNames hm.xdg.configFile);
  failedAssertions = hm: map (a: a.message) (builtins.filter (a: !a.assertion) hm.assertions);

  expectEqual =
    name: actual: expected:
    lib.assertMsg (
      actual == expected
    ) "${name}: expected ${builtins.toJSON expected}, got ${builtins.toJSON actual}";

  stub = pkgs.writeShellScriptBin "ox" "exit 0";

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
in
assert expectEqual "full targets" (oxTargets full) [
  "ox/AGENTS.md"
  "ox/config.toml"
];
assert expectEqual "full assertions" (failedAssertions full) [ ];
assert expectEqual "full package" (builtins.elem stub full.home.packages) true;
assert expectEqual "defaults targets" (oxTargets defaults) [ ];
assert expectEqual "defaults package" defaults.programs.oxide-code.package.pname "oxide-code";
assert expectEqual "no package" (map toString noPackage.home.packages) (
  map toString (evalHome { }).home.packages
);
assert expectEqual "disabled targets" (oxTargets disabled) [ ];
assert expectEqual "API key assertions" (failedAssertions apiKey) [
  (
    "programs.oxide-code.settings.client.api_key would write the key into the Nix store, "
    + "which every local user can read. Export ANTHROPIC_API_KEY from your secret manager "
    + "instead."
  )
];
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
