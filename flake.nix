{
  description = "Drive a Tock board held in a Treadmill testbed from the `tock` CLI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      perSystem =
        { pkgs, system, ... }:
        let
          inherit (pkgs) lib;
          inherit (inputs) crane fenix;

          fenixPkgs = fenix.packages.${system};

          # Components listed individually rather than `stable.toolchain`:
          # that attribute's components overlap, and symlinkJoin logs every
          # collision. Same reasoning as treadmill's own nix/lib.nix.
          nativeToolchain = fenixPkgs.combine (
            with fenixPkgs.stable;
            [
              cargo
              rustc
              clippy
              rustfmt
              rust-src
            ]
          );

          craneLib = (crane.mkLib pkgs).overrideToolchain nativeToolchain;

          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
              ./tests
            ];
          };

          common = {
            inherit src;
            strictDeps = true;
            # The three git dependencies (tock-proto, treadmill-rs,
            # treadmill-client) are vendored by crane straight from
            # Cargo.lock, so there are no hashes to keep in sync here.
          };

          cargoArtifacts = craneLib.buildDepsOnly common;

          crate = craneLib.buildPackage (
            common
            // {
              inherit cargoArtifacts;
              doCheck = false;
            }
          );

          # One binary out of the crate, so a developer installing the
          # transport does not also get the in-image agent, and an image
          # payload does not carry the transport.
          only =
            name:
            pkgs.runCommand "${name}" { meta.mainProgram = name; } ''
              mkdir -p $out/bin
              cp ${crate}/bin/${name} $out/bin/${name}
            '';

          # Static musl builds for the Treadmill image payload. The guest is
          # a different distribution from whatever built it, so a static
          # binary is the only thing that reliably runs there.
          #
          # Built without the `transport` feature: the agent reads a JSON
          # file and writes config, so it needs no switchboard client and no
          # TLS. That takes the dependency tree from 254 crates to 102 and
          # removes aws-lc-sys, whose hand-written aarch64 assembly is the
          # one thing here that genuinely resists cross-compilation.
          crossAgent =
            targetTriple:
            let
              crossPkgs = import inputs.nixpkgs {
                inherit system;
                crossSystem.config = targetTriple;
              };
              toolchain = fenixPkgs.combine [
                fenixPkgs.stable.cargo
                fenixPkgs.stable.rustc
                fenixPkgs.targets.${targetTriple}.stable.rust-std
              ];
              crossCrane = (crane.mkLib pkgs).overrideToolchain toolchain;
              envTriple = lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] targetTriple);
            in
            crossCrane.buildPackage {
              inherit src;
              strictDeps = true;
              doCheck = false;
              cargoExtraArgs = "--no-default-features --bin tml-tock-agent";
              CARGO_BUILD_TARGET = targetTriple;
              "CARGO_TARGET_${envTriple}_LINKER" = "${crossPkgs.stdenv.cc.targetPrefix}cc";
              depsBuildBuild = [ crossPkgs.stdenv.cc ];
              HOST_CC = "${pkgs.stdenv.cc.nativePrefix}cc";
            };
        in
        {
          packages = {
            default = only "tock-transport-treadmill";
            tock-transport-treadmill = only "tock-transport-treadmill";
            tml-tock-agent = only "tml-tock-agent";
          }
          // lib.optionalAttrs pkgs.stdenv.isLinux {
            # Payload binaries for the image layer in `image/`.
            tml-tock-agent-aarch64 = crossAgent "aarch64-unknown-linux-musl";
            tml-tock-agent-x86_64 = crossAgent "x86_64-unknown-linux-musl";
          };

          checks = {
            inherit crate;
            clippy = craneLib.cargoClippy (
              common
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              }
            );
            fmt = craneLib.cargoFmt { inherit src; };
            tests = craneLib.cargoNextest (common // { inherit cargoArtifacts; });
          };

          devShells.default = craneLib.devShell {
            packages = with pkgs; [
              rust-analyzer
              websocat
            ];
          };
        };
    };
}
