{
  inputs = {
    nixpkgs.url = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      inputs.nixpkgs.follows = "nixpkgs";
      url = "github:nix-community/fenix";
    };
    rust-manifest = {
      flake = false;
      url = "https://static.rust-lang.org/dist/2024-05-08/channel-rust-nightly.toml";
    };
  };

  outputs = { self, nixpkgs, flake-utils, fenix, ... } @ inputs:
    let
      g = pkgs: let
        rust-toolchain = pkgs.fenix.fromManifestFile inputs.rust-manifest;
        rust = pkgs.fenix.combine (with rust-toolchain; [
          rustc cargo rust-src rustfmt clippy
        ]);
        rustPlatform = (pkgs.makeRustPlatform {
          cargo = rust-toolchain.cargo;
          rustc = rust-toolchain.rustc // {
            inherit (pkgs.rustc) targetPlatforms badTargetPlatforms;
          };
        });

        inherit (rustPlatform) buildRustPackage bindgenHook;

        libraries = with pkgs;[
          (enableDebugging mesa)
          libffi
          pixman
          xorg.libxcb
          xorg.libX11
          libGL
          pipewire
        ];

        packages = with pkgs; [
          pkg-config
        ];
      in {
        devShell = pkgs.mkShell {
          buildInputs = libraries;
          nativeBuildInputs = [ rust ] ++ packages;
          LIBCLANG_PATH = pkgs.lib.makeLibraryPath [ pkgs.llvmPackages_latest.libclang.lib ];

          shellHook =
            ''
            '';
        };
        packages.default = buildRustPackage {
          name = "xdg-desktop-portal-picom";
          cargoLock.lockFile = ./Cargo.lock;
          src = ./.;
          nativeBuildInputs = packages;
          buildInputs = libraries ++ [ bindgenHook ];
          LIBCLANG_PATH = pkgs.lib.makeLibraryPath [ pkgs.llvmPackages_latest.libclang.lib ];

          postInstall = let
            dbus-service = pkgs.lib.generators.toINI {} {
              "D-BUS Service" = {
                Name = "org.freedesktop.impl.portal.desktop.picom";
                Exec = "@outpath@/bin/xdg-desktop-portal-picom";
                SystemdService = "xdg-desktop-portal-picom.service";
              };
            };
            systemd-service = pkgs.lib.generators.toINI {} {
              Unit = {
                Description = "xdg-desktop-portal ScreenCast based on picom";
                PartOf = "graphical-session.target";
              };
              Service = {
                BusName = "org.freedesktop.impl.portal.desktop.picom";
                ExecStart = "@outpath@/bin/xdg-desktop-portal-picom";
                Type = "dbus";
                Slice = "session.slice";
              };
            };
          in ''
            install -Dm644 $src/data/picom.portal $out/share/xdg-desktop-portal/portals/picom.portal
            mkdir -p $out/share/dbus-1/services
            mkdir -p $out/share/systemd/user

            cat >>$out/share/dbus-1/services/org.freedesktop.impl.portal.desktop.picom.service <<EOF
${dbus-service}
EOF
            cat >>$out/share/systemd/user/xdg-desktop-portal-picom.service <<EOF
${systemd-service}
EOF
            substituteInPlace $out/share/dbus-1/services/org.freedesktop.impl.portal.desktop.picom.service \
              --replace "@outpath@" $out
            substituteInPlace $out/share/systemd/user/xdg-desktop-portal-picom.service \
              --replace "@outpath@" $out
          '';
        };
      };
    in
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system}.extend fenix.overlays.default;
      in (g pkgs)) // {
      overlays.default = final: prev: {
        xdg-desktop-portal-picom = (g (final.extend fenix.overlays.default)).packages.default;
      };
    };
}
