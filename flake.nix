{
  inputs = {
    nixpkgs.url = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      inputs.nixpkgs.follows = "nixpkgs";
      url = github:nix-community/fenix;
    };
    rust-manifest = {
      flake = false;
      url = "https://static.rust-lang.org/dist/2024-05-08/channel-rust-nightly.toml";
    };
  };

  outputs = { self, nixpkgs, flake-utils, fenix, ... } @ inputs:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        fenix' = pkgs.callPackage "${fenix}/." {};
        rust-toolchain = fenix'.fromManifestFile inputs.rust-manifest;
        rust = fenix'.combine (with rust-toolchain; [
          rustc cargo rust-src rustfmt clippy
        ]);

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
      in
      {
        devShell = pkgs.mkShell {
          buildInputs = libraries;
          nativeBuildInputs = [ rust ] ++ packages;
          LIBCLANG_PATH = pkgs.lib.makeLibraryPath [ pkgs.llvmPackages_latest.libclang.lib ];

          shellHook =
            ''
            '';
        };
      });
}
