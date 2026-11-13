# Deploying into a Treadmill image

Two units and whatever installs them. Modelled on the image's existing SSH
service (`shared/treadmill-guest/treadmill-guest.sh` in the `images`
repository), so the shapes should look familiar to anyone who has debugged
that one:

| SSH | tock |
|---|---|
| `tml-sshws.service` | `tml-tock-broker@.service`, one instance per transport |
| `websocat … ws-u:unix-l:/run/tml-sshws/ssh.sock tcp:127.0.0.1:2222` | `websocat … ws-u:unix-l:/run/tml-tock-%i/broker.sock exec:tock …` |
| `"upstream": "unix//run/tml-sshws/ssh.sock"` | written by `tml-tock-agent`, same form |

## What the image needs

- `tml-tock-agent` on `PATH` (built from this crate, for the host's arch).
- `tock` and its board files, plus `tock-transport-openocd`,
  `tock-transport-serial`, and `openocd` itself.
- `websocat` — already fetched and sha256-pinned per arch in the image's
  `inputs.json`; nothing new to add.
- Both units, with `tml-tock-broker@openocd` and `tml-tock-broker@serial`
  enabled. `tml-tock-agent` declares services for whichever transports the
  DUT actually supports, so enabling an instance whose transport this DUT
  lacks announces nothing and costs an idle websocat.

## Verified against websocat 1.14.2

Two of the three unknowns were checked empirically rather than on hardware.
The image pins **1.14.1**; these were run against **1.14.2**, so a version
mismatch is still possible, but both behaviours are long-standing.

1. **`exec:` spawns without a shell.** `exec:sleep --exec-args 120` put
   `sleep` directly under websocat — `/proc/<pid>/comm` is `sleep`, cmdline
   `sleep 120`, no shell in between. So the process websocat signals is the
   one holding the probe. This matters because it is a bug this project
   already shipped once, in its own listener, and fixed.
2. **`--binary` preserves short reads as their own frames.** A child writing
   `AAAA`, pausing, then `BBBB` produced two separate 4-byte binary frames
   (opcode 2), not one buffered 8-byte frame. A buffering carrier here would
   hang rather than error, which is the same failure mode as the `splice(2)`
   short-read bug in this crate's history. websocat also sends a Close frame
   when the child reaches EOF.

## Still unverified — needs hardware

3. **Probe permissions.** The units run as `tml`, matching the SSH service.
   `tock broker` reaches a USB debug probe, which needs udev rules granting
   that user access. `openocd` failing to claim the probe is the most likely
   first-run failure.
4. **The carrier end to end.** `websocat` and the developer side's
   `wsproxy::run` have never been executed against each other — only each
   against a stand-in. That is the seam where two Critical defects hid behind
   a green suite during development. The first successful `tock list` against
   a real host is the test.
