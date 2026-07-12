{
  description = "Rust dev shell with rust-src via oxalica rust-overlay";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane }:
    let
      perSystem = flake-utils.lib.eachDefaultSystem (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };

          craneLib = crane.mkLib pkgs;
          src = pkgs.lib.sourceFilesBySuffices ./. [".rs" ".toml" ".lock" ".css"];

          commonArgs = {
            inherit src;
            strictDeps = true;
            # cargoLock = {
            #   # allowBuiltinFetchGit = true;
            #   # lockFile = ./Cargo.lock;
            # };
            buildInputs = [
              pkgs.gtk4
              pkgs.gtk4-layer-shell
              pkgs.pipewire
            ];
            nativeBuildInputs = [
              pkgs.clang # for bindgen, pipewire needs this
              pkgs.pkg-config
            ];
            LIBCLANG_PATH = "${pkgs.clang.cc.lib}/lib";
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          gtk-status-bar = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;

              # Tray icons like fcitx's `input-keyboard-symbolic` and KDE Connect's
              # `kdeconnectindicatordark` ship only as SVG. GTK renders themed SVGs
              # through the gdk-pixbuf librsvg loader, which the bare Cargo binary
              # has no way to find at runtime. wrapGAppsHook4 builds a loaders.cache
              # (via GDK_PIXBUF_MODULE_FILE) and wires up XDG_DATA_DIRS/schemas, so
              # SVG-only icons stop falling back to the "image-missing" glyph.
              nativeBuildInputs = commonArgs.nativeBuildInputs ++ [
                pkgs.wrapGAppsHook4
              ];
              buildInputs = commonArgs.buildInputs ++ [
                pkgs.librsvg
                pkgs.gdk-pixbuf
                pkgs.glib
                pkgs.gsettings-desktop-schemas
              ];
            }
          );


          # Pull the latest stable toolchain and add components.
          toolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "rustfmt" "clippy" ];
          };

          # Path to the standard-library sources (picked up automatically by rust-analyzer).
          rustLibSrc = "${toolchain}/lib/rustlib/src/rust/library";
        in {
          # `nix build` will produce the Rust toolchain derivation.
          packages = {
            toolchain      = toolchain;
            default        = gtk-status-bar;
            gtk-status-bar = gtk-status-bar;
          };

          # `nix develop` drops you into a shell with Rust + rust-src.
          #
          # The released binary is wrapped by wrapGAppsHook4, which points
          # GDK_PIXBUF_MODULE_FILE at a loaders cache that includes the librsvg
          # SVG decoder. In the dev shell the librsvg setup hook does the same
          # job: nixpkgs ships librsvg with a merged loaders.cache (all
          # gdk-pixbuf loaders + SVG) and the hook exports it, so `cargo run`
          # decodes SVG tray icons exactly like `nix run`.
          devShells.default = pkgs.mkShell {
            name = "rust-dev-shell";
            # Include Rust toolchain and required C libraries for GTK4
            nativeBuildInputs = [
              toolchain
              # pkg-config is needed to discover C library .pc files
              pkgs.pkg-config
              # GTK4 and its dependencies (glib, gdk-pixbuf, pango, cairo, etc.)
              pkgs.gtk4
              # Layer shell library for GTK4 surfaces
              pkgs.gtk4-layer-shell
              # PipeWire development libraries
              pkgs.pipewire
              # Clang for bindgen (required for PipeWire Rust bindings)
              pkgs.clang
              # SVG gdk-pixbuf loader for tray icons; see the comment above.
              pkgs.librsvg
            ];

            env = {
              # Explicit for completeness; rust-analyzer finds it even without this.
              RUST_SRC_PATH = rustLibSrc;
              # For bindgen (PipeWire Rust bindings).
              LIBCLANG_PATH = "${pkgs.clang.cc.lib}/lib";
            };
          };

          apps.default = flake-utils.lib.mkApp {
            drv = gtk-status-bar;
          };
        });

      # home-manager module: installs the user service but does NOT enable it.
      # Trigger is Hyprland's exec-once -> `systemctl --user start gtk-status-bar.service`,
      # so the bar only runs when Hyprland is up (and its env vars are imported into
      # the systemd user environment first; see README for the exec-once snippet).
      homeManagerModule = { config, lib, pkgs, ... }:
        let
          cfg = config.programs.gtk-status-bar;
        in {
          options.programs.gtk-status-bar = {
            enable = lib.mkEnableOption "GTK status bar (Hyprland-triggered, systemd-supervised)";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.system}.gtk-status-bar;
              description = "The gtk-status-bar package to install and run.";
            };

            logLevel = lib.mkOption {
              type = lib.types.str;
              default = "info";
              example = "debug";
              description = "RUST_LOG filter passed to the service via Environment=.";
            };
          };

          config = lib.mkIf cfg.enable {
            home.packages = [ cfg.package ];

            # Unit defined but Install.WantedBy intentionally omitted so it does not
            # auto-start with graphical-session.target. PartOf still binds shutdown:
            # when the graphical session ends, the bar is torn down.
            systemd.user.services.gtk-status-bar = {
              Unit = {
                Description = "GTK status bar for Hyprland";
                PartOf = [ "graphical-session.target" ];
                After = [ "graphical-session.target" ];
              };
              Service = {
                ExecStart = "${cfg.package}/bin/gtk-status-bar";
                Environment = [ "RUST_LOG=${cfg.logLevel}" ];
                Restart = "on-failure";
                RestartSec = 2;
              };
            };
          };
        };
    in
      perSystem // {
        # home-manager exposes modules via either name depending on version; expose both.
        homeManagerModules.default = homeManagerModule;
        homeModules.default = homeManagerModule;
      };
}
