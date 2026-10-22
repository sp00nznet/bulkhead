# Security

bulkhead reads and writes raw disks as administrator. A bug in it can lose
data, or fail to erase data someone believes is gone. Both count as security
issues here.

## Reporting

Report privately through GitHub's
[private vulnerability reporting](https://github.com/sp00nznet/bulkhead/security/advisories/new),
not a public issue. Include the command, its output, and what the disk was.
You'll get a reply within a week.

In scope:
- anything that writes to a disk or partition other than the one named;
- an erase that reports success, or a certificate that says Clear or Purge,
  when data is still readable;
- `restore`, `part move` or `scan --rebuild` corrupting a filesystem;
- a crafted disk image or filesystem that makes a reader crash, hang, or write
  outside the `--to` directory (undelete, cp, carve).

## What bulkhead does to protect you

- `restore`, `erase` and `part move` ask before writing. `erase` makes you type
  the drive's serial number. `restore` refuses the disk Windows is running from.
- `scan --rebuild` saves the old partition table first, and `bulkhead undo`
  puts it back.
- Images and filesystems are read-only unless you ask otherwise (`mount --rw`).
- The erase certificate is written whether the erase passed or failed, and its
  NIST 800-88 level (Clear or Purge) comes from the method used. It is not
  signed: it's a record, not proof against tampering.

## What it can't do

- bulkhead doesn't decrypt BitLocker. An encrypted volume images as ciphertext.
- An overwrite erase can't reach blocks the drive has remapped, so it never
  certifies more than Clear. For Purge, use a firmware method
  (`erase-info` lists what the drive supports).
- Erase certificates and `erase-info` output contain the drive's serial
  number. Treat them as you would the drive.
