"""Fileflux native Escape-restore regression smoke (X11 only; stdlib + libXtst via ctypes).

Bounded helper for the reopened T3 findings: it launches the local build
against a synthetic HOME/XDG fixture only, finds its window by the launched
PID, and drives real keys through XTest (the host ignores XSendEvent keys).
It reads only its own fixture window's `_NET_WM_NAME`; no global screenshots,
no clipboard, no real-home reads, no mouse events.

Automated assertions:
1. The window appears, titled "<home folder> — Fileflux".
2. Ctrl+L, nonexistent path, Enter, Escape, Enter: the title still names the
   failed folder (failed submit + Escape preserve the committed folder).
3. Ctrl+F, non-matching query, Escape: the app stays alive; a grid-content
   comparison is not automatable without an AT-SPI session, so the helper
   prints MANUAL for that visual check and never claims it passed.

Usage: nix develop --command python3 scripts/native-smoke.py
Requires target/debug/fileflux and an X display. Exits 3 with a MANUAL
report when only unverifiable checks remain; exits 0 only if every
automated assertion held.
"""

import ctypes
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

XTST = "/nix/store/i16gl3mqqn816xfn1lmp5k6yiq1z75ka-libxtst-1.2.5/lib/libXtst.so.6"
BINARY = "target/debug/fileflux"

KEYSYM = {
    "Return": 0xFF0D, "Escape": 0xFF1B, "Control": 0xFFE3,
}


def keysym_for(name: str) -> int:
    """A keysym for a named key or any printable ASCII character."""
    if name in KEYSYM:
        return KEYSYM[name]
    if len(name) == 1 and 0x20 <= ord(name) <= 0x7E:
        return ord(name)
    raise KeyError(f"no keysym for {name!r}")


class X:
    def __init__(self) -> None:
        self.x11 = ctypes.CDLL("libX11.so.6")
        self.xtst = ctypes.CDLL(XTST)
        self.x11.XOpenDisplay.restype = ctypes.c_void_p
        self.x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
        self.x11.XFlush.argtypes = [ctypes.c_void_p]
        self.x11.XKeysymToKeycode.restype = ctypes.c_uint
        self.x11.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        self.xtst.XTestFakeKeyEvent.argtypes = [
            ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong
        ]
        self.x11.XSetInputFocus.argtypes = [
            ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong
        ]
        self.x11.XMapRaised.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        self.x11.XInternAtom.restype = ctypes.c_ulong
        self.x11.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
        self.x11.XGetWindowProperty.restype = ctypes.c_int
        self.x11.XGetWindowProperty.argtypes = [
            ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong,
            ctypes.c_long, ctypes.c_long, ctypes.c_int, ctypes.c_ulong,
            ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_void_p),
        ]
        self.x11.XDefaultRootWindow.restype = ctypes.c_ulong
        self.x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
        self.x11.XQueryTree.restype = ctypes.c_int
        self.x11.XQueryTree.argtypes = [
            ctypes.c_void_p, ctypes.c_ulong,
            ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_uint),
        ]
        self.window = 0
        self.app_pid = None
        self.display = self.x11.XOpenDisplay(None)
        if not self.display:
            raise RuntimeError("no X display: run under X11/XWayland")
        self.root = self.x11.XDefaultRootWindow(self.display)
        self.codes: dict[str, int] = {}

    def _keycode(self, name: str) -> int:
        code = self.codes.get(name)
        if code is None:
            code = self.x11.XKeysymToKeycode(self.display, keysym_for(name))
            self.codes[name] = code
        return code

    def key(self, name: str, down: bool) -> None:
        self.xtst.XTestFakeKeyEvent(self.display, self._keycode(name), 1 if down else 0, 0)
        self.x11.XFlush(self.display)

    def focus(self, window: str) -> None:
        """Raise, map and focus our own fixture window.

        Mapping needs a beat to become viewable — XSetInputFocus on an
        unviewable window is an error Xlib reports asynchronously — so the
        calls settle before the readback.
        """
        self.window = int(window, 16)
        self.x11.XMapRaised(self.display, self.window)
        self.x11.XFlush(self.display)
        time.sleep(0.3)
        # RevertToPointerRoot = 1, CurrentTime = 0.
        self.x11.XSetInputFocus(self.display, self.window, 1, 0)
        self.x11.XFlush(self.display)

    def refocus(self) -> None:
        """Re-establish input focus right before a chord."""
        if self.window:
            self.x11.XSetInputFocus(self.display, self.window, 1, 0)
            self.x11.XFlush(self.display)

    def chord(self, *names: str) -> None:
        """Hold all but the last, tap the last, release in reverse."""
        for name in names[:-1]:
            self.key(name, True)
        self.key(names[-1], True)
        self.key(names[-1], False)
        for name in reversed(names[:-1]):
            self.key(name, False)

    def tap(self, name: str) -> None:
        self.chord(name)

    def type_text(self, text: str) -> None:
        """Type literal ASCII text, one printable character at a time."""
        for character in text:
            self.tap(character)

    def close(self) -> None:
        self.x11.XCloseDisplay(self.display)


