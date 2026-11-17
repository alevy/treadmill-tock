{
  lib,
  stdenv,
  rustPlatform,
  cacert,
  aws-lc,
  pkg-config,
  # Off for the image payload: the agent reads a JSON file and writes
  # config, so it needs no switchboard client and no TLS. That drops the
  # dependency tree from 254 crates to 102 and removes `aws-lc-sys`, whose
  # hand-written aarch64 assembly is the one thing here that resists
  # cross-compilation.
  withTransport ? true,
}:

rustPlatform.buildRustPackage {
  pname = if withTransport then "treadmill-tock" else "tml-tock-agent";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
      ./tests
    ];
  };

  cargoLock = {
    lockFile = ./Cargo.lock;
    # One entry per crate taken from a git dependency. `nix build` prints
    # the correct hash when a pin moves and this does not.
    outputHashes = {
      "tock-proto-0.0.1" = "sha256-qAfdyrcwXrPAIW53tErYAg/RCZLUHwlf7DD+ZMAtcA4=";
      "treadmill-rs-0.3.0" = "sha256-blHhLwRNyyxUmEsYovdm4vVi1+AXAPbdt79AdHaQUfA=";
      "treadmill-client-0.1.0" = "sha256-blHhLwRNyyxUmEsYovdm4vVi1+AXAPbdt79AdHaQUfA=";
    };
  };

  buildNoDefaultFeatures = !withTransport;

  # Only the transport speaks TLS, and nixpkgs sets AWS_LC_SYS_USE_SYSTEM=1
  # so `aws-lc-sys` links the packaged AWS-LC instead of compiling its
  # vendored copy. Without the feature none of this is reachable, which is
  # also why the cross payload needs no C toolchain at all.
  nativeBuildInputs = lib.optionals withTransport [ pkg-config ];
  buildInputs = lib.optionals withTransport [ aws-lc ];

  # The image payload is dropped into Raspberry Pi OS or Ubuntu, neither of
  # which ships a musl loader, so a musl build has to be static or it will
  # not start. nixpkgs links musl dynamically by default.
  env = lib.optionalAttrs stdenv.hostPlatform.isMusl {
    RUSTFLAGS = "-C target-feature=+crt-static";
  };

  # `tunnel_concurrency` builds a reqwest client, which refuses to construct
  # without a system trust store even though the test only ever talks to a
  # loopback stand-in. The sandbox has no certificates unless asked.
  nativeCheckInputs = [ cacert ];
  SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";

  meta = {
    description = "Drive a Tock board held in a Treadmill testbed from the `tock` CLI";
    mainProgram = if withTransport then "tock-transport-treadmill" else "tml-tock-agent";
  };
}
