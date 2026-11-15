export TML_IMAGE_CREATED="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
export TML_IMAGE_REVISION="$(git rev-parse HEAD)"
export TML_IMAGE_VERSION="$(date -u -d "$TML_IMAGE_CREATED" +%Y%m%dT%H%M%SZ)-${TML_IMAGE_REVISION:0:7}"
export TML_IMAGE_DOCUMENTATION="https://github.com/alevy/treadmill-tock/blob/$TML_IMAGE_REVISION/image/README.md"

nix develop ~/hack/tock/treadmill/images -c bash -c '
  payload=$(nix build --no-link --print-out-paths ~/hack/tock/treadmill/treadmill-tock#payload-aarch64)
  args=(); for f in "$payload"/bin/*; do args+=(--payload "$f"); done
  export CONTAINERS_REGISTRIES_CONF=$(mktemp)
  ~/hack/tock/treadmill/images/build/build-chain.sh --rebuild \
    --image ~/hack/tock/treadmill/treadmill-tock/image \
    -o ~/hack/tock/treadmill/treadmill-tock/out/tock-bridge "${args[@]}"
'
