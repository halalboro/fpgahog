# 🦔 fpgahog

Reserve FPGAs, and whole hosts, on machines you share with other people.

A fork of [hosthog](https://github.com/pogobanane/hosthog) that adds **per-FPGA claims** and
keeps whole-host hogging as it was.

```
Usage: fpgahog [COMMAND]

Commands:
  status    show current claims
  claim     Claim a resource. Fails if already claimed exclusively
  release   prematurely release a claim
  list      List the FPGAs this host offers and who holds them
  discover  Look for FPGAs on the PCI bus and optionally register them
  check     Exit 0 if you may use these resources now, 1 if someone else holds them
  hog       Hog the entire host (others will hate you)
  post      post a message to all logged in users
  users     List all logged in users
```

## Claiming

Anything claimable is a *resource*: `host` is the whole machine, every other name is an FPGA.

```bash
sudo fpgahog claim u280 4h running perf tests   # one board
sudo fpgahog claim host 15min --exclusive       # the machine
sudo fpgahog claim host,u280 2h                 # both at once
sudo fpgahog release u280                       # give one back
sudo fpgahog release                            # give back your hogs and exclusive claims
sudo fpgahog hog                                # lock ssh, timers and every board
fpgahog check u280 || exit 1                    # in a script, before touching a board
```

Claims on different boards never collide, so two people can use two cards at once. FPGA claims
are **exclusive** by default and host claims **shared**; `--exclusive` and `--shared` override
that. A shared claim only announces; an exclusive one is enforced.

## Registering the boards on a host

```bash
sudo fpgahog discover --write
```

This scans PCI and USB, registering boards under placeholder names (`fpga0`, `fpga1`, …) and
assigning each one the JTAG cable whose product name matches its alias. Then edit
`settings.fpgas` in `/var/lib/fpgahog/fpgahog.json` to give the boards real names and check
what was guessed:

```json
{ "alias": "u280", "bdf": "0000:c1:00.0", "pci_id": "10ee:903f",
  "devices": ["/dev/coyote_ultrascale_plus_fpga_0_reconfig",
              "/dev/coyote_ultrascale_plus_fpga_0_v0"],
  "cables":  ["217702174005"] }
```

A human has to check `devices`: a node named `coyote_fpga_0` carries an enumeration index, not
a PCI address, so when two cards share a driver nothing says which node is which. `discover`
fills the list only when one board uses that driver. Re-run it after a driver change; it
replaces a list whose nodes have all disappeared, and reports one that is only partly stale.
`list` marks nodes `(MISSING)` and shows each cable's state.

## What an exclusive claim enforces

- **Device files** are `chown`ed to you and set to `0600`; the original ownership is recorded
  and restored on release.
- **The board's JTAG cable and serial consoles** the same way, plus a rule in
  `/run/udev/rules.d` so the cable comes back locked after a replug or power cycle.
- **Every 5 minutes** an `at` job re-applies the locks, because a driver reload recreates
  device nodes world-writable.
- **`hog`** also blocks ssh logins (by bind-mounting empty `authorized_keys` files), stops all
  systemd timers, and takes every registered board.

If a lock cannot be applied the claim is still recorded, and `status` says `ENFORCEMENT FAILED`
rather than implying protection that is not there.

### Handles that are already open

Permissions do not revoke an open file, so a claim **refuses** when someone else holds a board
or its cable and names the process. `claim --force` and `hog --force` then re-plug the cable at
its hub port, which does cut every open handle off. Open *device files* keep working until
their process closes them.

## Limits

- **It stops accidents, not people.** Anyone with sudo can undo any of it.
- **A driver reload leaves a board open** until the next 5-minute pass.
- **After a reboot** cables are unlocked until the first fpgahog run, since the udev rule lives
  in `/run`.
- **One host at a time**: state is host-local, so run fpgahog on the machine you want.

## hosthog compatibility

Scripts and habits carry over: the binary is also installed as `hosthog`, `claim` still accepts
`claim --exclusive 15min some benchmarks` (claiming the host), and a bare `release` gives back
your hogs and exclusive claims while leaving shared ones. fpgahog reads hosthog's v1 and v2
statefiles, but keeps its own at `/var/lib/fpgahog/fpgahog.json`, so it can run beside an
installed hosthog — the two cannot see each other's claims, so do not hog with both at once.

One deliberate difference: hosthog's `release` unhogs a host somebody else hogged; fpgahog only
undoes a hog whose claim is gone.

## Installation

Needs `at` for claims to expire on time.

```bash
just install     # builds the nix package and pins it at /var/lib/fpgahog/pkg as a GC root
ln -s /var/lib/fpgahog/pkg/bin/fpgahog ~/.local/bin/fpgahog
```

Link the binary, not the directory: the package also provides a `hosthog` that would shadow an
installed one. Expiry and lock re-checks run fpgahog again later from `at`, and they use this
installed copy, so rebuilding the repository cannot strand a claim. `cargo install --path .`
works too, but then those jobs call whichever binary scheduled them, and fpgahog warns when
that is a `target/` build.
