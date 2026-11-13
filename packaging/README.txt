CHUD CLONE - disk cloning and diagnostics for Windows 10/11 (64-bit)
=====================================================================

CONTENTS
  chud-clone.exe        The application (graphical interface)
  chud-clone-cli.exe    Same engine on the command line (chud-clone-cli --help)
  portable.flag         (portable version only) settings and reports are saved next
                        to the program, in "ChudClone-data"
  licenses\             Licenses of Chud Clone, its fonts and the components it uses

LANGUAGE
  The interface, diagnostics and reports are available in English and French.
  The language follows Windows by default; change it in Settings > Display > Language.
  Command line: add --lang en (or --lang fr), or set the CHUD_LANG variable.

REQUIREMENTS
  - Windows 10 or 11, 64-bit.
  - Microsoft Edge WebView2 Runtime (ships with Windows 11 and up-to-date Windows 10).
    If it is missing the interface does not open: install "WebView2 Runtime" from
    Microsoft's website.
  - Administrator rights: reading and cloning disks needs them, so Windows asks for
    them (UAC prompt) every time Chud Clone starts.

PRIVACY
  No telemetry. Reports stay on this computer:
  - installed version: %APPDATA%\ChudClone\reports
  - portable version:  ChudClone-data\reports, next to the program

WHAT YOU CAN CLONE
  - Any disk to another disk, or to a disk image file (.img), or an image back to a disk.
  - A disk with Windows installed on it, connected externally (USB enclosure, dock,
    second internal disk): it is not the running system, so it can be cloned while
    Windows runs. Chud Clone locks and dismounts its volumes during the copy so that
    nothing changes underneath it.
  - The disk of the Windows that is currently running cannot be cloned from that same
    Windows (the copy would be inconsistent): use another Windows, or the WinPE rescue
    key or ISO (ChudClone-<version>-winpe.iso on the releases page; it boots from
    Ventoy or from a USB key written with Rufus).
  - Both disks in the same USB dock: supported. The source is read ahead while the
    destination is written, so both directions of the USB link work at the same time.

BEFORE CLONING TO A REAL DISK - PLEASE READ
  - The destination's content is permanently erased. Always keep the source disk intact
    until the clone has been checked in real use.
  - "Clone succeeded and verified" means: everything was copied, read back and compared,
    and the clone's structure is correct. Whether it actually boots also depends on the
    machine's firmware (BIOS/UEFI): it is not guaranteed.
  - On a larger disk, the Windows partition (NTFS) is grown automatically after the copy
    and its verification ("Grow the Windows partition" option). On the clone's first
    boot, Windows checks that volume once ("Scanning and repairing drive"): this is
    expected, let it finish.

HEALTH AND SURFACE SCAN
  - "Disks & health" shows each disk's SMART status and keeps a record at every
    reading: a degradation (pending or reallocated sectors increasing...) is flagged.
  - The surface scan reads the whole disk, without writing anything, and finds slow or
    unreadable areas. It repairs nothing. On a disk showing signs of weakness, clone it
    first: every full read stresses it.

SOURCE CODE, ISSUES
  https://github.com/BigFatDeuce/Chud-Clone
