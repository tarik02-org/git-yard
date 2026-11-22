flake:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.git-yard;
  toml = pkgs.formats.toml { };
  settingsFile = toml.generate "git-yard-config.toml" cfg.settings;
  shellIntegration = ''
    gyp() {
      local dir
      dir="$(${lib.getExe cfg.package} pick "$@")" || return $?
      [ -n "$dir" ] || return 1
      builtin cd -- "$dir"
    }
  '';
in
{
  options.programs.git-yard = {
    enable = lib.mkEnableOption "git-yard";
    package = lib.mkOption {
      type = lib.types.package;
      default = flake.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "git-yard.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "git-yard package to install.";
    };
    settings = lib.mkOption {
      type = toml.type;
      default = { };
      example = {
        roots = [ "~/work" ];
        pick.switcher = "git";
      };
      description = "Settings written to git-yard's user config file.";
    };
    enableBashIntegration = lib.mkEnableOption "the gyp picker function for Bash" // {
      default = true;
    };
    enableZshIntegration = lib.mkEnableOption "the gyp picker function for Zsh" // {
      default = true;
    };
    enableFishIntegration = lib.mkEnableOption "the gyp picker function for Fish" // {
      default = true;
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    xdg.configFile."git-yard/config.toml" =
      lib.mkIf (cfg.settings != { } && !pkgs.stdenv.hostPlatform.isDarwin)
        {
          source = settingsFile;
        };
    home.file."Library/Application Support/git-yard/config.toml" =
      lib.mkIf (cfg.settings != { } && pkgs.stdenv.hostPlatform.isDarwin)
        {
          source = settingsFile;
        };
    programs.bash.initExtra = lib.mkIf cfg.enableBashIntegration shellIntegration;
    programs.zsh.initContent = lib.mkIf cfg.enableZshIntegration shellIntegration;
    programs.fish.functions.gyp = lib.mkIf cfg.enableFishIntegration {
      description = "Pick a Git worktree and change directory";
      body = ''
        set -l dir (${lib.getExe cfg.package} pick $argv)
        or return $status
        test (count $dir) -eq 1
        or return 1
        test -n "$dir"
        or return 1
        builtin cd -- "$dir"
      '';
    };
  };
}
