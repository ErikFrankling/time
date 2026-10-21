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
  collectConfig = toml.generate "time-collect.toml" {
    inherit (cfg) device endpoint;
    inherit (cfg.collect) roots authors github;
  };
  backupScript = pkgs.writeShellScript "time-backup" ''
    set -euo pipefail
    export PATH=${
      lib.makeBinPath [
        pkgs.openssh
        pkgs.curl
        pkgs.jq
        pkgs.coreutils
        pkgs.findutils
        pkgs.procps
        cfg.package
      ]
    }:$PATH
    dir=${cfg.backup.dir}
    host=${cfg.backup.host}
    day=$(date +%F)
    mkdir -p "$dir"
    fail() {
      echo "time-backup: $*" >&2
      n=/run/current-system/sw/bin/notify-erik
      [ -x "$n" ] && "$n" -p high -t "Time backup failed" "$*" || true
      exit 1
    }
    k="sudo kubectl -n homelab exec deploy/time-v2 --"
    ssh "$host" "$k time-server backup /config/config.toml /tmp/time-backup.db" || fail "taking the snapshot"
    ssh "$host" "$k cat /tmp/time-backup.db" > "$dir/.time-$day.db.part" || fail "copying the snapshot"
    ssh "$host" "$k rm -f /tmp/time-backup.db" || true
    mv "$dir/.time-$day.db.part" "$dir/time-$day.db"

    # Restore test: a throwaway v2 server on the copy must answer with data.
    r=$(mktemp -d)
    trap 'pkill -f "time-server serve $r/config.toml" || true; rm -rf "$r"' EXIT
    cp "$dir/time-$day.db" "$r/time.db"
    printf 'data_dir = "%s"\nlisten = "127.0.0.1:17373"\n' "$r" > "$r/config.toml"
    time-server verify "$r/time.db" || fail "integrity check of $dir/time-$day.db"
    time-server serve "$r/config.toml" > "$r/log" 2>&1 &
    for _ in $(seq 1 60); do curl -sf http://127.0.0.1:17373/healthz >/dev/null && break; sleep 1; done
    status=$(curl -sf http://127.0.0.1:17373/api/status) || fail "restored server did not answer"
    labelled=$(echo "$status" | jq .pipeline.classifier.labelled_total)
    events=$(echo "$status" | jq .pipeline.events_total)
    [ "$events" -gt 0 ] || fail "restored copy has no events"
    echo "$(date -Is) $dir/time-$day.db restored: $events events, $labelled labelled minutes" | tee -a "$dir/restore-test.log"
    find "$dir" -name 'time-*.db' -mtime +14 -delete
  '';
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

    collect = {
      enable = lib.mkEnableOption "commit and pull-request collection (time-collect), every 15 minutes";
      roots = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ "~/projects" ];
        description = "Directories walked for git repositories.";
      };
      authors = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "Substrings of author name or email that are you. Empty: each repository's user.email.";
      };
      github = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Also fetch pull requests and reviews (token from the gh CLI login).";
      };
    };

    backup = {
      enable = lib.mkEnableOption "the nightly off-cluster backup of Time's database, with a restore test";
      host = lib.mkOption {
        type = lib.types.str;
        default = "naiaclaw";
        description = "ssh host that can run kubectl against the cluster running time-v2.";
      };
      dir = lib.mkOption {
        type = lib.types.str;
        default = "${config.home.homeDirectory}/backups/time";
        description = "Where the copies are kept (14 days).";
      };
    };

    heartbeatMinutes = lib.mkOption {
      type = lib.types.ints.positive;
      default = 5;
      description = "Minutes between heartbeats.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.time-backup = lib.mkIf cfg.backup.enable {
      Unit.Description = "Time: pull a consistent database copy off the cluster and restore-test it";
      Service = {
        Type = "oneshot";
        ExecStart = "${backupScript}";
        Nice = 10;
      };
    };
    systemd.user.timers.time-backup = lib.mkIf cfg.backup.enable {
      Unit.Description = "Time: nightly off-cluster backup";
      Timer = {
        OnCalendar = "*-*-* 04:15:00";
        Persistent = true;
      };
      Install.WantedBy = [ "timers.target" ];
    };

    systemd.user.services.time-collect = lib.mkIf cfg.collect.enable {
      Unit.Description = "Time: commits and pull requests";
      Service = {
        Type = "oneshot";
        ExecStart = "${lib.getExe' cfg.package "time-collect"} ${collectConfig}";
        Nice = 10;
      }
      // lib.optionalAttrs (cfg.tokenFile != null) { EnvironmentFile = cfg.tokenFile; };
    };
    systemd.user.timers.time-collect = lib.mkIf cfg.collect.enable {
      Unit.Description = "Time: commits and pull requests, every 15 minutes";
      Timer = {
        OnBootSec = "5min";
        OnUnitActiveSec = "15min";
      };
      Install.WantedBy = [ "timers.target" ];
    };

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
