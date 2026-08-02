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
            openssl # native-tls / reqwest default backend; drop if you use rustls
          ]
          ++ lib.optionals stdenv.isDarwin [ libiconv ];
      in
      {
        devShells.default = pkgs.mkShell {
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
          ];

          # tokio-console requires this cfg across the whole build graph.
          RUSTFLAGS = "--cfg tokio_unstable";

          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          RUST_BACKTRACE = "1";
          OPENSSL_NO_VENDOR = "1";
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

        formatter = pkgs.nixpkgs-fmt;
      }
    );
}
