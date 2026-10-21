{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:

# The v2 desktop agent: one `time.screen.minute` event a minute and a
# heartbeat every few, sent as OTLP to the local Alloy (or the homelab
# gateway). Replaces services.time-agent at go-live (M4).
let
  cfg = config.services.time-agent-v2;
  toml = pkgs.formats.toml { };
  agentConfig = toml.generate "time-agent.toml" {
    inherit (cfg)
      device
      endpoint
      blocklist
      browsers
      ;
    spool_dir = cfg.spoolDir;
    heartbeat_minutes = cfg.heartbeatMinutes;
  };
in
{
  options.services.time-agent-v2 = {
    enable = lib.mkEnableOption "the Time v2 desktop agent";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.time-v2;
      description = "Package providing `time-agent`.";
    };

    device = lib.mkOption {
      type = lib.types.str;
      example = "pc";
      description = "Name this machine reports as (`host.name`).";
    };

    endpoint = lib.mkOption {
      type = lib.types.str;
      default = "http://127.0.0.1:4318/v1/logs";
      description = ''
        OTLP/HTTP logs endpoint: the local Alloy when it runs on this machine,
        otherwise https://otlp.erikfrankling.duckdns.org/v1/logs.
      '';
    };

    tokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        File holding `TIME_OTLP_TOKEN=<token>`, for the homelab gateway. A
        sops secret path; never the token itself. Not needed for a local Alloy.
      '';
    };

    spoolDir = lib.mkOption {
      type = lib.types.str;
      default = "${config.xdg.dataHome}/time/spool-v2";
      description = "Where unsent events wait.";
    };

    blocklist = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [
        "keepassxc"
        "bitwarden"
        "vaultwarden"
        "1password"
        "lastpass"
        "signal"
        "gnome-keyring"
        "polkit"
        "private"
      ];
      description = ''
        Case-insensitive substrings of window class, title or browser host.
        A match sends a minute marked `blocked` with nothing else in it.
      '';
    };

    browsers = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [
        "firefox"
        "zen"
        "zen-beta"
        "chromium"
        "google-chrome"
        "helium"
        "brave-browser"
      ];
      description = "Window classes whose front tab is looked up via aw-watcher-web.";
    };

    heartbeatMinutes = lib.mkOption {
      type = lib.types.ints.positive;
      default = 5;
      description = "Minutes between heartbeats.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.time-agent-v2 = {
      Unit = {
        Description = "Time v2 desktop agent";
        After = [ "graphical-session.target" ];
        PartOf = [ "graphical-session.target" ];
      };
      Service = {
        ExecStart = "${lib.getExe' cfg.package "time-agent"} ${agentConfig}";
        Restart = "always";
        RestartSec = 10;
        # A minute's work is milliseconds; keep it out of the way of
        # everything else on the desktop.
        Nice = 10;
        MemoryMax = "128M";
      }
      // lib.optionalAttrs (cfg.tokenFile != null) { EnvironmentFile = cfg.tokenFile; };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
