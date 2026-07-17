{
  description = "payjoin-no-std-harness dev shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-utils.url = "github:numtide/flake-utils";

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
      treefmt-nix,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ rust-overlay.overlays.default ];

        pkgs = import nixpkgs {
          inherit system overlays;
        };

        treefmtEval = treefmt-nix.lib.evalModule pkgs ./treefmt.nix;

        embeddedRustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "rustfmt"
            "clippy"
          ];

          targets = [
            "thumbv7em-none-eabihf"
          ];
        };

        embeddedDevShell = pkgs.mkShell {
          name = "embedded-dev";

          packages = with pkgs; [
            embeddedRustToolchain
            gcc-arm-embedded
            pkg-config
            udev

            treefmtEval.config.build.wrapper
          ];

          CC_thumbv7em_none_eabihf = "arm-none-eabi-gcc";

          PKG_CONFIG_PATH = "${pkgs.udev.dev}/lib/pkgconfig";

          shellHook = ''
            echo "Embedded Rust environment"
            echo "Target: thumbv7em-none-eabihf"
          '';
        };
      in
      {
        formatter = treefmtEval.config.build.wrapper;

        checks.formatting = treefmtEval.config.build.check self;

        devShells = {
          default = embeddedDevShell;

          # Alias opcional:
          embedded = embeddedDevShell;
        };
      }
    );
}
