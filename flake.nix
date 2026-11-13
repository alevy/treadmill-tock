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
      packages = forAll (pkgs: {
        default = pkgs.callPackage ./package.nix { };
        tock-transport-treadmill = pkgs.callPackage ./package.nix { };

        # Static payload binaries for the image layer in `image/`, built
        # without the `transport` feature. See package.nix for why.
      }
      // nixpkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
        tml-tock-agent-aarch64 = pkgs.pkgsCross.aarch64-multiplatform-musl.callPackage ./package.nix {
          withTransport = false;
        };
        tml-tock-agent-x86_64 = pkgs.pkgsCross.musl64.callPackage ./package.nix {
          withTransport = false;
        };
      });

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
