# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Session tokens and the input authority they carry, shared by both transports.

`user_tokens` is the live table `/api/tokens` provisions and `active_mk_token`
the holder of keyboard and mouse while one is named; every transport
authorizes input against them here, and a token update reaches live WebRTC
peers through `webrtc_reconcile_hook`, which the WebRTC service registers.
"""
import hmac
from typing import Any, Awaitable, Callable, Optional

from .settings import settings as app_settings

user_tokens: dict[str, dict] = {}
active_mk_token: Optional[str] = None

# The kernel gamepads a session presents; a token's slots are one-based among them.
GAMEPAD_SLOTS = 4


def token_slots(value: Any) -> list[int]:
    """The one-based gamepad slots a token entry's `slot` names, in its order.

    An entry names one slot (1 to GAMEPAD_SLOTS, or its decimal string), a list
    of them, or none (null, 0, or an empty list). The order is the page's: its
    first pad drives the first slot, its second the next. A value that is no
    such slot names none, and a slot named twice counts once.
    """
    slots: list[int] = []
    for item in (value if isinstance(value, (list, tuple)) else [value]):
        if isinstance(item, bool):
            continue
        try:
            slot = int(item)
        except (TypeError, ValueError):
            continue
        if 1 <= slot <= GAMEPAD_SLOTS and slot not in slots:
            slots.append(slot)
    return slots


def stored_slot(value: Any) -> Any:
    """A token entry's `slot` as the table keeps and pages are told it: null
    for none, the number for one, and the list for several (`token_slots`)."""
    slots = token_slots(value)
    if not slots:
        return None
    return slots[0] if len(slots) == 1 else slots


def current_session_tokens() -> tuple[dict[str, dict], Optional[str]]:
    """Live control-plane token view, as provisioned via /api/tokens.

    Returns:
        The `(user_tokens mapping, active mk-token)` pair. Both transports
        authorize input against this.
    """
    return user_tokens, active_mk_token


def _perms_hold_input_authority(perms: Optional[dict], token: Optional[str] = None) -> bool:
    """Apply the single input-authority rule shared by both transports.

    While an mk token is active only its holder may drive keyboard/mouse (and
    the commands and clipboard that ride the same gate); otherwise any
    controller-role client may.

    Args:
        perms: The client's permission entry (may be None or empty).
        token: Covers callers holding a user_tokens entry, which carries no
            token field of its own.
    """
    perms = perms or {}
    if active_mk_token is not None:
        return (perms.get("token") if token is None else token) == active_mk_token
    return perms.get("role", "viewer") == "controller"


def _mk_access_verdict(perms: Optional[dict], token: Optional[str] = None) -> bool:
    """The MK_ACCESS verdict a tokened websockets client is told.

    Input authority under the mk-token rule, with a viewer additionally held
    to enable_collab: a read-only viewer must not attach an input context
    whose every message the gate then drops. The same verdict WebRTC pushes
    at data-channel open.

    Args:
        perms: The client's permission entry (may be None or empty).
        token: Covers callers holding a user_tokens entry, which carries no
            token field of its own.
    """
    perms = perms or {}
    if perms.get("role") != "controller" and not bool(app_settings.enable_collab[0]):
        return False
    return _perms_hold_input_authority(perms, token=token)


def _lookup_session_token(token: Optional[str]) -> Optional[dict]:
    """The user_tokens entry for a session token, compared in constant time.

    Every provisioned token is compared (no early exit), so the reply timing
    does not tell a probing client how far its guess matched or which table
    entry it resembles.

    Returns:
        The token's permission entry, or None when it is not provisioned.
    """
    if not token:
        return None
    candidate = token.encode("utf-8")
    match = None
    for known, perms in user_tokens.items():
        if hmac.compare_digest(candidate, known.encode("utf-8")):
            match = perms
    return match


# Registered by the WebRTC service; the WebSockets reconcile walks only its own sockets.
webrtc_reconcile_hook: Optional[Callable[[], Awaitable[None]]] = None
