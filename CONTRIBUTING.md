# Contributing

Issues and pull requests are welcome. bulkhead writes to disks, so the bar is
evidence that a change works, not just that it compiles.

## Reporting a problem

Say which command you ran, paste what it printed, and say what the disk was
(size, interface: SATA, NVMe, USB bridge, VM). `bulkhead identify diskN` output
helps. **Remove serial numbers** if you don't want them public; erase output
and certificates include them.

A bug that loses or damages data goes to [SECURITY.md](SECURITY.md) first, not
a public issue.

## Sending a change

1. Fork, branch, change.
2. These must pass, as they do in CI:
   ```
   cargo fmt --check
   cargo clippy --locked --all-targets -- -D warnings
   cargo test --locked
   ```
3. If you touched a filesystem reader, partitioning, imaging, undelete or carve,
   run the conformance harness too. `fs-smoke.ps1` needs WSL with the `mkfs`
   tools. `smoke.ps1` needs an elevated prompt and creates and destroys its own
   throwaway VHDX disks. CI runs both through `tools/conformance.ps1`.
4. Anything that can only be proven on hardware (erase, a physical system
   disk): say what you ran it on and paste the output in the PR.

Keep the existing style: a module docstring saying what it's for, comments on
decisions rather than syntax, and a `ponytail:` comment on any deliberate
shortcut naming its limit.

## Where your code comes from

bulkhead is MIT. Code derived from a GPL project would make it GPL, so this
matters more than anything else in a PR.

- **Write from the specification, not from another implementation.** Good
  sources: Microsoft's open specifications (VHDX, MS-PST), Apple's TN1150
  (HFS+), the Linux kernel's *documentation* of the ext4 and XFS on-disk
  layouts, the UEFI spec (GPT), and the T13 ACS and NVMe specs.
- **Don't write code with these open:** e2fsprogs, xfsprogs, the Linux
  kernel's `fs/` source, ntfs-3g, TestDisk and PhotoRec, GNU ddrescue, nwipe,
  GParted, or WinFsp's sources. All are GPL. Running them as a black box to
  compare output is fine; reading their code and then writing "our own" isn't.
- If you used permissively licensed code (MIT, BSD, Apache, zlib), say so in
  the PR. It keeps its header, and it gets a `NOTICE` entry.
- **AI-assisted contributions are welcome**, provided a human understood and
  verified the change and can answer questions about it.

Contributors are credited in `CONTRIBUTORS.md`, with what they found or fixed.