class Fixture:
    """A synthetic HOME with one file named alpha.txt."""

    def __init__(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="fileflux-smoke-"))
        self.home = self.root / "home"
        self.home.mkdir()
        (self.home / "alpha.txt").write_bytes(b"a")
        self.alt = self.root / "alt-folder"
        self.alt.mkdir()

    def env(self) -> dict[str, str]:
        env = dict(os.environ)
        env["HOME"] = str(self.home)
        env["XDG_CONFIG_HOME"] = str(self.root / "config")
        env["XDG_DATA_HOME"] = str(self.root / "data")
        env.pop("WAYLAND_DISPLAY", None)
        return env

    def cleanup(self) -> None:
        shutil.rmtree(self.root, ignore_errors=True)


# Window property walk, pure Xlib (no xprop/xwininfo out in this shell).

CARDINAL = 6  # XA_CARDINAL
_PROP_SUCCESS = 0


class AtomRef:
    """The lazily interned atoms the PID/title reads need."""

    def __init__(self) -> None:
        self.pid: int | None = None
        self.name: int | None = None
        self.utf8: int | None = None


UTF8_STRING = AtomRef()  # filled after the display opens


def intern_atoms(x: X) -> None:
    UTF8_STRING.pid = x.x11.XInternAtom(x.display, b"_NET_WM_PID", 0)
    UTF8_STRING.name = x.x11.XInternAtom(x.display, b"_NET_WM_NAME", 0)
    UTF8_STRING.utf8 = x.x11.XInternAtom(x.display, b"UTF8_STRING", 0)


def window_pid(x: X, window: int) -> int | None:
    """The `_NET_WM_PID` of the window, or None."""
    if UTF8_STRING.pid is None:
        return None
    type_, data, _length = read_property(x, window, UTF8_STRING.pid)
    if type_ != CARDINAL or not data:
        return None
    value = ctypes.cast(data, ctypes.POINTER(ctypes.c_uint)).contents.value
    x.x11.XFree(data)
    return value


def read_property(x: X, window: int, property_: int):
    """One property, sliced to (type, bytes pointer, count); free with XFree."""
    actual_type = ctypes.c_ulong()
    actual_format = ctypes.c_int()
    items = ctypes.c_ulong()
    bytes_after = ctypes.c_ulong()
    data = ctypes.c_void_p()
    status = x.x11.XGetWindowProperty(
        x.display, window, property_, 0, 4096, 0, 0,
        ctypes.byref(actual_type), ctypes.byref(actual_format),
        ctypes.byref(items), ctypes.byref(bytes_after), ctypes.byref(data),
    )
    if status != _PROP_SUCCESS or not data:
        return None, None, 0
    return actual_type.value, data, items.value


def window_name(x: X, window: int) -> str:
    """The `_NET_WM_NAME` of the window, as text."""
    if UTF8_STRING.name is None or UTF8_STRING.utf8 is None:
        return ""
    type_, data, length = read_property(x, window, UTF8_STRING.name)
    if type_ != UTF8_STRING.utf8 or not data:
        return ""
    raw = ctypes.string_at(data, length)
    x.x11.XFree(data)
    return raw.decode("utf-8", "replace").strip()


def all_windows(x: X) -> list[int]:
    """Every direct child of the root window (no nested walk needed)."""
    root = ctypes.c_ulong()
    parent = ctypes.c_ulong()
    children = ctypes.c_void_p()
    count = ctypes.c_uint()
    status = x.x11.XQueryTree(
        x.display, x.root, ctypes.byref(root), ctypes.byref(parent),
        ctypes.byref(children), ctypes.byref(count),
    )
    if not status or not children:
        return []
    found = [
        ctypes.cast(children, ctypes.POINTER(ctypes.c_ulong))[index]
        for index in range(count.value)
    ]
    x.x11.XFree(children)
    return found


def find_window(x: X) -> str | None:
    """Our fixture window: the one whose `_NET_WM_PID` equals the app's."""
    if x.app_pid is None:
        return None
    for candidate in all_windows(x):
        if window_pid(x, candidate) == x.app_pid:
            return hex(candidate)
    return None


def fail(message: str) -> None:
    print(f"FAIL: {message}", file=sys.stderr)
    sys.exit(1)


