{
  description = "nix-process — a TUI-less process manager for services defined in a flake.nix";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f {
        inherit system;
        pkgs = nixpkgs.legacyPackages.${system};
      });
    in
    {
      packages = forAllSystems ({ pkgs, ... }: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "nix-process";
          version = "0.1.0";
          # Exclude build artifacts (target/, result, local state) so consuming
          # this as a `path:` flake doesn't copy 100s of MB into the store.
          src = nixpkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: _type:
              let base = baseNameOf path;
              in base != "target" && base != "result" && base != ".nix-process";
          };
          cargoLock.lockFile = ./Cargo.lock;
          # nix-process shells out to `nix` at runtime; make sure it's on PATH.
          nativeBuildInputs = [ pkgs.makeWrapper ];
          postInstall = ''
            wrapProgram $out/bin/nix-process \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.nix ]}
          '';
          meta = {
            description = "Run and supervise processes defined in a flake.nix, no TUI";
            mainProgram = "nix-process";
          };
        };
      });

      apps = forAllSystems ({ system, ... }:
        let bin = "${self.packages.${system}.default}/bin/nix-process"; in
        {
          default = self.apps.${system}.up;
          up = { type = "app"; program = bin; };
        });

      devShells = forAllSystems ({ pkgs, ... }: {
        default = pkgs.mkShell {
          packages = [ pkgs.cargo pkgs.rustc pkgs.rust-analyzer pkgs.clippy pkgs.nix ];
        };
      });

      formatter = forAllSystems ({ pkgs, ... }: pkgs.nixpkgs-fmt);
    };
}
