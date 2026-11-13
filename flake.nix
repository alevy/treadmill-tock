{
  description = "Drive a Tock board held in a Treadmill testbed from the `tock` CLI";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (
        pkgs:
        let
          # Everything a Treadmill image layer installs, as one directory of
          # binaries to pass to `build-image.sh --payload`.
          payload =
            cross:
            pkgs.symlinkJoin {
              name = "tml-tock-payload-${cross.stdenv.hostPlatform.config}";
              paths = [
                (cross.callPackage ./package.nix { withTransport = false; })
                (cross.callPackage ./tock.nix { })
              ];
            };
        in
        {
          default = pkgs.callPackage ./package.nix { };
          tock-transport-treadmill = pkgs.callPackage ./package.nix { };
          tock-cli = pkgs.callPackage ./tock.nix { };
        }
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          payload-aarch64 = payload pkgs.pkgsCross.aarch64-multiplatform-musl;
          payload-x86_64 = payload pkgs.pkgsCross.musl64;
          tml-tock-agent-aarch64 = pkgs.pkgsCross.aarch64-multiplatform-musl.callPackage ./package.nix {
            withTransport = false;
          };
          tml-tock-agent-x86_64 = pkgs.pkgsCross.musl64.callPackage ./package.nix {
            withTransport = false;
          };
        }
      );

      # `buildRustPackage` runs the test suite as part of the build, so the
      # package being buildable is most of the check. Lints reuse its
      # vendoring rather than setting up their own.
      checks = forAll (
        pkgs:
        let
          crate = self.packages.${pkgs.system}.default;
          lint = name: deps: phase: crate.overrideAttrs (old: {
            pname = "${old.pname}-${name}";
            nativeBuildInputs = old.nativeBuildInputs ++ deps;
            checkPhase = phase;
          });
        in
        {
          build = crate;
          clippy = lint "clippy" [ pkgs.clippy ] "cargo clippy --all-targets -- --deny warnings";
          fmt = lint "fmt" [ pkgs.rustfmt ] "cargo fmt --check";
        }
      );

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            websocat
          ];
        };
      });
    };
}
