# Secure erase

```powershell
bulkhead erase-info disk3
```

Blancco and KillDisk charge per drive for one command the drive already
implements, plus a piece of paper. The command is the easy part. The parts
worth building are knowing *which* command a given drive will accept, and
producing a record afterwards that means something — so that comes first, and
it is read-only.

It reports the drive's identity and every erase path it offers: ATA security
erase, ATA sanitize (crypto/block/overwrite), NVMe format, NVMe sanitize. More
usefully it says what is **stopping** one:

- **FROZEN** — nearly every desktop firmware freezes the ATA security state at
  boot, and a security erase cannot start until the drive is power-cycled.
  Suspend and resume, or hot-plug it. ATA *sanitize* is a separate feature set
  and is not affected, which is why it is preferred where available.
- **USB** — bridges rarely pass these commands through, and one that
  half-implements them can report success without erasing anything.
- **password already set** — an existing ATA password must be known first.
- **the drive did not answer** — some storage drivers, Intel RST and VMD
  especially, do not pass capability queries through at all. That is reported
  as *unknown*, never as "this drive cannot be erased": a question that was
  never asked is not a negative answer.

Where a drive advertises sanitize, `erase-info` also asks it for its sanitize
**status**. That command changes nothing, but it rides the same pass-through,
the same task-file split and the same 48-bit flag as the sanitize that erases
the drive — so an answer here means the destructive command will reach the
drive as well. Better to find that out before typing a serial number than
after.

```powershell
bulkhead erase disk5 --method overwrite
```

Writes zeros over every sector, then reads back 32 points spread across the
drive — including the first and last, where partition tables live — and refuses
to claim success unless they all come back blank. It samples the same points
*before* the write too, so it can say whether anything was actually removed or
the drive was already empty.

It asks for the drive's **serial number**, not a yes. A serial cannot be typed
by reflex, and finding it means looking at which drive this really is.

Where the drive offers a real sanitize, that is used instead and the drive
erases its own media:

```powershell
bulkhead erase disk1 --method ata-sanitize-crypto   # discards the key, seconds
bulkhead erase disk1 --method ata-sanitize-block    # erases every block
```

**Overwrite is the weaker method and is labelled as such wherever it appears.**
A firmware sanitize reaches blocks the drive has quietly remapped out of
service over its life; an overwrite reaches only what the drive currently maps.
On flash — SSDs, SD cards, USB sticks — wear levelling can leave old data in
spare blocks that no write will ever land on. If a drive advertises a sanitize
and you ask for an overwrite anyway, it says so before it starts.

Verification matches the method rather than assuming one shape. An overwrite or
a block erase has to read back blank. A crypto scramble does not and never
will: it throws the key away, so the media still reads as dense ciphertext.
Checking that for blankness would fail a successful erase, so what gets
verified is that the old contents are gone — and the output says plainly that
the key's destruction is the drive's claim, not a thing bulkhead observed.

## The certificate

```powershell
bulkhead erase disk5 --method overwrite --cert wipe-<serial>.html
bulkhead erase disk5 --method overwrite --cert wipe-<serial>.json
```

The other half of what Blancco sells. The extension picks the form: `.json` for
whatever consumes it next, anything else for a page that prints — drive
identity, the method, its NIST SP 800-88 class, elapsed time, and every sample
point with the first sixteen bytes before and after.

Three things it deliberately does:

- **It is written whether the erase passed or failed.** A certificate that only
  exists on success is a certificate that lies by omission, so a failed run
  produces one that says FAILED and marks which points did not verify.
- **It says Clear or Purge, and means it.** SP 800-88 draws that line exactly
  at the remapped-block problem: a host overwrite reaches what the drive maps
  (Clear), a firmware sanitize reaches the media (Purge). Claiming Purge for an
  overwrite is the one lie a certificate must not tell.
- **The limits are printed on the certificate itself**, not filed in a manual —
  that sampling is not a full read-back, that an overwrite cannot reach retired
  sectors, and that a crypto erase's key destruction is the drive's claim. It
  is read months later by someone who never saw the terminal.

It is self-attested and unsigned: nothing cryptographically ties the printed
page to the JSON record, and the document says so on its face.

The sanitize path has run. A 512 GB SATA SSD that advertises BLOCK_ERASE_EXT
only was erased with `--method ata-sanitize-block`: 14 s of drive time, all 33
sample points blank afterwards where 26 of them held data before — ext4
metadata, the protective MBR at offset 0, `EFI PART` in the last sector. The
certificate reads **Purge**. Crypto scramble is still unrun: that drive does
not offer it.

Getting even that far meant discovering that Windows' own `storahci` refuses
ATA opcode 0xB4 on `IOCTL_ATA_PASS_THROUGH_DIRECT` outright, with
ERROR_NOT_SUPPORTED, before the command reaches the drive. It is the opcode
being filtered and not the request: `IDENTIFY` and `READ VERIFY SECTORS EXT` go
through the very same call untouched, on the same drives. Wrapping the
identical command in a SCSI ATA PASS-THROUGH(16) CDB and letting the driver's
SAT layer unwrap it gets through, and the drive's own verdict comes back in the
sense data — so that is what `sanitize.rs` does. `examples/atprobe.rs` is the
experiment that established it, kept in the tree because it is the only thing
that tells you *which layer* is refusing a command.

The overwrite path has run: a 7.4 GB USB card reader was imaged, erased,
checked with `identify` and `scan` (both found nothing -- no table, no
filesystem signatures anywhere on the device), then restored from the image
with its contents intact.

That test is worth describing precisely, because it proves less than it looks
like it does. It shows the mapped sectors were blank afterwards and held data
before. It does not show the card retains no data at all -- an overwrite cannot
reach blocks the controller has remapped, and nothing observable from the host
can tell you whether any exist. For flash, only a firmware sanitize makes the
stronger claim.

---

[< back to the README](../README.md)
