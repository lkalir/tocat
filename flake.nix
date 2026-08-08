{
  description = "tocat - a featurful relay client/server written in Rust";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        # Primary toolchain: Stable (or from rust-toolchain.toml)
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
            "clippy"
            "rustfmt"
          ];
          targets = [
            "x86_64-unknown-linux-musl"
            "wasm32-unknown-unknown"
          ];
        };

        # Secondary toolchain: Nightly
        rustNightly = pkgs.rust-bin.selectLatestNightlyWith (
          toolchain:
          toolchain.default.override {
            extensions = [ "rust-src" ];
          }
        );

        # Symlink wrapper providing cargo-nightly and rustc-nightly,
        # while ensuring cargo-nightly uses the nightly rustfmt.
        rustNightlyWrapped = pkgs.symlinkJoin {
          name = "rust-nightly-wrapped";
          paths = [ rustNightly ];
          postBuild = ''
            for bin in $out/bin/*; do
              if [ -f "$bin" ] && [ ! -L "$bin-nightly" ]; then
                ln -s "$bin" "$bin-nightly"
              fi
            done

            # Wrap cargo-nightly so it explicitly invokes nightly rustfmt
            wrapProgram $out/bin/cargo-nightly \
              --set RUSTFMT "${rustNightly}/bin/rustfmt"
          '';
          nativeBuildInputs = [ pkgs.makeWrapper ];
        };

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        # Build-time tools that -sys crates shell out to.
        nativeDeps = with pkgs; [
          pkg-config # so openssl-sys et al. can locate libraries
          cmake # pulled in by some transitive -sys crates
        ];

        # C libraries linked into the binary.
        libDeps =
          with pkgs;
          [
            llvmPackages.clang-unwrapped
            llvmPackages.lld
            gnumake
          ]
          ++ lib.optionals stdenv.isDarwin [ libiconv ];
      in
      {
        devShells.default = pkgs.mkShell {
          hardeningDisable = [ "all" ];
          nativeBuildInputs = [
            rustToolchain
            rustNightlyWrapped
          ]
          ++ nativeDeps;
          buildInputs = libDeps;

          packages = with pkgs; [
            # cargo ecosystem
            cargo-nextest
            cargo-watch
            cargo-edit
            cargo-audit
            cargo-deny
            bacon
            mdbook

            # async runtime introspection
            tokio-console

            # poking at sockets
            curl
            websocat
            netcat-gnu
            socat
            tcpdump

            # other utils
            tombi
            pv
            hyperfine
            binaryen
            wasm-tools
          ];

          # tokio-console requires this cfg across the whole build graph.
          RUSTFLAGS = "--cfg tokio_unstable";

          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          RUST_BACKTRACE = "1";

          CLANG = "clang-unwrapped";
          CLANGXX = "clang++-unwrapped";
        };

        packages.default = rustPlatform.buildRustPackage {
          pname = "tocat";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = nativeDeps;
          buildInputs = libDeps;

          doCheck = false;
        };

        packages.wasm-sdk = pkgs.stdenv.mkDerivation {
          pname = "tocat-wasm-sdk";
          version = "0.1.0";
          src = ./sdk/wasm;

          nativeBuildInputs = [ pkgs.cmake ];
          cmakeFlags = [ "-DTOCAT_WASM_BUILD_EXAMPLES=OFF" ];
        };

        check.wasm-examples = pkgs.stdenv.mkDerivation {
          name = "tocat-wasm-examples";
          src = ./sdk/wasm;

          nativeBuildInputs = with pkgs; [
            cmake
            llvmPackages.clang-unwrapped
            lld
          ];

          cmakeFlags = [
            "-DTOCAT_WASM_BUILD_EXAMPLES=ON"
            "-DCMAKE_TOOLCHAIN_FILE=${./sdk/wasm/cmake/wasm32-toolchain.cmake}"
          ];

          installPhase = "mkdir -p $out && cp examples/*.wasm $out/";
        };

        formatter = pkgs.nixpkgs-fmt;

      }

    );
}
