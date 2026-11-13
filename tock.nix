# `tock` and its transports, built from tockloader-ng.
#
# That project ships no nix packaging, which does not matter: the source is
# just an input. Pinned to the same revision this crate takes `tock-proto`
# from, so the protocol the image speaks and the protocol the transport was
# compiled against cannot drift apart -- bump both together or neither.
{
  lib,
  stdenv,
  rustPlatform,
  fetchgit,
  rev ? "512fc834d39a2539aeb2076f6778d2910cd024bc",
  hash ? "sha256-5XM6bF/Ry03xDHd5fe78T8GFPKFKrnBwU8rob0OSldc=",
}:

let
  src = fetchgit {
    url = "https://github.com/alevy/tockloader-next";
    inherit rev hash;
  };
in
rustPlatform.buildRustPackage {
  pname = "tock-cli";
  version = "0.0.1";
  inherit src;

  cargoLock.lockFile = "${src}/Cargo.lock";
  cargoBuildFlags = [
    "-p"
    "tock-cli"
  ];

  # Every binary, not just the three the carrier needs. `tock list` run over
  # SSH on the host is how you tell a broken carrier from a broken board,
  # and it is useless if only `tock` itself is installed.
  doCheck = false;

  # As in package.nix: the payload lands in a distribution with no musl
  # loader, so a musl build has to be static.
  env = lib.optionalAttrs stdenv.hostPlatform.isMusl {
    RUSTFLAGS = "-C target-feature=+crt-static";
  };

  meta = {
    description = "The tock CLI and its transports, for a Treadmill image payload";
    mainProgram = "tock";
  };
}
