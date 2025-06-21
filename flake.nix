{
  description = "Rust dev shell with rust-src via oxalica rust-overlay";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        # Pull the latest stable toolchain and add components.
        toolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rustfmt" "clippy" ];
        };

        # Path to the standard-library sources (picked up automatically by rust-analyzer).
        rustLibSrc = "${toolchain}/lib/rustlib/src/rust/library";
      in {
        # `nix build` will produce the Rust toolchain derivation.
        packages.default = toolchain;

        # `nix develop` drops you into a shell with Rust + rust-src.
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
          ];

          # Explicit for completeness; rust-analyzer finds it even without this.
          RUST_SRC_PATH = rustLibSrc;
          
        };
      });
}
