# ==================================================================================================
# oxide-code Home Manager Module
# ==================================================================================================

{
  mkOxideCode,
}:

{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.oxide-code;
  toml = pkgs.formats.toml { };
in
{
  # ------------------------------------------------------------------------------------------------
  # Module Options
  # ------------------------------------------------------------------------------------------------
  options.programs.oxide-code = {
    enable = lib.mkEnableOption "oxide-code, the terminal AI coding assistant `ox`";

    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = mkOxideCode pkgs;
      defaultText = lib.literalMD "oxide-code built from this flake with `pkgs`";
      description = ''
        The oxide-code package to install, or `null` to manage only the configuration.
      '';
    };

    settings = lib.mkOption {
      inherit (toml) type;
      default = { };
      example = lib.literalExpression ''
        {
          client.effort = "high";
          tui = {
            show_thinking = true;
            status_line = [ "model-with-effort" "context-used" "run-state" ];
          };
        }
      '';
      description = ''
        Configuration written to {file}`$XDG_CONFIG_HOME/ox/config.toml`. The configuration
        guide lists every key:
        <https://github.com/hakula139/oxide-code/blob/main/docs/guide/configuration.md>.
      '';
    };

    context = lib.mkOption {
      type = lib.types.either lib.types.lines lib.types.path;
      default = "";
      example = lib.literalExpression "./AGENTS.md";
      description = ''
        User instructions for {file}`$XDG_CONFIG_HOME/ox/AGENTS.md`: inline text, or a Nix path or
        package linked as the file. A string is always inline text.
      '';
    };
  };

  # ------------------------------------------------------------------------------------------------
  # Module Config
  # ------------------------------------------------------------------------------------------------
  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = !(cfg.settings ? client.api_key);
        message =
          "programs.oxide-code.settings.client.api_key would write the key into the Nix store, "
          + "which every local user can read. Export ANTHROPIC_API_KEY from your secret manager "
          + "instead.";
      }
    ];

    home.packages = lib.mkIf (cfg.package != null) [ cfg.package ];

    xdg.configFile = {
      "ox/config.toml" = lib.mkIf (cfg.settings != { }) {
        source = toml.generate "ox-config.toml" cfg.settings;
      };

      "ox/AGENTS.md" = lib.mkIf (cfg.context != "") (
        if builtins.isString cfg.context then { text = cfg.context; } else { source = cfg.context; }
      );
    };
  };
}
