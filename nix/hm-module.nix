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

  # The collector settings live under [server] because that is where the code
  # reads them, but they are used here, on the workstation: the repositories are
  # on this disk and the database is in the cluster, so the scan has to run on
  # this side and post its results like the agent does.
  collectToml = lib.optionalString cfg.collect.enable ''

    [server]
    code_roots = [${lib.concatMapStringsSep ", " (r: ''"${r}"'') cfg.collect.roots}]
    code_days = ${toString cfg.collect.days}
    ${lib.optionalString (cfg.collect.githubUser != null) ''github_user = "${cfg.collect.githubUser}"''}
  '';

  agentToml = pkgs.writeText "time-config.toml" ''
    [agent]
    server = "${cfg.server}"
    device = "${cfg.device}"
    width = ${toString cfg.width}
    ${lib.optionalString (cfg.note != null) ''note = "${escapeToml cfg.note}"''}
    blocklist = [
    ${lib.concatMapStringsSep "\n" (b: ''  "${b}",'') cfg.blocklist}
    ]
    ${collectToml}
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
      default = 768;
      description = ''
        Downscale width before sending. This is the cost dial -- larger is
        more legible to the model and more expensive per minute. It stops
        buying accuracy well before 1024: 768 and 1024 labelled the same
        twelve minutes identically over six runs each, so the default sits
        at the narrow end.
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

    collect = {
      enable = lib.mkEnableOption ''
        the nightly output collector. Reads commits and diffs out of the local
        clones and pull requests out of GitHub, and posts a per-day summary to
        the same server. Nightly is enough: a commit timestamp never changes
      '';

      roots = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ "~/projects" ];
        description = ''
          Directories walked for git repositories. The walk stops at the first
          `.git` on a path, so naming a parent of everything is the intent.
        '';
      };

      githubUser = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          GitHub login, for pull requests, issues and reviews. The token is
          never configured here: it comes from TIME_GITHUB_TOKEN, or from the
          `gh` CLI's own credentials if it is logged in.
        '';
      };

      days = lib.mkOption {
        type = lib.types.ints.positive;
        default = 30;
        description = ''
          How far back each run looks. Overlapping runs are free -- rows are
          replaced, not added -- so this is sized for a laptop that was shut
          for a fortnight rather than for the gap since last night.
        '';
      };
    };

    agents = {
      enable = lib.mkEnableOption ''
        the nightly agent-session summary. Reads local Claude Code, codex and
        opencode session state and posts per-day and per-minute totals, so
        "how much of that was me" becomes answerable. Reads only counts and
        lengths -- never prompt or response text
      '';
    };

    metrics = {
      enable = lib.mkEnableOption ''
        the near-live metrics timer. Runs `time collect 2` and `time agents 2`
        on a short interval so today's commit and token numbers reach the
        dashboard within minutes instead of at the nightly run. Affordable at
        one minute because both commands keep a scan cache in the data dir and
        skip any source whose files have not changed since the previous run
      '';

      interval = lib.mkOption {
        type = lib.types.str;
        default = "1m";
        description = ''
          How long after one run starts the next may start (systemd
          OnUnitActiveSec). Runs never overlap: the timer only re-triggers
          the same oneshot unit, and a unit still running is a single job.
        '';
      };
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

    systemd.user.services.time-collect = lib.mkIf cfg.collect.enable {
      Unit.Description = "collect committed output and post it to the time server";
      Service = {
        Type = "oneshot";
        ExecStart = "${cfg.package}/bin/time collect";
        # Needs `git` for nothing and `gh` for its stored credentials. A user
        # unit does not inherit the login shell's PATH, so say where to look.
        Environment = [ "PATH=%h/.nix-profile/bin:/run/current-system/sw/bin" ];
        Slice = "background.slice";
        Nice = 15;
      };
    };

    systemd.user.timers.time-collect = lib.mkIf cfg.collect.enable {
      Unit.Description = "nightly output collection";
      Timer = {
        OnCalendar = "*-*-* 03:30:00";
        # A laptop asleep at half three still gets its numbers, late.
        Persistent = true;
        RandomizedDelaySec = "20m";
      };
      Install.WantedBy = [ "timers.target" ];
    };

    systemd.user.services.time-agents = lib.mkIf cfg.agents.enable {
      Unit.Description = "summarise AI agent sessions and post them to the time server";
      Service = {
        Type = "oneshot";
        ExecStart = "${cfg.package}/bin/time agents";
        Slice = "background.slice";
        # Transcripts run to gigabytes, so this reads a lot more than the git
        # collector does. Keep it well out of the way of anything interactive.
        Nice = 19;
        IOSchedulingClass = "idle";
      };
    };

    systemd.user.timers.time-agents = lib.mkIf cfg.agents.enable {
      Unit.Description = "nightly agent-session summary";
      Timer = {
        # After the git collector rather than alongside it: both walk large
        # trees and there is nothing to gain from doing it at the same time.
        OnCalendar = "*-*-* 04:00:00";
        Persistent = true;
        RandomizedDelaySec = "20m";
      };
      Install.WantedBy = [ "timers.target" ];
    };

    systemd.user.services.time-metrics = lib.mkIf cfg.metrics.enable {
      Unit.Description = "near-live commit and agent metrics for the time server";
      Service = {
        Type = "oneshot";
        # Both halves run even if one fails -- a broken GitHub token must not
        # also stop the token numbers -- but a failure is still reported.
        ExecStart = "${pkgs.writeShellScript "time-metrics" ''
          status=0
          ${cfg.package}/bin/time collect 2 || status=$?
          ${cfg.package}/bin/time agents 2 || status=$?
          exit $status
        ''}";
        # Needs `git` and `gh` for its stored credentials, same as the nightly
        # collector: a user unit does not inherit the login shell's PATH.
        Environment = [ "PATH=%h/.nix-profile/bin:/run/current-system/sw/bin" ];
        Slice = "background.slice";
        # Runs every minute next to interactive work, so stay maximally out of
        # the way -- the scan cache keeps the common run to a few stat calls.
        Nice = 19;
        IOSchedulingClass = "idle";
      };
    };

    systemd.user.timers.time-metrics = lib.mkIf cfg.metrics.enable {
      Unit.Description = "near-live metrics on an interval";
      Timer = {
        OnBootSec = cfg.metrics.interval;
        # Measured from the last activation of the oneshot unit, so a slow
        # scan delays the next run instead of stacking on top of it.
        # Persistent is pointless at this cadence: the next tick is never far.
        OnUnitActiveSec = cfg.metrics.interval;
      };
      Install.WantedBy = [ "timers.target" ];
    };
  };
}
