{
  description = "time - minute-by-minute activity tracking";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # Runtime tools the daemon shells out to.
        runtimeDeps = [
          pkgs.grim
          pkgs.typst
        ];

        timePkg = pkgs.rustPlatform.buildRustPackage {
          pname = "time";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          postInstall = ''
            wrapProgram $out/bin/time \
              --prefix PATH : ${pkgs.lib.makeBinPath runtimeDeps}
          '';
        };
      in
      {
        packages.default = timePkg;

        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rustfmt
            pkgs.clippy
            pkgs.rust-analyzer
            pkgs.pkg-config
            pkgs.sqlite
          ] ++ runtimeDeps;

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      }
    );
}
