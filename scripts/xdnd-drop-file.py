#!/usr/bin/python3
"""Send one native Xdnd file drop to a target X11 window."""

from __future__ import annotations

import select
import struct
import sys
import time
from pathlib import Path

from Xlib import X, Xatom, display, protocol


def send_client_message(connection, target, atom, data):
    event = protocol.event.ClientMessage(
        window=target,
        client_type=atom,
        data=(32, data),
    )
    target.send_event(event, propagate=False, event_mask=0)
    connection.flush()


def client_message_fields(event):
    data = event.data
    if len(data) == 2 and data[0] in (8, 16, 32):
        data = data[1]
    if isinstance(data, bytes):
        return struct.unpack("=5I", data)
    return data


def main() -> int:
    if len(sys.argv) != 3:
        raise SystemExit("usage: xdnd-drop-file.py WINDOW_ID PATH")

    target_id = int(sys.argv[1], 0)
    path = Path(sys.argv[2]).resolve(strict=True)
    connection = display.Display()
    target = connection.create_resource_object("window", target_id)
    root = connection.screen().root
    source = root.create_window(
        -10,
        -10,
        1,
        1,
        0,
        X.CopyFromParent,
        X.InputOutput,
        X.CopyFromParent,
        event_mask=X.PropertyChangeMask,
    )
    source.map()

    atoms = {
        name: connection.intern_atom(name)
        for name in (
            "XdndEnter",
            "XdndPosition",
            "XdndDrop",
            "XdndStatus",
            "XdndFinished",
            "XdndSelection",
            "XdndTypeList",
            "XdndActionCopy",
            "text/uri-list",
        )
    }
    uri_type = atoms["text/uri-list"]
    source.change_property(atoms["XdndTypeList"], Xatom.ATOM, 32, [uri_type])
    source.set_selection_owner(atoms["XdndSelection"], X.CurrentTime)
    connection.flush()

    send_client_message(
        connection,
        target,
        atoms["XdndEnter"],
        [source.id, 5 << 24, uri_type, 0, 0],
    )
    coords = root.translate_coords(target, 320, 120)
    packed_coords = ((coords.x & 0xFFFF) << 16) | (coords.y & 0xFFFF)
    send_client_message(
        connection,
        target,
        atoms["XdndPosition"],
        [source.id, X.CurrentTime, packed_coords, X.CurrentTime, atoms["XdndActionCopy"]],
    )

    deadline = time.monotonic() + 10
    accepted = False
    selection_served = False
    drop_sent = False
    finished = False
    while time.monotonic() < deadline and not finished:
        if not connection.pending_events():
            select.select([connection.fileno()], [], [], 0.1)
            continue
        event = connection.next_event()
        if event.type == X.ClientMessage and event.client_type == atoms["XdndStatus"]:
            accepted = bool(client_message_fields(event)[1] & 1)
            if not accepted:
                raise RuntimeError("the GPUI window rejected the dropped file")
            if not drop_sent:
                send_client_message(
                    connection,
                    target,
                    atoms["XdndDrop"],
                    [source.id, X.CurrentTime, 0, 0, 0],
                )
                drop_sent = True
            continue
        if event.type == X.ClientMessage and event.client_type == atoms["XdndFinished"]:
            finished = bool(client_message_fields(event)[1] & 1)
            continue
        if event.type != X.SelectionRequest:
            continue

        property_atom = event.property or event.target
        event.requestor.change_property(
            property_atom,
            event.target,
            8,
            f"{path.as_uri()}\r\n".encode(),
            mode=X.PropModeReplace,
        )
        notification = protocol.event.SelectionNotify(
            time=event.time,
            requestor=event.requestor,
            selection=event.selection,
            target=event.target,
            property=property_atom,
        )
        event.requestor.send_event(notification, propagate=False, event_mask=0)
        connection.flush()
        selection_served = True

    connection.close()
    if not accepted:
        raise RuntimeError("the GPUI window did not accept the dropped file")
    if not selection_served:
        raise RuntimeError("the GPUI window did not request the dropped file URI")
    if not finished:
        raise RuntimeError("the GPUI window did not finish the dropped file operation")
    print("xdnd drop complete")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
