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

  credentialFileType = lib.types.either (lib.types.pathWith {
    inStore = false;
    absolute = true;
  }) (lib.types.strMatching "~/.*");

  credentialAssertions = [
    {
      assertion = !(cfg.settings ? client.api_key);
      message =
        "programs.oxide-code.settings.client.api_key would write the key into the Nix store, "
        + "which every local user can read. Export ANTHROPIC_API_KEY from your secret manager "
        + "instead.";
    }

    {
      assertion = !(cfg.settings ? client.auth.value);
      message =
        "programs.oxide-code.settings.client.auth.value would write the credential into the Nix store, "
        + "which every local user can read. Set client.auth.file to a secret file path.";
    }

    {
      assertion =
        !(cfg.settings ? client.auth.file)
        || (
          let
            file = cfg.settings.client.auth.file;
          in
          credentialFileType.check file && !(builtins.hasContext file)
        );
      message =
        "programs.oxide-code.settings.client.auth.file must be a quoted absolute or ~/ runtime path "
        + "outside the Nix store, without string context. Interpolating a Nix path can already copy "
        + "its contents into the store.";
    }
  ];

  checkedSettings =
    let
      failures = builtins.filter (check: !check.assertion) credentialAssertions;
    in
    if failures == [ ] then
      cfg.settings
    else
      throw (lib.concatMapStringsSep "\n" (check: check.message) failures);

  contextTarget =
    if builtins.isString cfg.context then { text = cfg.context; } else { source = cfg.context; };

  # Only the files the check reads go through configDir, so an out-of-store context link stays live.
  checkedFiles = lib.optionalAttrs (cfg.settings != { }) {
    "config.toml" = toml.generate "ox-config.toml" checkedSettings;
  };

  configDir = pkgs.runCommandLocal "ox-config" { } ''
    mkdir -p "$out/ox"
    ${lib.concatStrings (
      lib.mapAttrsToList (rel: source: ''
        install -D -m 0444 ${source} "$out/ox/${rel}"
      '') checkedFiles
    )}
    ${lib.optionalString cfg.checkConfig ''
      # crates/ox/tests/cli.rs runs `ox config check` in this same scratch environment.
      export HOME=$TMPDIR/home XDG_CONFIG_HOME=$out \
        XDG_DATA_HOME=$TMPDIR/data XDG_STATE_HOME=$TMPDIR/state
      mkdir "$HOME" "$TMPDIR/work"
      cd "$TMPDIR/work"
      ${lib.getExe' cfg.package "ox"} config check || {
        echo "ox config check rejected the config.toml generated from" \
          "programs.oxide-code.settings." >&2
        echo "If it names a path the build cannot read," \
          "set programs.oxide-code.checkConfig = false." >&2
        exit 1
      }
    ''}
  '';
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

    checkConfig = lib.mkOption {
      type = lib.types.bool;
      default = cfg.package != null;
      defaultText = lib.literalExpression "config.programs.oxide-code.package != null";
      description = ''
        Whether to validate the generated configuration at build time with
        {command}`ox config check` from {option}`programs.oxide-code.package`.
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

    configDir = lib.mkOption {
      type = lib.types.package;
      readOnly = true;
      internal = true;
      description = ''
        The generated directory holding `ox/` with every file `ox config check` reads, checked
        when {option}`programs.oxide-code.checkConfig` is on.
      '';
    };
  };

  # ------------------------------------------------------------------------------------------------
  # Module Config
  # ------------------------------------------------------------------------------------------------
  config = lib.mkIf cfg.enable {
    programs.oxide-code.configDir = configDir;

    assertions = [
      {
        assertion = cfg.checkConfig -> cfg.package != null;
        message =
          "programs.oxide-code.checkConfig runs ox from programs.oxide-code.package, "
          + "which is null. Set a package or checkConfig = false.";
      }

    ]
    ++ credentialAssertions;

    home.packages = lib.mkIf (cfg.package != null) [ cfg.package ];

    xdg.configFile =
      lib.mapAttrs' (
        rel: _: lib.nameValuePair "ox/${rel}" { source = "${configDir}/ox/${rel}"; }
      ) checkedFiles
      // {
        "ox/AGENTS.md" = lib.mkIf (cfg.context != "") contextTarget;
      };
  };
}