def manual(message: str) -> None:
    print(f"MANUAL: {message}", flush=True)
    sys.exit(3)


def main() -> None:
    fixture = Fixture()
    x = None
    app = None
    manual_report = False
    try:
        x = X()
        with (Path("target") / "native-smoke.log").open("w") as log:
            app = subprocess.Popen([BINARY], stdout=log, stderr=log, env=fixture.env())
            x.app_pid = app.pid
            intern_atoms(x)
            window = None
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline and window is None:
                if app.poll() is not None:
                    fail(f"app exited early with {app.returncode}: see log")
                window = find_window(x)
                time.sleep(0.2)
            if window is None:
                fail("no window owned by the app PID within 30s")
            time.sleep(1.5)  # first scan + places discovery settle

            expected = "Home — Fileflux"
            title = window_name(x, int(window, 16))
            if expected not in title:
                fail(f"title {title!r} does not contain {expected!r}")
            print(f"PASS: window titled for the fixture home ({title!r})", flush=True)

            # Input-focus probe, run before any assertion can mean
            # something: a bare Xvfb has no window manager, GPUI's window
            # never becomes viewable, and XTest keys go nowhere. XSendEvent
            # keys are ignored by the XI2 host, so only XTest is tried; if
            # the delivery probe fails the visual checks stay MANUAL and
            # the helper exits 3 instead of passing anything falsely.
            x.focus(window)
            x.refocus()
            x.chord("Control", "l")
            time.sleep(0.3)
            x.type_text(str(fixture.alt))
            time.sleep(0.3)
            x.tap("Return")
            time.sleep(1.0)
            delivered = window_name(x, int(window, 16))
            if "alt-folder" not in delivered:
                manual(
                    "input focus cannot be established here (no window "
                    "manager: the fixture window stays unviewable and "
                    f"XTest keys do not reach it; title stayed {delivered!r}). "
                    "Run under a window-managed X11 fixture: then Escape "
                    "after a typed path must restore the committed path and "
                    "keep the prior folder after Enter"
                )
            print("PASS: keys reach the app; navigated to the alternate folder", flush=True)

            # Finding 1, path side: the now-committed alt folder; type the
            # home path again, Escape must cancel it (restore the committed
            # alt path), and Enter must then reload alt — the title must
            # return to alt-folder. Without the fix the untouched field
            # still holds the home path and Enter navigates home.
            x.chord("Control", "l")
            time.sleep(0.3)
            x.type_text(str(fixture.home))
            time.sleep(0.3)
            x.tap("Escape")
            time.sleep(0.3)
            x.tap("Return")
            time.sleep(1.0)
            title_after_cancel = window_name(x, int(window, 16))
            if delivered not in title_after_cancel:
                fail(
                    f"after Ctrl+L, home path, Escape, Enter the title is "
                    f"{title_after_cancel!r}, expected the prior folder "
                    f"{delivered!r}; Escape did not restore the committed path"
                )
            print(
                "PASS: valid path typed + Escape + Enter retains the prior folder",
                flush=True,
            )
            # Failed-submit retention: a bogus path errors out and leaves
            # the committed folder untouched through Escape and Enter.
            x.chord("Control", "l")
            time.sleep(0.3)
            x.type_text("no-such-folder")
            time.sleep(0.2)
            x.tap("Return")
            time.sleep(0.8)
            x.tap("Escape")
            time.sleep(0.3)
            x.tap("Return")
            time.sleep(1.0)
            if delivered not in window_name(x, int(window, 16)):
                fail("a failed path submit disturbed the committed folder")
            print("PASS: failed path submit + Escape + Enter retains the prior folder", flush=True)

            # Finding 1, search side: drive the actual repro (Ctrl+F, a
            # non-matching query, Escape) for the app's liveness, then
            # report the visible content restore as what it is: manual.
            x.chord("Control", "f")
            time.sleep(0.3)
            x.type_text("zz-nomatch")
            time.sleep(0.5)
            x.tap("Escape")
            time.sleep(0.5)
            if app.poll() is not None:
                fail(f"app died during the search drive: {app.returncode}")
            manual_report = True

        if app.poll() is not None:
            fail(f"app died during the drive: {app.returncode}")
    finally:
        if app is not None and app.poll() is None:
            app.terminate()
            try:
                app.wait(timeout=3)
            except subprocess.TimeoutExpired:
                app.kill()
                app.wait(timeout=3)
        if x is not None:
            x.close()
        fixture.cleanup()
    if manual_report:
        manual(
            "Ctrl+F 'no-match' Escape must visibly restore the listing; "
            "read the grid with an AT-SPI session or by eye — this helper "
            "cannot verify it and did not pretend to"
        )


if __name__ == "__main__":
    main()
