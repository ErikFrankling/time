{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.time-agent;

  # Flatten to one line and escape, so a multi-line note stays a valid TOML
  # basic string rather than silently breaking the config file.
  escapeToml =
    s:
    lib.escape [ "\\" "\"" ] (
      lib.concatStringsSep " " (lib.filter (x: x != "" && !lib.isList x) (builtins.split "[[:space:]]+" s))
    );

  agentToml = pkgs.writeText "time-config.toml" ''
    [agent]
    server = "${cfg.server}"
    device = "${cfg.device}"
    width = ${toString cfg.width}
    ${lib.optionalString (cfg.note != null) ''note = "${escapeToml cfg.note}"''}
    blocklist = [
    ${lib.concatMapStringsSep "\n" (b: ''  "${b}",'') cfg.blocklist}
    ]
  '';

in
{
  options.services.time-agent = {
    enable = lib.mkEnableOption "the time activity-tracking agent";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "The time package to run.";
    };

    server = lib.mkOption {
      type = lib.types.str;
      example = "https://time.example.org";
      description = "Base URL of the time server that does the classifying.";
    };

    device = lib.mkOption {
      type = lib.types.str;
      description = "Name this machine reports as. Minutes are keyed by it.";
    };

    width = lib.mkOption {
      type = lib.types.ints.positive;
      default = 1024;
      description = ''
        Downscale width before sending. This is the cost dial -- larger is
        more legible to the model and more expensive per minute.
      '';
    };

    blocklist = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [
        "vaultwarden"
        "bitwarden"
        "keepass"
        "signal"
        "gnome-keyring"
        "polkit"
        "private"
      ];
      description = ''
        Case-insensitive substrings matched against the active window class
        and title. A matching window is never screenshotted and its title is
        not sent either, so nothing about it leaves the machine.
      '';
    };

    note = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "This machine runs unattended AI agent sessions.";
      description = ''
        Free-text context about this machine, passed to the model with every
        frame. Use it for things a screenshot alone would misrepresent -- for
        instance that an agent drives this screen with nobody present, so
        visible activity is not evidence the user is here.
      '';
    };

  };

  config = lib.mkIf cfg.enable {
    # Deliberately not adding cfg.package to home.packages: it installs a
    # binary called `time`, which would shadow GNU time on PATH.
    home.packages = [
      (pkgs.writeShellScriptBin "time-agent" ''
        exec ${cfg.package}/bin/time "$@"
      '')
    ];

    xdg.configFile."time/config.toml".source = agentToml;

    systemd.user.services.time-agent = {
      Unit = {
        Description = "time activity-tracking agent";
        # Screenshotting needs a running compositor, so this belongs to the
        # graphical session rather than the login session.
        PartOf = [ "graphical-session.target" ];
        After = [ "graphical-session.target" ];
        X-Restart-Triggers = [ "${agentToml}" ];
      };

      Service = {
        # No credentials to load: the server is reachable only on the LAN and
        # over the VPN, which is the whole access control.
        ExecStart = "${cfg.package}/bin/time agent";
        # The server being down, the VPN dropping, or a compositor restart are
        # all expected over months of running. Always come back.
        Restart = "always";
        RestartSec = 30;
        Slice = "background.slice";
        Nice = 10;
      };

      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
