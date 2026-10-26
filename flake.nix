{
  description = "Astesia, a native database workspace built with GPUI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            }
          )
        );

      # Only the files Cargo needs, so editing docs or CI does not trigger a rebuild.
      src = nixpkgs.lib.fileset.toSource {
        root = ./.;
        fileset = nixpkgs.lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./src
          ./icons
          ./packaging
        ];
      };

      # Everything Astesia needs from the system, shared by the package and the dev shell.
      deps =
        pkgs:
        let
          inherit (pkgs) lib stdenv;

          # Pinned by rust-toolchain.toml; the dev shell also gets rust-src for rust-analyzer.
          toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          devToolchain = toolchain.override {
            extensions = [
              "clippy"
              "rustfmt"
              "rust-src"
            ];
          };

          nativeBuildInputs = with pkgs; [
            cmake
            perl
            pkg-config
            rustPlatform.bindgenHook
          ];

          # GPUI loads these with dlopen at runtime, so they also go on LD_LIBRARY_PATH.
          linuxRuntimeLibs = with pkgs; [
            libGL
            libxkbcommon
            vulkan-loader
            wayland
            libx11
            libxcb
            libxcursor
            libxi
            libxrandr
          ];

          buildInputs =
            with pkgs;
            [
              openssl
              sqlite
              zstd
            ]
            ++ lib.optionals stdenv.hostPlatform.isLinux (
              [
                alsa-lib
                dbus
                fontconfig
                freetype
                glib
              ]
              ++ linuxRuntimeLibs
            );
        in
        {
          inherit
            toolchain
            devToolchain
            nativeBuildInputs
            buildInputs
            linuxRuntimeLibs
            ;
        };
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          inherit (pkgs) lib stdenv;
          d = deps pkgs;
          cargoToml = lib.importTOML ./Cargo.toml;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = d.toolchain;
            rustc = d.toolchain;
          };
        in
        {
          default = self.packages.${pkgs.stdenv.hostPlatform.system}.astesia;

          astesia = rustPlatform.buildRustPackage {
            pname = "astesia";
            inherit (cargoToml.package) version;

            inherit src;

            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs =
              d.nativeBuildInputs ++ lib.optionals stdenv.hostPlatform.isLinux [ pkgs.makeWrapper ];
            inherit (d) buildInputs;

            # The UI tests need a display and a GPU, which the Nix sandbox does not have.
            doCheck = false;

            postInstall = lib.optionalString stdenv.hostPlatform.isLinux ''
              install -Dm644 packaging/linux/com.astesia.app.desktop \
                $out/share/applications/com.astesia.app.desktop
              install -Dm644 icons/icon.png \
                $out/share/icons/hicolor/512x512/apps/com.astesia.app.png
            '';

            postFixup = lib.optionalString stdenv.hostPlatform.isLinux ''
              wrapProgram $out/bin/astesia \
                --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath d.linuxRuntimeLibs}
            '';

            meta = {
              inherit (cargoToml.package) description;
              homepage = "https://github.com/zhangchi0104/Astesia";
              mainProgram = "astesia";
              platforms = systems;
            };
          };
        }
      );

      # `nix flake check` builds the app and runs the same Clippy and rustfmt checks as AGENTS.md.
      checks = forAllSystems (
        pkgs:
        let
          d = deps pkgs;
          astesia = self.packages.${pkgs.stdenv.hostPlatform.system}.astesia;
        in
        {
          inherit astesia;

          clippy = astesia.overrideAttrs {
            pname = "astesia-clippy";
            buildPhase = ''
              runHook preBuild
              cargo clippy --locked --all-targets --offline
              runHook postBuild
            '';
            installPhase = "touch $out";
            postInstall = "";
            postFixup = "";
          };

          rustfmt =
            pkgs.runCommand "astesia-rustfmt"
              {
                nativeBuildInputs = [ d.devToolchain ];
              }
              ''
                cd ${src}
                cargo fmt --check
                touch $out
              '';
        }
      );

      devShells = forAllSystems (
        pkgs:
        let
          inherit (pkgs) lib stdenv;
          d = deps pkgs;
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = d.nativeBuildInputs ++ [
              d.devToolchain
              pkgs.rust-analyzer
            ];
            inherit (d) buildInputs;

            env = {
              RUST_SRC_PATH = "${d.devToolchain}/lib/rustlib/src/rust/library";
            }
            // lib.optionalAttrs stdenv.hostPlatform.isLinux {
              LD_LIBRARY_PATH = lib.makeLibraryPath d.linuxRuntimeLibs;
            };
          };
        }
      );

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
