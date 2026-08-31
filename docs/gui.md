# The window

```powershell
bulkhead gui
```

![The bulkhead window](gui-main.png)

Native Win32 controls, no toolkit, no new dependencies — so it runs anywhere
USER32 does, including WinPE, where the recovery media actually needs it. It is
DPI-aware, which on a 4K panel is the difference between crisp text and a
bitmap-stretched blur.

The window never touches a disk. Every button runs bulkhead as a child process
and pipes its output into the log, so the GUI can get the arguments wrong but
never the engine.

Progress redraws with a bare carriage return, which no text box can follow, so
it drives the bar, the status line and the title bar — the last of those stays
readable while the window is minimised. **Cancel** kills the running child; it
says so plainly, because a killed child never runs its own cleanup and leaves
the image attached and the VSS snapshot behind.

## The destructive buttons

`erase` and `restore` are on the second row, and the window does **not** decide
they are safe. It has no `--yes` to give. What it does is pipe your answer to
the engine's own prompt over stdin, so the check still happens where it always
did:

- **Restore** asks for `YES`, after a dialog naming the disk it will overwrite.
- **Erase** asks for the drive's serial, which `cmd_erase` compares against the
  drive it is about to destroy. Type it into the box on the left; **Erase info**
  prints it. Get it wrong and nothing happens.

`part move` and `scan --rebuild` are still command-line only.

---

[< back to the README](../README.md)
