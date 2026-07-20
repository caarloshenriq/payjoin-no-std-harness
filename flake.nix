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

        # Same toolchain source for both shells: rust-toolchain.toml pins
        # nightly (needed for `cargo fmt`'s unstable options and for
        # `-Z direct-minimal-versions`/`-Z minimal-versions` in
        # contrib/update-lock-files.sh). There is no rustup inside this
        # shell, so `cargo +nightly ...` will NOT work here -- the `cargo`
        # on PATH already *is* nightly, so drop the `+nightly` prefix from
        # any command run inside `nix develop`.
        hostRustToolchain = (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
          extensions = [
            "rust-src"
            "rustfmt"
            "clippy"
          ];
        };

        embeddedRustToolchain = (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
          extensions = [
            "rust-src"
            "rustfmt"
            "clippy"
          ];
          targets = [ "thumbv7em-none-eabihf" ];
        };

        defaultDevShell = pkgs.mkShell {
          name = "harness-dev";
          packages = with pkgs; [
            hostRustToolchain
            pkg-config
            udev # provides libudev.pc, needed by harness-host's serialport dependency
            treefmtEval.config.build.wrapper
          ];
          PKG_CONFIG_PATH = "${pkgs.udev.dev}/lib/pkgconfig";
        };

        embeddedDevShell = pkgs.mkShell {
          name = "embedded-dev";
          packages = with pkgs; [
            embeddedRustToolchain
            gcc-arm-embedded
            dfu-util
            treefmtEval.config.build.wrapper
          ];
          CC_thumbv7em_none_eabihf = "arm-none-eabi-gcc";
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
          default = defaultDevShell;
          embedded = embeddedDevShell;
        };
      }
    );
}
