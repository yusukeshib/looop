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

        looop = pkgs.stdenvNoCC.mkDerivation {
          pname = "looop";
          version = "0.7.2";
          src = ./.;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          # Runtime deps the script shells out to.
          runtimeDeps = with pkgs; [ bash coreutils git jq ];

          installPhase = ''
            runHook preInstall
            install -Dm755 looop "$out/bin/looop"
            wrapProgram "$out/bin/looop" \
              --prefix PATH : ${pkgs.lib.makeBinPath (with pkgs; [ bash coreutils git jq ])}
            runHook postInstall
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
          packages = with pkgs; [ bash coreutils git jq shellcheck shfmt ];
        };
      });
}
