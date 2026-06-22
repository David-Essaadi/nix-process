{
  description = "Example services for nix-process";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in
    {
      # `nix-process up` (run from this directory) evaluates this attribute.
      #
      # Commands here use bare tool names resolved from PATH, so the demo is
      # turnkey inside `nix develop`. In a real project you would interpolate
      # store paths, e.g. command = "${pkgs.postgresql}/bin/postgres ...".
      processes = {
        ticker = {
          command = "while true; do echo tick $(date +%T); sleep 1; done";
        };

        web = {
          command = "python3 -m http.server 8080";
          depends_on = [ "ticker" ];
          health_check = {
            tcp_port = 8080;
            timeout_seconds = 15;
          };
        };

        worker = {
          command = "while true; do echo 'worker doing work'; sleep 2; done";
          depends_on = [ "web" ];
          # A command probe: ready once the web port answers.
          health_check = {
            command = "python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", 8080), 2).close()'";
            timeout_seconds = 15;
          };
        };
      };

      # Same as `processes` but adds a process that ignores SIGTERM, to exercise
      # the SIGKILL escalation path:  nix-process up --attr stubbornDemo
      stubbornDemo = self.processes // {
        stubborn = {
          command = "trap '' TERM; echo 'stubborn: ignoring SIGTERM'; while true; do sleep 1; done";
        };
      };

      devShells.x86_64-linux.default = pkgs.mkShell {
        packages = [ pkgs.python3 pkgs.bash pkgs.coreutils ];
      };
    };
}
