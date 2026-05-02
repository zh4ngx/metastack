{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.programs.metastack;
in
{
  options.programs.metastack = {
    enable = lib.mkEnableOption "metastack";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText =
        lib.literalExpression "inputs.metastack.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "The metastack package to install.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
  };
}
