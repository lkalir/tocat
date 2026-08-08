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
      self,
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

        version = (pkgs.lib.importTOML ./Cargo.toml).workspace.package.version;

        # Only what cargo reads. Editing the book, the C SDK, or the schema then does not invalidate the binary.
        rustSrc = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./crates
            ./tocat.schema.json
          ];
        };

        # One workspace member, not the workspace: without -p this builds tocat-wasm-shell too.
        workspaceMember =
          {
            pname,
            description,
            mainProgram ? pname,
          }:
          rustPlatform.buildRustPackage {
            inherit pname version;
            src = rustSrc;
            cargoLock.lockFile = ./Cargo.lock;

            cargoBuildFlags = [
              "-p"
              pname
            ];

            nativeBuildInputs = nativeDeps;
            buildInputs = libDeps;

            doCheck = false;

            meta = {
              inherit description mainProgram;
              homepage = "https://github.com/lkalir/tocat";
              license = with pkgs.lib.licenses; [
                mit
                # Everything a guest author wants
                asl20
              ];
            };
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

            # markdown
            python3Packages.grip
            marksman
            dprint
          ];

          # tokio-console requires this cfg across the whole build graph.
          RUSTFLAGS = "--cfg tokio_unstable";

          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          RUST_BACKTRACE = "1";

          CLANG = "clang-unwrapped";
          CLANGXX = "clang++-unwrapped";
        };

        packages.default = self.packages.${system}.tocat;

        packages.tocat = workspaceMember {
          pname = "tocat";
          description = "A socat-inspired relay with a config file and a plugin pipeline";
        };

        packages.wasm-shell = workspaceMember {
          pname = "tocat-wasm-shell";
          description = "REPL for poking at a tocat WASM guest without a relay";
          mainProgram = "tocat-wasm-shell";
        };

        packages.wasm-sdk = pkgs.stdenv.mkDerivation {
          pname = "tocat-wasm-sdk";
          inherit version;
          src = ./sdk/wasm;

          nativeBuildInputs = [ pkgs.cmake ];
          cmakeFlags = [ "-DTOCAT_WASM_BUILD_EXAMPLES=OFF" ];

          meta = {
            description = "Guest SDK for tocat WASM plugins, for C and C++";
            homepage = "https://github.com/lkalir/tocat";
            license = with pkgs.lib.licenses; [
              mit
              asl20
            ];
          };
        };

        # Everything a guest author wants
        packages.wasm-sdk-full = pkgs.symlinkJoin {
          name = "tocat-wasm-sdk-full-${version}";
          paths = with self.packages.${system}; [
            wasm-sdk
            wasm-shell
          ];
        };

        checks.wasm-examples = pkgs.stdenv.mkDerivation {
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
