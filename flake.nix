{
  description = "experimint — Fedimint module experiments (AMM)";

  inputs = {
    # Same nixpkgs branch fedimint master pins.
    nixpkgs.url = "github:nixos/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        lib = pkgs.lib;

        # Channel comes from ./rust-toolchain.toml, which mirrors fedimint
        # master's flakebox `toolchain.channel = "stable"`.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # fedimint builds against LLVM 20 (see its flake.nix `toolchainArgs`).
        llvm = pkgs.llvmPackages_20;
      in
      {
        devShells.default = (pkgs.mkShell.override { stdenv = llvm.stdenv; }) {
          packages = [
            rustToolchain
          ]
          ++ (with pkgs; [
            # native build deps of the fedimint dependency tree
            pkg-config
            protobuf # fedimint-server / gateway build.rs (tonic)
            cmake # aws-lc-sys
            perl # openssl-sys / aws-lc-sys
            git # fedimint-build's `git rev-parse` code-version probe

            # gmp-mpfr-sys (rug <- cggmp21 <- fedimint-usdt-server) builds GMP
            # from source with GMP's own autoconf script, which shells out to
            # both of these. Without them `configure` fails with "No usable m4
            # in $PATH" and a missing /usr/bin/file.
            m4
            file
            openssl
            sqlite

            # dev conveniences
            just
            cargo-nextest
          ])
          ++ lib.optionals (!pkgs.stdenv.isDarwin) (
            with pkgs;
            [
              util-linux
              iproute2
            ]
          )
          ++ lib.optionals pkgs.stdenv.isDarwin [ pkgs.libiconv ];

          # bindgen (librocksdb-sys, aws-lc-sys) needs to find libclang.
          LIBCLANG_PATH = "${llvm.libclang.lib}/lib";

          # tonic/prost build scripts must not try to download protoc.
          PROTOC = "${pkgs.protobuf}/bin/protoc";
          PROTOC_INCLUDE = "${pkgs.protobuf}/include";

          # `tikv-jemalloc-sys` (pulled in unconditionally by
          # fedimint-gateway-server, hence by fedimint-testing) runs jemalloc
          # 5.3's autoconf `configure`, whose strerror_r probes compile with
          # `-Werror` (see jemalloc/configure.ac, "strerror_r returns char with
          # gnu source"). nixpkgs' default `fortify` hardening injects
          # -D_FORTIFY_SOURCE, and cc-rs compiles build-script C at -O0, so
          # glibc's features.h fires `#warning _FORTIFY_SOURCE requires
          # compiling with optimization (-O)`. With -Werror that warning fails
          # *both* probes and configure aborts with "cannot determine return
          # type of strerror_r". Dropping the fortify hardening is the fix; it
          # only ever applied to C dependencies built from source anyway.
          hardeningDisable = [
            "fortify"
            "fortify3"
          ];

          # Note: deliberately no `--cfg tokio_unstable` in RUSTFLAGS. fedimint
          # only sets that for its fuzz targets and docs.rs builds, not for
          # normal builds, and setting it here would fork the dependency cache.

          shellHook = ''
            # Another project's direnv may have exported this; inheriting it
            # makes cargo write into that project's target dir and pick up its
            # stale artifacts. Scope us back to ./target.
            if [ -n "''${CARGO_BUILD_TARGET_DIR:-}" ]; then
              echo "note: unsetting inherited CARGO_BUILD_TARGET_DIR=$CARGO_BUILD_TARGET_DIR"
              unset CARGO_BUILD_TARGET_DIR
            fi

            echo "experimint dev shell — $(rustc --version)"
          '';
        };
      }
    );
}
