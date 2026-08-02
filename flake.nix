{
  description = "time - minute-by-minute activity tracking";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      ...
    }:
    {
      homeManagerModules.default = import ./nix/hm-module.nix { inherit self; };
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
          config.android_sdk.accept_license = true;
        };

        # Enough SDK to build the APK and run it in an emulator, so the Android
        # client can be tested here rather than only on a real phone.
        android = pkgs.androidenv.composeAndroidPackages {
          platformVersions = [ "35" ];
          buildToolsVersions = [ "35.0.0" ];
          includeEmulator = true;
          includeSystemImages = true;
          systemImageTypes = [ "default" ];
          abiVersions = [ "x86_64" ];
        };

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
            pkgs.jdk17
            pkgs.gradle
            android.androidsdk
          ] ++ runtimeDeps;

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

          ANDROID_HOME = "${android.androidsdk}/libexec/android-sdk";
          ANDROID_SDK_ROOT = "${android.androidsdk}/libexec/android-sdk";
          JAVA_HOME = "${pkgs.jdk17}";
          # AGP needs to find aapt2 from the Nix store rather than the copy it
          # would otherwise try to download into a read-only SDK.
          GRADLE_OPTS =
            "-Dorg.gradle.project.android.aapt2FromMavenOverride="
            + "${android.androidsdk}/libexec/android-sdk/build-tools/35.0.0/aapt2";
        };
      }
    );
}
