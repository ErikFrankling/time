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
      homeManagerModules.agent-v2 = import ./nix/hm-module-v2.nix { inherit self; };
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
          platformVersions = [
            "35"
            "36"
          ];
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
          src = ./legacy;
          cargoLock.lockFile = ./legacy/Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          postInstall = ''
            wrapProgram $out/bin/time \
              --prefix PATH : ${pkgs.lib.makeBinPath runtimeDeps}
          '';
        };
        # v2: one workspace, built once. Only the files the Rust build reads,
        # so editing the Android app or the docs does not rebuild it.
        v2 = pkgs.rustPlatform.buildRustPackage {
          pname = "time-v2";
          version = "2.0.0-dev";
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./crates
              ./config
              ./fixtures
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.makeWrapper ];
          # The agent asks MPRIS whether anything is playing via playerctl.
          postInstall = ''
            wrapProgram $out/bin/time-agent \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.playerctl ]}
          '';
          meta.mainProgram = "time-server";
        };
      in
      {
        # v1 stays the default until v2 goes live (M4); the dotfiles use it.
        packages.default = timePkg;
        packages.time-v2 = v2;

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
            # keytool needs a source of randomness and gh needs to be present
            # for scripts/setup-signing.sh to run entirely inside the shell.
            pkgs.openssl
            pkgs.gh
            pkgs.gradle
            android.androidsdk
          ]
          ++ runtimeDeps;

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
