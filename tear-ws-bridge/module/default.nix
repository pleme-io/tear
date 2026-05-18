# Home-manager module for the tear-ws-bridge sidecar.
#
# The bridge exposes the tear-daemon CBOR wire over ws:// so any
# browser / wasm renderer can attach without speaking UDS or TCP
# directly. Lifecycle is managed by a per-user launchd agent on
# Darwin or a systemd user unit on Linux — independent of the
# tear-daemon's own unit, so operators can enable browser access
# without touching the daemon's posture.
#
# Usage from a consumer flake:
#
#   imports = [ inputs.tear.homeManagerModules.ws-bridge ];
#   programs.tear-ws-bridge = {
#     enable = true;
#     listen = "127.0.0.1:8181";          # default
#     tear   = "/Users/me/.local/share/tear/tear.sock";  # default
#   };
#
# Both the bridge AND the daemon must be running for browser
# attach to work. The daemon's HM module is at
# `inputs.tear.homeManagerModules.default` (the substrate
# module-trio output for `programs.tear.daemon`).

{ tearPackages, hmHelpers }:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.tear-ws-bridge;
  system = pkgs.stdenv.hostPlatform.system;
  bridgePackage =
    if cfg.package != null then cfg.package
    else tearPackages.${system}.tear-ws-bridge;
  homeDir = config.home.homeDirectory;
  args = [
    "--listen" cfg.listen
    "--tear" cfg.tear
  ];
in
{
  options.programs.tear-ws-bridge = {
    enable = lib.mkEnableOption "tear-ws-bridge — WebSocket → tear-daemon CBOR bridge for browser / wasm renderers";

    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = ''
        Override the bridge package. `null` (default) pulls the
        bridge from `inputs.tear.packages.<system>.tear-ws-bridge`
        — the canonical workspace build.
      '';
    };

    listen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:8181";
      description = ''
        TCP listen address for the bridge. Use `0.0.0.0:8181` to
        accept browser connections from the LAN; tunnel through
        SSH / WireGuard / a TLS proxy for untrusted networks (the
        wire is unencrypted at this layer).
      '';
    };

    tear = lib.mkOption {
      type = lib.types.str;
      default = "${homeDir}/.local/share/tear/tear.sock";
      description = ''
        Tear-daemon target. Filesystem path → UDS, or
        `tcp://host:port` → TCP (the bridge can front a REMOTE
        daemon, so a browser on one machine can attach to a
        tear-daemon on another).
      '';
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    { home.packages = [ bridgePackage ]; }

    (lib.mkIf pkgs.stdenv.isDarwin (hmHelpers.mkLaunchdService {
      name = "tear-ws-bridge";
      label = "io.pleme.tear-ws-bridge";
      command = "${bridgePackage}/bin/tear-ws-bridge";
      inherit args;
      logDir = "${homeDir}/Library/Logs";
    }))

    (lib.mkIf (!pkgs.stdenv.isDarwin) (hmHelpers.mkSystemdService {
      name = "tear-ws-bridge";
      description = "tear-ws-bridge — WebSocket → tear-daemon CBOR bridge";
      command = "${bridgePackage}/bin/tear-ws-bridge";
      inherit args;
    }))
  ]);
}
