{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.programs.metastack;
  yamlFormat = pkgs.formats.yaml { };
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

    routingConfig = lib.mkOption {
      type = lib.types.nullOr yamlFormat.type;
      default = null;
      example = lib.literalExpression ''
        {
          version = 2;
          backends.codex = {
            type = "codex";
            url = "ws://127.0.0.1:4107";
          };
          agents.local-codex = {
            backend = "codex";
            cwd = "/path/to/project";
          };
        }
      '';
      description = ''
        Routing config rendered to ~/.config/metastack/routing.yaml. Leave null
        to manage the routing config separately.
      '';
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      home.packages = [ cfg.package ];
    }
    (lib.mkIf (cfg.routingConfig != null) {
      xdg.configFile."metastack/routing.yaml".source =
        yamlFormat.generate "metastack-routing.yaml" cfg.routingConfig;
    })
  ]);
}
