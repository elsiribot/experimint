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

        # The package build has to use the same toolchain and the same stdenv
        # as the dev shell, or it is not building the thing that was tested.
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
          stdenv = llvm.stdenv;
        };

        # `fedimint-build`'s `build.rs` probe shells out to `git rev-parse
        # HEAD`. A Nix build sees no git metadata at all — neither our source
        # (flakes hand over a bare tree) nor the vendored fedimint crates — so
        # the probe would fall back to 40 zeros and print a warning.
        #
        # Forcing the value is not cosmetic. `fedimintd::run` asserts that the
        # hash it is handed is the *same length* as the one baked into the
        # `fedimintd` crate, and both come from this one variable, so it has to
        # be set for the whole build or not at all.
        codeVersion = self.rev or self.dirtyRev or "0000000000000000000000000000000000000000";

        # The three binaries this repo publishes differ only in which package
        # they build and what they call themselves. Everything else — the
        # toolchain, the vendored lockfile, every native build input and every
        # hardening workaround below — is a property of the *dependency tree*,
        # which they share in full. Factoring it out is what stops the daemon
        # from being built one way and the tools another.
        mkExperimintPackage =
          { pname, description }:
          rustPlatform.buildRustPackage {
            inherit pname;
            version = "0.1.0";

            src = ./.;

            # `cargoHash` vendors from the *committed* `Cargo.lock` rather than
            # re-resolving. That is load-bearing here, not just reproducibility
            # hygiene: a fresh resolve picks `bdk_electrum 0.23.2`, whose
            # `electrum-client 0.24.1` cannot unify with the `0.23.1` that
            # `fedimint-ldk-node` depends on directly, and the tree stops
            # compiling. See the note in `Cargo.toml`.
            #
            # Every `fedimint-*` crate is a git dependency on one revision of
            # elsiribot/fedimint, so this hash also pins that checkout.
            #
            # It is shared across all three packages because it hashes the
            # vendored *workspace* lockfile, not the selected package. Adding a
            # workspace member changes it for every one of them at once.
            cargoHash = "sha256-1WJ+m/wS8mit8fmxwuZulNcJySblfYZfARnWz5ezjvs=";

            # The workspace also contains the `*-tests` crates, which drag in
            # `fedimint-testing` -> the gateway -> `fedimint-ldk-node`. None of
            # that is needed to produce a binary.
            cargoBuildFlags = [
              "--package"
              pname
            ];

            nativeBuildInputs = with pkgs; [
              pkg-config
              protobuf # fedimint-server build.rs (tonic)
              cmake # aws-lc-sys
              perl # openssl-sys / aws-lc-sys
              git # fedimint-build's code-version probe, if ever unforced

              # gmp-mpfr-sys (rug <- cggmp21 <- fedimint-usdt-server) builds GMP
              # from source with GMP's own autoconf script, which shells out to
              # both of these. Without them `configure` fails with "No usable m4
              # in $PATH" and a missing /usr/bin/file.
              m4
              file
            ];

            buildInputs = with pkgs; [
              openssl
              sqlite
            ];

            # `cmake` is here for `aws-lc-sys`' own build script to call. The
            # workspace root is a cargo project, not a CMake one, so the setup
            # hook must not try to configure it.
            dontUseCmakeConfigure = true;

            # bindgen (librocksdb-sys, aws-lc-sys) needs to find libclang.
            env = {
              LIBCLANG_PATH = "${llvm.libclang.lib}/lib";
              PROTOC = "${pkgs.protobuf}/bin/protoc";
              PROTOC_INCLUDE = "${pkgs.protobuf}/include";
              FEDIMINT_BUILD_FORCE_GIT_HASH = codeVersion;
            };

            # `tikv-jemalloc-sys` runs jemalloc 5.3's autoconf `configure`, whose
            # strerror_r probes compile with `-Werror`. nixpkgs' default `fortify`
            # hardening injects -D_FORTIFY_SOURCE, and cc-rs compiles build-script
            # C at -O0, so glibc's features.h fires `#warning _FORTIFY_SOURCE
            # requires compiling with optimization (-O)`; with -Werror that fails
            # both probes and configure aborts with "cannot determine return type
            # of strerror_r".
            hardeningDisable = [
              "fortify"
              "fortify3"
            ];

            # The test lane is `cargo test --workspace` in the dev shell, which is
            # where `anvil` and `bitcoind` live. Running it here would build the
            # `*-tests` crates this package deliberately does not need, and the
            # EVM suites would skip silently without Foundry anyway.
            doCheck = false;

            meta = {
              inherit description;
              homepage = "https://github.com/elsirion/experimint";
              license = lib.licenses.mit;
              mainProgram = pname;
              platforms = lib.platforms.linux ++ lib.platforms.darwin;
            };
          };
      in
      {
        packages.default = self.packages.${system}.fedimintd-experimint;

        packages.fedimintd-experimint = mkExperimintPackage {
          pname = "fedimintd-experimint";
          description = "fedimintd carrying the experimint module set (v2 core modules + meta + amm + usdt)";
        };

        # The only client that can drive `amm` and `usdt`. Packaged so a host
        # can join and fund a wallet without a copy of the repo and a rust
        # toolchain on it — the price keeper below needs a data dir that is
        # already joined, and nothing else can produce one.
        packages.fedimint-cli-experimint = mkExperimintPackage {
          pname = "fedimint-cli-experimint";
          description = "fedimint-cli built with the experimint client module set";
        };

        packages.amm-price-keeper = mkExperimintPackage {
          pname = "amm-price-keeper";
          description = "Holds the experimint AMM's BTC/USDt pool near an external reference price";
        };

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
