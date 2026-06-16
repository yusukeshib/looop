{
  description = "loop — a tiny, portable, Kubernetes-shaped control loop for your work";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        loop = pkgs.stdenvNoCC.mkDerivation {
          pname = "loop";
          version = "0.2.0";
          src = ./.;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          # Runtime deps the script shells out to.
          runtimeDeps = with pkgs; [ bash coreutils git jq ];

          installPhase = ''
            runHook preInstall
            install -Dm755 loop "$out/bin/loop"
            wrapProgram "$out/bin/loop" \
              --prefix PATH : ${pkgs.lib.makeBinPath (with pkgs; [ bash coreutils git jq ])}
            runHook postInstall
          '';

          meta = with pkgs.lib; {
            description = "A tiny, portable, Kubernetes-shaped control loop for your work";
            homepage = "https://github.com/yusukeshib/loop";
            license = licenses.mit;
            mainProgram = "loop";
            platforms = platforms.unix;
          };
        };
      in
      {
        packages.default = loop;
        packages.loop = loop;

        apps.default = {
          type = "app";
          program = "${loop}/bin/loop";
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [ bash coreutils git jq shellcheck shfmt ];
        };
      });
}
