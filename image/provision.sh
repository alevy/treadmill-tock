#!/bin/sh
set -eu

# The Tock bridge: carry the `tock` protocol from a developer's machine to
# the board wired to this host.
#
# A layer on the published base, which already provides everything this
# borrows -- `websocat` for the carrier, an OpenOCD built from a pinned
# commit for the probe, the Treadmill daemon and its caddy. Nothing is
# fetched here and nothing is built; this installs four binaries and wires
# them up.

: "${TML_PAYLOAD_DIR:?}"

# Four binaries, which is the whole of it: `tock` for its `broker`
# subcommand, one transport per capability, and the agent that writes their
# config at job start. The payload carries `tock`'s other subcommands too --
# they are a convenience for a human logged in over SSH, not part of the
# path to the board.
for binary in tock tock-transport-openocd tock-transport-serial tml-tock-agent; do
	install -m 0755 "$TML_PAYLOAD_DIR/$binary" "/usr/local/bin/$binary"
done

# Which `tock` this image ships. A developer's machine resolves its own board
# file, and a layout that disagrees with this one produces confusing refusals
# rather than a clear error; recording the version at least lets a mismatch
# be named.
mkdir -p /etc/tml
tock --version >/etc/tml/tock-version 2>/dev/null ||
	echo unknown >/etc/tml/tock-version

# Writes tock's config for the DUT wired to this host. Runs as root because
# the config has to land in tml's home, and the brokers run as tml.
#
# After the daemon, not before: the daemon is what writes
# /run/tml/host-spec.json, and this reads it. `Type=notify` on that unit
# means systemd holds this back until the daemon reports ready, by which
# point the file is there. Ordering it first -- which an earlier version did,
# to get service declarations in before the daemon read them -- made the
# agent fail on a file that could not exist yet. The declarations are
# written into the image instead, so nothing here needs to beat the daemon.
cat >/etc/systemd/system/tml-tock-setup.service <<'SERVICE'
[Unit]
Description=Configure tock for the board wired to this host
After=network.target tml-daemon.service
Requires=tml-daemon.service
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/bin/tml-tock-agent --config /home/tml/.config/tock/config.toml
ExecStartPost=/bin/chown -R tml:tml /home/tml/.config/tock
[Install]
WantedBy=multi-user.target
SERVICE
systemctl enable tml-tock-setup.service

# One instance per transport. websocat terminates the WebSocket a
# developer's `tock` opens through the job gateway and hands the connection
# to a broker it spawns; %i is both the socket directory's suffix and the
# transport `tock` is pinned to.
#
# Brokers are not serialised, which is measured rather than assumed:
# websocat keeps listening between connections and spawns one broker per
# connection, so two clients at once get two brokers and the second fails in
# OpenOCD with LIBUSB_ERROR_BUSY. Sequential use -- the actual development
# loop -- is fine, and concurrent use of one board is a developer racing
# themselves on a host they hold exclusively. A `.socket` unit with
# `Accept=yes` and `MaxConnections=1` is the fix if it ever matters.
#
# RestartSec=0, where the image's SSH service uses 5s: an ssh client retries
# a refused connection, and `tock-transport-treadmill` does not.
cat >/etc/systemd/system/tml-tock-broker@.service <<'SERVICE'
[Unit]
Description=Serve a tock %i broker over a WebSocket
After=network.target tml-tock-setup.service
Requires=tml-tock-setup.service
[Service]
User=tml
Group=tml
RuntimeDirectory=tml-tock-%i
RuntimeDirectoryMode=0755
ExecStart=/usr/local/bin/websocat --binary --exit-on-eof \
    ws-u:unix-l:/run/tml-tock-%i/broker.sock \
    exec:tock --exec-args --transport %i broker
Restart=always
RestartSec=0
[Install]
WantedBy=multi-user.target
SERVICE
systemctl enable tml-tock-broker@openocd.service
systemctl enable tml-tock-broker@serial.service

# Declared here rather than left to the agent, because the Treadmill daemon
# reads this directory once at start-up and `tml job reload-services` does
# not work -- its D-Bus handler runs on a thread with no tokio reactor and
# panics, returning success. A declaration written after the daemon starts
# is therefore never announced. These exist before it ever runs.
#
# Both transports are declared unconditionally. The agent only generates
# config for what the DUT actually has, so an announced service whose
# transport this board lacks fails on connect rather than misbehaving.
mkdir -p /etc/tml/services.d
cat >/etc/tml/services.d/tockopenocd.json <<'SERVICEDECL'
{
	"name": "tockopenocd",
	"label": "tock openocd",
	"protocol": "tockws",
	"upstream": "unix//run/tml-tock-openocd/broker.sock"
}
SERVICEDECL
cat >/etc/tml/services.d/tockserial.json <<'SERVICEDECL'
{
	"name": "tockserial",
	"label": "tock serial",
	"protocol": "tockws",
	"upstream": "unix//run/tml-tock-serial/broker.sock"
}
SERVICEDECL
