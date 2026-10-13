#!/usr/bin/env python3
"""A virtual keyboard, injected through /dev/uinput.

Why this exists: KOReader on this machine cannot read the real keyboard —
`/dev/input/event*` is root:input and the user is not in that group, so the
reader opens no input device and receives no keys at all. `/dev/uinput` is
writable by the user, so the fix is to give the reader a device it is allowed to
open, and type into that.

Measured, and the reason this is not currently used: the device the kernel
creates for a uinput writer is owned root:input with mode 0660, not by the
process that wrote it, because udev sets the ownership from its input rules.
So the reader still cannot open it — `os.open` fails with EACCES, and `setfacl`
refuses because the file is not ours. Creating the device and typing into it
both work; only the reader's half of it does not.

What would make this work, in order of preference:

    sudo usermod -aG input shin    # then the real keyboard is readable, and
                                   # KOReader needs no synthetic device at all

Anything done with a synthetic device needs the reader restarted afterwards: it
scans /dev/input at startup, so a device that appears while it runs is invisible
to it. Create first, then start the reader.

The device is left in place by `create` and destroyed when that process exits.

ASCII input only. A character such as a Vietnamese letter does not map to one
key but to a sequence of them, and reproducing that needs a keymap rather than a
character, so search the ASCII form of a query — the engine folds diacritics
when it indexes, so `higashino` finds what it should.
"""

import fcntl
import os
import struct
import sys
import time

# From linux/uinput.h and linux/input-event-codes.h.
UI_DEV_CREATE = 0x5501
UI_DEV_DESTROY = 0x5502
UI_SET_EVBIT = 0x40045564
UI_SET_KEYBIT = 0x40045565

# The modern way to name a device: an ioctl taking `struct uinput_setup`
# (`struct input_id` + a name + a max-ff-effects count). Writing the struct
# straight at the fd is the old interface and the kernel rejects it.
UI_DEV_SETUP = 0x405C5503

EV_SYN = 0x00
EV_KEY = 0x01
EV_ABS = 0x03

SYN_REPORT = 0
KEY_RESERVED = 0
KEY_ENTER = 28
KEY_BACKSPACE = 14
KEY_TAB = 15
KEY_SPACE = 57
KEY_LEFTSHIFT = 42
KEY_LEFTCTRL = 29
KEY_ESC = 1

# ASCII characters that map straight onto one key, with the shift state needed.
KEYMAP = {
    "a": (30, 0), "b": (48, 0), "c": (46, 0), "d": (32, 0), "e": (18, 0),
    "f": (33, 0), "g": (34, 0), "h": (35, 0), "i": (23, 0), "j": (36, 0),
    "k": (37, 0), "l": (38, 0), "m": (50, 0), "n": (49, 0), "o": (24, 0),
    "p": (25, 0), "q": (16, 0), "r": (19, 0), "s": (31, 0), "t": (20, 0),
    "u": (22, 0), "v": (47, 0), "w": (17, 0), "x": (45, 0), "y": (21, 0),
    "z": (44, 0),
    "1": (2, 0), "2": (3, 0), "3": (4, 0), "4": (5, 0), "5": (6, 0),
    "6": (7, 0), "7": (8, 0), "8": (9, 0), "9": (10, 0), "0": (11, 0),
    " ": (KEY_SPACE, 0),
    "-": (12, 0), "=": (13, 0), "[": (26, 0), "]": (27, 0), "\\": (43, 0),
    ";": (39, 0), "'": (40, 0), "`": (41, 0), ",": (51, 0), ".": (52, 0),
    "/": (53, 0),
    "A": (30, 1), "B": (48, 1), "C": (46, 1), "D": (32, 1), "E": (18, 1),
    "F": (33, 1), "G": (34, 1), "H": (35, 1), "I": (23, 1), "J": (36, 1),
    "K": (37, 1), "L": (38, 1), "M": (50, 1), "N": (49, 1), "O": (24, 1),
    "P": (25, 1), "Q": (16, 1), "R": (19, 1), "S": (31, 1), "T": (20, 1),
    "U": (22, 1), "V": (47, 1), "W": (17, 1), "X": (45, 1), "Y": (21, 1),
    "Z": (44, 1),
    "!": (2, 1), "@": (3, 1), "#": (4, 1), "$": (5, 1), "%": (6, 1),
    "^": (7, 1), "&": (8, 1), "*": (9, 1), "(": (10, 1), ")": (11, 1),
    "_": (12, 1), "+": (13, 1), "{": (26, 1), "}": (27, 1), "|": (43, 1),
    ":": (39, 1), "\"": (40, 1), "~": (41, 1), "<": (51, 1), ">": (52, 1),
    "?": (53, 1),
}

