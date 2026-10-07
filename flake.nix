{
  description = "Rust-native tmux-compatible terminal multiplexer with typed shikumi config";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    crate2nix.url = "github:nix-community/crate2nix";
    flake-utils.url = "github:numtide/flake-utils";
    substrate = {
      url = "github:pleme-io/substrate";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, crate2nix, flake-utils, substrate, ... }:
    let
      baseOutputs = (import "${substrate}/lib/rust-workspace-release-flake.nix" {
        inherit nixpkgs crate2nix flake-utils;
      }) {
        toolName = "tear";
        # The Cargo workspace MEMBER crate, which is published as
        # `pleme-tear` (the bare `tear` is owned by an unrelated crate on
        # crates.io — see tear/Cargo.toml). Everything operator-facing
        # stays `tear`: `packageAttr` below keeps `nix run .#tear`, and
        # `binaryName` keeps the installed binary named `tear`.
        packageName = "pleme-tear";
        src = self;
        repo = "pleme-io/tear";

        # Substrate module-trio — emit homeManagerModules.default /
        # nixosModules.default / darwinModules.default from one spec.
        #
        # Today's surface:
        #   * `services.tear` (system enable + package on NixOS/Darwin)
        #   * `programs.tear.daemon = { enable, extraArgs, environment }`
        #     — user-level launchd agent on Darwin, systemd user unit on
        #     Linux. The daemon binds at `$XDG_RUNTIME_DIR/tear.sock`
        #     (Linux) or `~/.local/share/tear/tear.sock` (Darwin), which
        #     is exactly where mado's `tear_discovery::resolve_socket_path`
        #     looks first — so a fleet with the service enabled gets
        #     persistent sessions for free, and mado attaches without any
        #     extra config.
        #   * `services.tear.settings = { ... }` — typed shikumi YAML
        #     written to `~/.config/tear/tear.yaml` on activation.
        module = {
          description = "tear — Rust-native tmux-compatible multiplexer";
          packageAttr = "tear";
          binaryName = "tear";
          withUserDaemon = true;
          daemonWorkloadClass = "session-host";
          withShikumiConfig = true;
          shikumiDefaults = {};
          extraHmConfigFn = { cfg, lib, config, pkgs, ... }:
            let
              settings = config.services.tear.settings or { };
              held = ((settings.sessions or { }).durability or "process_bound") == "held";
              daemon = cfg.daemon.enable or false;
            in
            lib.mkMerge [
              (lib.mkIf (held && daemon && pkgs.stdenv.hostPlatform.isDarwin) {
                launchd.agents.tear-daemon.config.AbandonProcessGroup = true;
              })
              (lib.mkIf (held && daemon && !pkgs.stdenv.hostPlatform.isDarwin) {
                systemd.user.services.tear-daemon.Service.KillMode = "process";
              })
            ];
        };
      };

      # ── tear-ws-bridge sidecar module ─────────────────────────
      # Hand-written rather than a second module-trio invocation
      # because workspace-release-flake emits one trio per spec.
      # The bridge is its own opt-in surface — operators enable it
      # only when browser / wasm renderer attach is needed.
      hmHelpers = import "${substrate}/lib/hm/service-helpers.nix" {
        lib = nixpkgs.lib;
      };
      wsBridgeModule = import ./tear-ws-bridge/module {
        tearPackages = baseOutputs.packages;
        inherit hmHelpers;
      };

      benchApp = system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          workspace = substrate.lib.${system}.mkRustWorkspace { name = "tear"; src = self; };
          tearbench = pkgs.runCommand "tearbench" { nativeBuildInputs = [ pkgs.makeBinaryWrapper ]; } ''
            makeBinaryWrapper ${workspace.binaryOf "tear-bench"}/bin/tearbench $out/bin/tearbench --add-flags "--tear-bin ${baseOutputs.packages.${system}.tear}/bin/tear"
          '';
        in {
          type = "app";
          program = "${tearbench}/bin/tearbench";
        };
    in
      baseOutputs // {
        apps = builtins.mapAttrs (system: apps: apps // { bench = benchApp system; }) baseOutputs.apps;
        homeManagerModules = (baseOutputs.homeManagerModules or {}) // {
          ws-bridge = wsBridgeModule;
        };
      };
}
