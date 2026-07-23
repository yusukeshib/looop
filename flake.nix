{
  description = "looop — a tiny, portable, Kubernetes-shaped control loop for your work";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # The Rust binary. babysit is linked as a library and the whole worker
        # fleet runs in-process — no `babysit` binary needed at runtime. The
        # configured LLM runner is the user's to provide.
        looop = pkgs.rustPlatform.buildRustPackage {
          pname = "looop";
          version = "0.67.2";
          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;

          # The test suite exercises PTYs/terminal state (portable-pty, crossterm,
          # vt100) which hang or fail inside the Nix build sandbox (no real tty).
          # Run `cargo test` in the devShell / CI instead of during the package build.
          doCheck = false;

          meta = with pkgs.lib; {
            description = "A tiny, portable, Kubernetes-shaped control loop for your work";
            homepage = "https://github.com/yusukeshib/looop";
            license = licenses.mit;
            mainProgram = "looop";
            platforms = platforms.unix;
          };
        };
      in
      {
        packages.default = looop;
        packages.looop = looop;

        apps.default = {
          type = "app";
          program = "${looop}/bin/looop";
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc clippy rustfmt git ];
        };
      });
}