# `struct input_event` on x86-64 is 24 bytes: a timeval is two longs, then the
# two u16 fields and the s32. Getting this wrong writes garbage the kernel
# drops, which shows up as a device that exists and does nothing.
EVENT_FMT = "@llHHi"
EVENT_SIZE = struct.calcsize(EVENT_FMT)


class VirtualKeyboard:
    """A uinput device that only reports key presses."""

    def __init__(self, name="turbosearch-virtual-keyboard"):
        self.fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
        self._name = name
        self._setup()

    def _setup(self):
        fcntl.ioctl(self.fd, UI_SET_EVBIT, EV_KEY)
        fcntl.ioctl(self.fd, UI_SET_EVBIT, EV_SYN)
        for code in {KEY_LEFTSHIFT, KEY_LEFTCTRL, KEY_ESC, KEY_TAB,
                     KEY_BACKSPACE, KEY_ENTER}:
            fcntl.ioctl(self.fd, UI_SET_KEYBIT, code)
        for code, _shift in KEYMAP.values():
            fcntl.ioctl(self.fd, UI_SET_KEYBIT, code)

        name_bytes = self._name.encode()[:80].ljust(80, b"\0")
        fcntl.ioctl(self.fd, UI_DEV_SETUP,
                    struct.pack("@HHHH80sI", 0x03, 0x1234, 1, 0,
                                name_bytes, 0))

        fcntl.ioctl(self.fd, UI_DEV_CREATE)
        # The kernel needs a moment to populate /dev/input/eventN.
        time.sleep(0.4)

    def _event(self, event_type, code, value):
        payload = struct.pack(EVENT_FMT, 0, 0, event_type, code, value)
        os.write(self.fd, payload)

    def _syn(self):
        self._event(EV_SYN, SYN_REPORT, 0)

    def press(self, code, shift=False):
        """One complete press-and-release of a key."""
        if shift:
            self._event(EV_KEY, KEY_LEFTSHIFT, 1)
            self._syn()
        self._event(EV_KEY, code, 1)
        self._syn()
        self._event(EV_KEY, code, 0)
        self._syn()
        if shift:
            self._event(EV_KEY, KEY_LEFTSHIFT, 0)
            self._syn()

    def type(self, text, delay=0.03):
        for char in text:
            entry = KEYMAP.get(char)
            if entry is None:
                sys.stderr.write(f"no key for {char!r}, skipped\n")
                continue
            self.press(*entry)
            time.sleep(delay)

    def key(self, code, times=1, delay=0.08):
        for _ in range(times):
            self.press(code)
            time.sleep(delay)

    def close(self):
        fcntl.ioctl(self.fd, UI_DEV_DESTROY)
        os.close(self.fd)


def main(argv):
    if len(argv) >= 2 and argv[1] == "create":
        keyboard = VirtualKeyboard()
        sys.stderr.write("virtual keyboard created\n")
        # Stay alive, or the kernel drops the device along with this process.
        try:
            while True:
                time.sleep(3600)
        except KeyboardInterrupt:
            keyboard.close()
        return 0

    if len(argv) >= 2 and argv[1] == "type":
        keyboard = VirtualKeyboard()
        keyboard.type(" ".join(argv[2:]))
        keyboard.close()
        return 0

    sys.stderr.write("usage: uinput_keys.py [create|type <text>]\n")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
