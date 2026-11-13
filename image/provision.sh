#!/bin/sh
set -eu

# A Raspberry Pi 5 host whose only job is to carry the `tock` protocol to the
# board wired to it.
#
# This is a root image, not a layer on `../base`: that base is a developer
# workstation -- compilers for three architectures, editors, gdb, tmux -- and
# none of it is reachable from a protocol relay. What is left below is the
# netboot plumbing, OpenOCD, and the four binaries that move bytes.
#
# The Raspberry Pi OS and NBD arrangement is taken wholesale from
# `../base/provision.sh`; where the two agree, that file is the original and
# the comments there explain why.

: "${TML_ARCH:?}"
: "${TML_IMAGE_DIR:?}"
: "${TML_PAYLOAD_DIR:?}"

# Installing nbd-client triggers update-initramfs, which under Raspberry Pi
# OS's default MODULES=dep probes the current root device and fails in a
# no-boot build. MODULES=most bundles a broad set without probing, which an
# NBD-netboot image needs anyway.
mkdir -p /etc/initramfs-tools/conf.d
cat >/etc/initramfs-tools/conf.d/10-tml-netboot <<'CONF'
MODULES=most
CONF

# The whole package list. `dbus` is how the tml daemon is reachable,
# `ca-certificates` is how it reaches the switchboard, and the other two are
# how the root file system arrives and resolves. OpenOCD's build
# dependencies are installed and then purged by install-openocd.sh, so they
# are deliberately absent here -- naming one would make it permanent.
apt-get install -y \
	ca-certificates \
	dbus \
	nbd-client \
	systemd-resolved

"$TML_IMAGE_DIR/install-openocd.sh"

# No ttyd, no sshd, no Rust toolchain. A bridge has no terminal to serve and
# nothing to build. SSH is the one real loss -- it is how you look at a host
# that is misbehaving -- so it is a single `yes` away when a deployment wants
# it back.
# shellcheck disable=SC2016,SC2089,SC2090
daemon_args='--supervisor-url "http://$(ip route show 0.0.0.0/0 | cut -d" " -f3 | head -n1):3859"'
serial_consoles=''
with_ttyd=no
with_ssh=no
with_rustup=no
# shellcheck disable=SC2090
export daemon_args serial_consoles with_ttyd with_ssh with_rustup
"$TML_IMAGE_DIR/treadmill-guest.sh"

usermod -a -G dialout tml
usermod -a -G gpio tml

# --- the bridge itself -------------------------------------------------

# Four binaries, which is the whole of it: `tock` for its `broker`
# subcommand, one transport per capability, and the agent that writes their
# config. `tock`'s other subcommands are a debugging convenience for someone
# logged in, and nothing can log in here.
for binary in tock tock-transport-openocd tock-transport-serial tml-tock-agent; do
	install -m 0755 "$TML_PAYLOAD_DIR/$binary" "/usr/local/bin/$binary"
done

mkdir -p /etc/tml
tock --version >/etc/tml/tock-version 2>/dev/null ||
	echo unknown >/etc/tml/tock-version

install -m 0644 "$TML_IMAGE_DIR/tml-tock-setup.service" \
	/etc/systemd/system/tml-tock-setup.service
install -m 0644 "$TML_IMAGE_DIR/tml-tock-broker@.service" \
	/etc/systemd/system/tml-tock-broker@.service
systemctl enable tml-tock-setup.service
systemctl enable tml-tock-broker@openocd.service
systemctl enable tml-tock-broker@serial.service

# --- netboot plumbing, as in ../base -----------------------------------

cat >/etc/fstab <<'FSTAB'
proc /proc proc defaults 0 0
/dev/nbd0 / ext4 defaults,noatime,nodiratime 0 1
/dev/nbd1 /boot/firmware vfat defaults,x-systemd.requires=tml-nbd-boot.service,x-systemd.after=tml-nbd-boot.service 0 2
FSTAB

cat >/etc/systemd/system/tml-nbd-boot.service <<'SERVICE'
[Unit]
Description=Attach the boot file system over NBD
DefaultDependencies=no
Before=boot-firmware.mount
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'exec nbd-client "$(ip -4 route show default | cut -d" " -f3 | head -n1)" -N bootfs /dev/nbd1'
ExecStop=/usr/sbin/nbd-client -d /dev/nbd1
SERVICE

cat >/boot/firmware/cmdline.txt <<'CMDLINE'
console=serial0,115200 ip=dhcp root=/dev/nbd0 rw nbdroot=dhcp,rootfs,nbd0 rootfstype=ext4 fsckfix rootwait net.ifnames=0 loglevel=7
CMDLINE

mask_units() {
	for unit in "$@"; do
		ln -snf /dev/null "/etc/systemd/system/$unit"
	done
}

mask_units \
	dphys-swapfile.service \
	rpi-eeprom-update.service \
	userconfig.service \
	systemd-hostnamed.service \
	systemd-hostnamed.socket \
	systemd-logind.service \
	rpi-resize.service \
	systemd-growfs-root.service \
	rpi-resize-swap-file.service \
	sshswitch.service \
	cloud-init.service \
	cloud-init-local.service \
	cloud-init-network.service \
	cloud-final.service \
	cloud-init.target

# Raspberry Pi OS defaults to NetworkManager. eth0 carries the NBD root, so
# it must be networkd's alone or the two fight over the interface the root
# filesystem arrives on.
mask_units \
	NetworkManager.service \
	NetworkManager-wait-online.service \
	dhcpcd.service

# No `systemctl enable ssh.service`, and no /boot/firmware/ssh.txt: there is
# no sshd in this image to enable.
