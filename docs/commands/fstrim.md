# brokkr fstrim

`brokkr fstrim` syncs and trims filesystems while holding the brokkr lock,
so no bench in any project measures while a trim runs. This page covers
what a trim does and what it doesn't establish, then the helper the
privileged step runs in (Installing the helper, How a trim runs) and the
refusals.

These are the forms the command takes:

```sh
brokkr fstrim                    # the project's scratch, else the cwd
brokkr fstrim /data /media/fast  # the filesystems holding these
```

## What a trim does

A trim is two system calls on the filesystem holding each path, in this
order:

1. `syncfs`, which writes back everything dirty on that filesystem
2. `FITRIM` over the whole filesystem, which asks it to pass its free
   space to the device as discards

Every path is a directory on ext4, and brokkr opens and checks each one
before the first trim runs, so a bad later path refuses the whole
command. Two paths on one filesystem are trimmed once. With no path,
brokkr trims the filesystem holding the project's resolved `scratch`
(`brokkr man bench`, Iteration directory), which must exist, or the
current directory outside a project. brokkr prints one line per
filesystem: how long the sync and the trim took and the length `FITRIM`
returned.

That length is what the filesystem reports it passed on for discard, not
what the device discarded. A successful trim doesn't establish that every
block a deleted file freed was eligible, since ext4 can make a freed
block available only after its journal commit runs, and it doesn't
establish that the device has settled.

`brokkr fstrim` takes the lock and waits for it like `brokkr check`. The
free-space gate doesn't apply, since a full disk is a reason to trim.

## Installing the helper

The privileged step runs in `brokkr-fstrim`, a helper with one job:
`syncfs` then `FITRIM` on a directory brokkr hands it already open. No
sudo or polkit is involved. `cargo install` puts a copy beside `brokkr`,
which brokkr never runs, since any write to a file drops its
capability.

Install it once, root-owned, at the fixed path brokkr runs:

```sh
sudo install -D -o root -g "$(id -gn)" -m 0750 \
    ~/.cargo/bin/brokkr-fstrim /usr/local/libexec/brokkr-fstrim
sudo setcap cap_sys_admin+ep /usr/local/libexec/brokkr-fstrim
```

Repeat both steps after an upgrade that changes the helper's protocol;
`brokkr env` says when the installed one doesn't match. The `fstrim:`
row shows whether a trim can run, and if not, why.

The mode `0750` and your group decide who may run the helper. Anyone who
can run it can sync and trim any ext4 filesystem they can open a
directory on, with or without brokkr. Don't install it with a group that
includes anyone else.

## How a trim runs

Once per command, before the first launch, brokkr walks
`/usr/local/libexec/brokkr-fstrim` from `/` without following a link.
It refuses when the file or any directory above it isn't owned by root
or is writable by anyone else. It then runs that very file for the probe
and for every trim, so a file put at the path afterwards never runs. The
helper itself refuses to start a trim without `CAP_SYS_ADMIN` or for a
protocol other than its own.

The helper receives the target directory and the lock as open
descriptors, never as paths:

- **The target**: the directory brokkr opened and checked, so the
  filesystem trimmed is the one checked. The helper refuses one that
  isn't ext4.
- **The lock**: a copy of the descriptor brokkr holds the lock through.
  If brokkr dies during a trim, the lock stays held until the helper
  exits, and the helper is killed with its parent, so ext4 stops the
  trim at its next checkpoint.

brokkr waits for the helper to exit before it goes on. Where brokkr
can't wait, or dies, the helper's copy of the descriptor keeps the lock
held until the helper exits. Ctrl-C and `brokkr kill` stop the helper
like any child. brokkr then reports the stages that finished, the sync
for example, and the rest as unreported, crediting only lines the helper
printed whole and in order.

## Refusals

brokkr refuses before any trim runs in these cases:

| Message | Cause | Fix |
|---|---|---|
| `/usr/local/libexec/brokkr-fstrim does not exist` | The helper isn't installed | Install it (see Installing the helper) |
| `... is not root-owned and writable by root alone` | The helper or a directory above it can be changed by someone other than root | Fix the owner and mode, or reinstall |
| `... speaks protocol N, this brokkr M` | The installed helper is from another brokkr | Reinstall it |
| `... runs without CAP_SYS_ADMIN` | The capability was never set, or a copy over the file dropped it | Run the `setcap` step again |
| `... is not on ext4` | The path's filesystem isn't ext4 | Trim it some other way |
| `... (a trim target is a directory)` | The path doesn't exist or isn't a directory | Name a directory |

A trim that fails after it starts, `FITRIM` refused by the device for
example, prints its line as an error, and the command exits non-zero
once every filesystem has been tried.
