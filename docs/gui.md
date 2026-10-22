# The window

```powershell
bulkhead gui        # or run bulkhead-gui.exe
```

![The bulkhead window, after recovering three deleted files](gui-main.png)

Native Win32 controls, no toolkit, no new dependencies — so it runs anywhere
USER32 does, including WinPE, where the recovery media needs it.

The window never touches a disk. Every button runs `bulkhead.exe` from the same
folder as a child process and shows its output in the log, so the window can
get the arguments wrong but never the engine. The one exception is **Bring
disk online**, a `Set-Disk` call the engine has no command for.

## Administrator

`bulkhead-gui.exe` asks for elevation when it starts (a `requireAdministrator`
manifest), because every button reads a disk. `bulkhead gui` from an
unelevated prompt still opens, says at the top that no disk can be read, and
offers **Restart as administrator**. A refused disk shows in the log as
"Windows refused access", not as `0x80070005`. In WinPE everything already
runs elevated.

## The disk list

Each disk shows its size, bus (SATA, NVMe, USB, or `image` with the file's
path for a mounted VHDX), model and serial, plus `[holds Windows]` and
`[offline]` where they apply. Volumes sit under their disk with letter, size,
filesystem and label. The line under the list describes the selection and
says which buttons apply and why the others are off. The list refreshes after
every command.

Buttons that don't fit the selected row are greyed out: Scan, Rebuild table,
Erase info, Erase, Restore and Bring disk online need a disk; Undelete needs
an NTFS volume; Restore, Erase and Rebuild table are off for the disk holding
the running Windows; Erase is off for a drive that reports no serial number
(VHDs and some USB bridges), because there is nothing to check the typed
serial against.

## The buttons

| Group | Button | Runs |
|---|---|---|
| Back up | Disk -> image | `image <selection> <image file>` |
| | Mount image / Unmount | `mount` / `unmount <image file>` |
| Recover | Scan for partitions | `scan <disk>` (read-only) |
| | Rebuild table | `scan <disk> --rebuild`, after a confirmation naming the disk |
| | Undelete files / Carve files | `undelete` / `carve <selection> --to <folder>` |
| | Erase info | `erase-info <disk>` |
| | Open folder | opens the recover-into folder in Explorer |
| Overwrites a disk | Restore image -> disk | `restore <image file> <disk>` |
| | Erase disk | `erase <disk>`, with `--method overwrite` if ticked |
| | Bring disk online | `Set-Disk -IsOffline $false`, after a restore |

Paths are checked before anything starts: a new image needs a `.vhdx` name in
a folder that exists and mustn't overwrite one; mount, unmount and restore
need an image that exists; the recover-into folder needs a full path on a
drive that exists. Browse for the image is an Open dialog: pick an image to
mount or restore, or type a new name to image into.

## The destructive ones

The window does **not** decide these are safe. It has no `--yes` to give. It
pipes your answer to the engine's own prompt over stdin, so the check still
happens where it always did, after a dialog that names the disk by number,
model or image path, size, serial and volumes:

- **Restore** and **Rebuild table** answer the engine's `YES` prompt.
  Rebuild table saves the old table next to `bulkhead.exe` first; the log says
  where and how to put it back (`bulkhead undo`, from a command prompt — there
  is no button for that).
- **Erase** sends the serial typed in the box on the left, and `cmd_erase`
  compares it against the drive it is about to destroy. **Erase info** prints
  it. Get it wrong and the log says nothing was erased.

After a restore the disk is left offline, so Windows can't change its
partition IDs while the source image is still attached. **Bring disk online**
when you want its volumes in Explorer; unmount the image first.

## While it runs

Progress drives the bar, the status line and the title bar (readable while
minimised). Before the first percentage, and during any silent step — restore
spends half a minute taking a disk offline — the status line shows the last
step and a running clock, so it doesn't look hung. Every command ends with
**Done**, **Failed** (the log says why) or **Cancelled**, which stays on the
status line until the next one.

**Cancel** kills the running child. It says plainly that the child's cleanup
didn't run: an image may be left attached (press Unmount) and a VSS snapshot
left behind, and a half-written image isn't a backup.

The engine's command-line hints ("Add --rebuild", "bulkhead mount …",
`Set-Disk …`) are translated into what to press here; prompts the window has
already answered are hidden.

## Keyboard and size

Tab and Shift+Tab move between controls, Space presses a focused button, and
Ctrl+A selects all in a text box. The window opens inside the screen's work
area and has a minimum size, so the log and the last row of buttons can't be
squeezed off.

It is system-DPI aware: crisp on the main monitor at any scale, bitmap-stretched
if dragged to a monitor with a different scale.

There is no common-controls v6 manifest, because WinPE only has the v5
comctl32 that `bulkhead media` copies in. `part move` stays command-line only.

---

[< back to the README](../README.md)
