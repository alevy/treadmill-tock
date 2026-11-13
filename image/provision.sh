#!/bin/sh
set -eu

# Treadmill image layer: makes the board wired to this host reachable from a
# developer's `tock` CLI.
#
# Expects the base layer, which already provides `tml`, `caddy`, `websocat`
# and an OpenOCD built from a pinned upstream commit. This layer adds the
# `tock` side and the two units that carry it.

: "${TML_ARCH:?}"
: "${TML_IMAGE_DIR:?}"
: "${TML_PAYLOAD_DIR:?}"

# Binaries arrive prebuilt in the payload, as `tml` and `caddy` do in the
# base layer. `nix build .#payload-<arch>` in the treadmill-tock repository
# produces all of them together.
#
# Every `tock` subcommand, not just the three the carrier needs: running
# `tock list` over SSH on the host is how you tell a broken carrier from a
# broken board, and `tock` alone cannot do it -- subcommands are separate
# executables it finds on PATH.
for binary in \
	tock tock-console tock-install tock-list tock-uninstall \
	tock-transport-file tock-transport-openocd tock-transport-serial \
	tml-tock-agent; do
	install -m 0755 "$TML_PAYLOAD_DIR/$binary" "/usr/local/bin/$binary"
done

# No board files are installed: `tock` compiles its reference set in, and
# reports them as `bundled`. A board that is not bundled is named by path in
# the config instead, which is a per-DUT fact the agent would have to write
# -- not something this layer can anticipate.

# Which `tock` this image ships. A developer's machine resolves its own board
# file, and a layout that disagrees with this one produces confusing refusals
# rather than a clear error; recording the version at least lets a mismatch be
# named. See docs/design.md, "A note on board-file skew".
mkdir -p /etc/tml
tock --version >/etc/tml/tock-version 2>/dev/null ||
	echo unknown >/etc/tml/tock-version

install -m 0644 "$TML_IMAGE_DIR/tml-tock-setup.service" \
	/etc/systemd/system/tml-tock-setup.service
install -m 0644 "$TML_IMAGE_DIR/tml-tock-broker@.service" \
	/etc/systemd/system/tml-tock-broker@.service

systemctl enable tml-tock-setup.service

# One websocat per transport. Enabling an instance whose transport this DUT
# does not support costs an idle process and announces nothing, because
# `tml-tock-agent` only declares services for transports the DUT actually
# has -- so both are enabled unconditionally rather than guessing here, where
# the host spec is not yet readable.
systemctl enable tml-tock-broker@openocd.service
systemctl enable tml-tock-broker@serial.service

# No udev rule here, deliberately. The base image already puts `tml` in
# `plugdev` and `dialout`, and on a live host that was enough for OpenOCD to
# claim a SEGGER J-Link as `tml`. A rule was written here first and removed
# once it proved unnecessary; if a probe family turns up that the base
# image's groups do not cover, this is where its rule goes.
