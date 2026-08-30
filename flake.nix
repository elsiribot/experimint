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

            # `anvil`, for the usdt module's EVM end-to-end suites. Without it
            # `spawn_anvil` returns `Ok(None)` and every anvil-gated test
            # SKIPS — silently, and indistinguishably from passing. Those
            # suites are the only coverage of the real ERC-4337 UserOp path,
            # withdrawal batching against a live chain, reorg handling and
            # residual recovery, so a dev shell without Foundry makes a green
            # `cargo test --workspace` mean much less than it appears to.
            foundry

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

          # `cargo check --target wasm32-unknown-unknown` (the WASM-safety gate
          # on the `*-common`/`*-client` crates) has to compile one C
          # dependency from source: `secp256k1-sys`, which vendors libsecp256k1
          # along with its own `wasm/wasm-sysroot`.
          #
          # nixpkgs' cc-wrapper is single-target by construction — it warns
          # "supplying the --target wasm32-unknown-unknown != <host> argument
          # to a nix-wrapped compiler may not work correctly" and then proves
          # it twice: it injects `-fzero-call-used-regs=used-gpr`, which clang
          # rejects outright for wasm32, and it puts the host glibc headers on
          # the include path, so the build dies on a missing `gnu/stubs-32.h`
          # while looking for 32-bit glibc that has nothing to do with wasm.
          #
          # Pointing this target's CC at the *unwrapped* clang sidesteps both:
          # no injected hardening flags, no host libc include paths. The vendored
          # `wasm/wasm-sysroot` supplies the headers instead, which is exactly
          # what `secp256k1-sys` intends for this target.
          #
          # (The platform branch solves the same problem differently, with a
          # whole separate flakebox `toolchainWasm` cross shell. This is the
          # small version of that: one env var, same effect for our one C dep.)
          # Unwrapped clang does not put its own resource-dir headers
          # (`stddef.h` and friends, which the compiler supplies rather than
          # libc) on the include path the way the wrapper does, so they are
          # added back explicitly — and only those, not the host libc.
          CC_wasm32_unknown_unknown = "${llvm.clang-unwrapped}/bin/clang";
          CFLAGS_wasm32_unknown_unknown =
            "-isystem ${llvm.clang-unwrapped.lib}/lib/clang/"
            + "${lib.versions.major llvm.clang-unwrapped.version}/include";

          # `getrandom` 0.3 refuses to build for wasm32-unknown-unknown unless
          # told which backend to use — the browser `crypto.getRandomValues`
          # one, here. Copied from the platform branch's
          # `nix/flakebox.nix:253`, which sets the identical value.
          CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS = ''--cfg getrandom_backend="wasm_js"'';

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
