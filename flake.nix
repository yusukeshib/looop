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
        # fleet runs in-process — no `babysit` binary needed at runtime. `git`
        # (for the memory dir) is wrapped onto PATH; the configured LLM runner
        # (pi/claude) is the user's to provide.
        looop = pkgs.rustPlatform.buildRustPackage {
          pname = "looop";
          version = "0.6.0";
          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          # `git` is shelled out for the memory dir.
          postInstall = ''
            wrapProgram "$out/bin/looop" \
              --prefix PATH : ${pkgs.lib.makeBinPath (with pkgs; [ git ])}
          '';

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
