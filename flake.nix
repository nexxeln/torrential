{
  description = "torrential development shell";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "aarch64-darwin" "x86_64-darwin" "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in {
      # Static libraries, so the binary loads no OpenSSL from the Nix store.
      # pkgsStatic.openssl would be musl on Linux, which a glibc link can't use.
      devShells = forAll (pkgs: let openssl = pkgs.openssl.override { static = true; }; in {
        default = pkgs.mkShellNoCC {
          # Linux needs a compiler/linker using the same glibc as static OpenSSL.
          packages = [ pkgs.cmake pkgs.ninja pkgs.pkg-config pkgs.boost.dev openssl.dev openssl.out pkgs.bun ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.stdenv.cc ];
          # Cargo links C++ through rustc, so expose the matching runtime as well.
          LD_LIBRARY_PATH = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux
            (pkgs.lib.makeLibraryPath [ pkgs.stdenv.cc.cc.lib ]);
          BOOST_ROOT = "${pkgs.boost.dev}";
          OPENSSL_ROOT_DIR = "${openssl.dev}";
          OPENSSL_LIB_DIR = "${openssl.out}/lib";
          OPENSSL_INCLUDE_DIR = "${openssl.dev}/include";
        };
      });
    };
}
