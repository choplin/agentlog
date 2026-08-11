{
  description = "Agentlog — local browser for AI coding-agent session history";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0";
    flake-parts.url = "github:hercules-ci/flake-parts";
    devshell = {
      url = "github:numtide/devshell";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      nixpkgs,
      flake-parts,
      devshell,
      fenix,
      ...
    }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];
    in
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ ];

      flake = { };

      systems = supportedSystems;

      perSystem =
        {
          config,
          self',
          inputs',
          pkgs,
          system,
          ...
        }:
        let
          rustToolchain = fenix.packages.${system}.stable.withComponents [
            "cargo"
            "clippy"
            "rust-analyzer"
            "rust-src"
            "rustc"
            "rustfmt"
          ];
          packagePkgs = if pkgs.stdenv.hostPlatform.isLinux then pkgs.pkgsStatic else pkgs;
          agentlog = packagePkgs.rustPlatform.buildRustPackage {
            pname = "agentlog";
            version = "0.1.0";
            src = ./.;

            cargoLock.lockFile = ./Cargo.lock;
            doCheck = false;

            meta = {
              description = "A local browser for AI coding-agent session history";
              homepage = "https://github.com/choplin/agentlog";
              license = pkgs.lib.licenses.mit;
              mainProgram = "agentlog";
              platforms = supportedSystems;
            };
          };
        in
        {
          _module.args.pkgs = import nixpkgs {
            inherit system;
            overlays = [ devshell.overlays.default ];
          };

          devShells.default = pkgs.devshell.mkShell {
            imports = [ (pkgs.devshell.importTOML ./devshell.toml) ];
            packages = [
              pkgs.cargo-release
              rustToolchain
            ];
          };

          packages = {
            inherit agentlog;
            default = agentlog;
          };

          apps.default = {
            type = "app";
            program = "${agentlog}/bin/agentlog";
            meta.description = "Browse local AI coding-agent session history";
          };

          checks.agentlog = agentlog;
        };
    };
}
