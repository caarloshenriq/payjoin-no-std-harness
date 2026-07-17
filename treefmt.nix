{ pkgs, ... }:

{
  projectRootFile = "flake.nix";

  programs = {
    nixfmt.enable = true;
    ruff-format.enable = true;

    rustfmt = {
      enable = true;
      package = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.default);
      edition = "2021";
    };

    shellcheck.enable = true;

    shfmt = {
      enable = true;
      indent_size = 4;
    };

    taplo.enable = true;
  };

  settings.formatter = {
    rustfmt.options = [
      "--config-path"
      "./rustfmt.toml"
    ];

    shellcheck = {
      includes = [ "*.sh" ];
      excludes = [ "*.envrc" ];
    };

    shfmt = {
      includes = [ "*.sh" ];
      excludes = [ "*.envrc" ];
      options = [ "--case-indent" ];
    };
  };
}
