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

## Three things to verify on the first real deployment

None of these can be checked without hardware, and each fails quietly
rather than loudly.

1. **`exec:` semantics.** `exec:tock --exec-args --transport %i broker`
   should spawn `tock` directly, with no shell between websocat and the
   broker. That matters: a shell in between means the process websocat
   signals is not the one holding the probe, which is a bug this project
   already shipped once and fixed. Confirm against websocat 1.14.1
   specifically; if `exec:` does not behave, `cmd:` works but reintroduces
   the shell.
2. **Frame boundaries under `--binary`.** The developer's end writes a
   request and waits for a reply, so a short read must leave as its own
   frame rather than sit in a buffer. A buffering carrier here hangs rather
   than errors — the same failure mode as the `splice(2)` short-read bug
   fixed in this crate's history (see `git log`).
3. **Probe permissions.** The units run as `tml`, matching the SSH service.
   `tock broker` reaches a USB debug probe, which needs udev rules granting
   that user access. `openocd` failing to claim the probe is the most
   likely first-run failure.
