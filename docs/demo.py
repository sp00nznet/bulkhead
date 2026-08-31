#!/usr/bin/env python3
"""Render docs/demo.gif -- the README's terminal demo.

    pip install pillow
    # drop termshot.py from github.com/sp00nznet/termshot next to this file
    python3 docs/demo.py

The transcript below is real output from a bench run on 2026-08-31 (a FIKWOT
FX815 512 GB on SATA), with the drive's serial replaced by a placeholder. Keep
it that way: the point of a rendered screenshot is that it matches what the
program actually prints, and the moment it drifts it is just an advert.
"""
import os

from termshot import Term, FG, GREEN, CYAN, DIM, YELLOW, PROMPT

HERE = os.path.dirname(os.path.abspath(__file__))

# Consolas is on every Windows box; DejaVu is termshot's default and is not.
FONT_REG = r"C:\Windows\Fonts\consola.ttf"
FONT_BOLD = r"C:\Windows\Fonts\consolab.ttf"

SERIAL = "FKW512G0000123456"


class PSTerm(Term):
    """A PowerShell prompt rather than termshot's `user@host:cwd$`.

    bulkhead is a Windows tool and needs an elevated prompt; a bash prompt on
    the README would be the first thing a reader noticed was wrong.
    """

    def prompt(self, command="", cursor=True):
        segs = [("PS ", PROMPT, True), (self.cwd, CYAN, True),
                ("> ", FG, False), (command, FG, False)]
        if cursor:
            segs.append(("\u2588", FG, False))
        return segs


def one(text, color=FG, bold=False):
    return [(text, color, bold)]


def kv(key, value):
    return [("  " + key.ljust(9), DIM, False), (value, FG, False)]


t = PSTerm(title="Administrator: Windows PowerShell", font_size=21,
           cwd="F:\\", reg=FONT_REG, bold=FONT_BOLD)

# 1. Read a Linux filesystem Windows will not mount.
t.type("bulkhead ls disk0 --at 1MB")
t.reveal([
    one(r"[*] \\.\PhysicalDrive0 at 1.0 MB: ext2/3/4, 476.9 GB", CYAN),
    one("                dump/"),
    one("                images/"),
    one("                template/"),
    one("       29.6 GB  vzdump-qemu-100-2026_07_20-02_06_48.vma.zst"),
    one("[*] 8 entries", DIM),
], ms=260)
t.blank()

# 2. Ask the drive what it will actually accept.
t.type("bulkhead erase-info disk0")
t.reveal([
    one(r"[*] \\.\PhysicalDrive0 (476.9 GB)", CYAN),
    kv("model", "FIKWOT FX815 512GB"),
    kv("serial", SERIAL),
    kv("bus", "SATA/ATA"),
    one("  ATA security: supported, FROZEN", YELLOW),
    one("  ATA sanitize: block"),
    one("[*] usable: ata-sanitize-block", CYAN),
    one("[+] sanitize commands reach this drive; none in progress", GREEN),
], ms=240)
t.blank()

# 3. The drive erases its own media, and says so on paper.
t.type("bulkhead erase disk0 --method ata-sanitize-block --cert purge.html")
t.reveal([
    one("[!] This ERASES disk 0: FIKWOT FX815 512GB (476.9 GB)", YELLOW),
    one("[!] The drive erases itself (BlockErase). There is no undo.", YELLOW),
    one("    Type the serial (" + SERIAL + ") to continue: " + SERIAL),
    one("[*] the drive accepted the command and is erasing itself", CYAN),
], ms=300)
for pct in ("  25%", "  44%", "  74%", "  89%", " 100%"):
    t.add(list(t._screen) + [one(pct, DIM)], 260)
t._screen.append(one(" 100%", DIM))
t.reveal([
    one("[+] certificate written to purge.html", GREEN),
    one("[+] 33 sample points across the drive read back blank", GREEN),
    one("[+] those points held data before, and do not now", GREEN),
], ms=380)
t.hold(2600)

out = os.path.join(HERE, "demo.gif")
t.save_gif(out)
print("wrote", out)
