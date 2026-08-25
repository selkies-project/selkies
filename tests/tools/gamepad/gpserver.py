"""Serve one synthetic gamepad on `$SELKIES_JS_SOCKET_PATH` and drive events into
it, as the peer of the C enumeration/read tools alongside: prints `SERVER_UP` once
the sockets accept, then alternates button 0 and axis 1 for about thirty seconds."""
import asyncio
import os
import sys
sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))), "src"))
from selkies.input_handler import SelkiesGamepad

D = os.environ["SELKIES_JS_SOCKET_PATH"]

async def main():
    gp = SelkiesGamepad(os.path.join(D, "selkies_js0.sock"),
                        os.path.join(D, "selkies_event1000.sock"),
                        asyncio.get_running_loop())
    gp.set_config("Selkies Test Pad", 16, 4)
    asyncio.create_task(gp.run_servers())
    await asyncio.sleep(0.7)
    print("SERVER_UP", flush=True)
    for _ in range(300):
        gp.send_event(0, 1, True)
        await asyncio.sleep(0.05)
        gp.send_event(0, 0, True)
        gp.send_event(1, 0.5, False)
        await asyncio.sleep(0.05)

asyncio.run(main())
