# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import argparse
import sys
import os
import asyncio
import logging
from aiohttp import web
from typing import Dict, Optional, Tuple

from settings import settings_webrtc as settings

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

async def start_websockets_mode():
    from .selkies import ws_entrypoint
    await ws_entrypoint()

async def start_webrtc_mode():
    from .webrtc_mode import wr_entrypoint
    await wr_entrypoint()

if __name__ == "__main__" and __package__ is None:
    current_script_dir = os.path.dirname(os.path.abspath(__file__))
    package_container_dir = os.path.dirname(current_script_dir)
    if package_container_dir not in sys.path:
        sys.path.insert(0, package_container_dir)

class StreamSupervisor:
	"""
	Manages the lifecycle of application tasks.
	The applications it can manage are injected during initialization.
	"""
	def __init__(self, stream_mods: Dict):
		self.stream_modes = stream_mods
		self.current_streaming_mode: Optional[str] = None
		self.current_task: Optional[asyncio.Task] = None
		self.lock = asyncio.Lock()

	async def switch_to_mode(self, mode: str) -> Tuple[bool, str]:
		"""Switches to requested application task."""
		if not mode:
			return False, "Stream mode name cannot be empty."

		async with self.lock:
			if mode not in self.stream_modes:
				return False, f"Unknown stream mode '{mode}'"

			if self.current_streaming_mode == mode and self.current_task and not self.current_task.done():
				logger.warning(f"Stream mode '{mode}' is already running.")
				return False, f"'{mode}' mode is already running"

			if self.current_task and not self.current_task.done():
				logger.info(f"Stopping '{self.current_streaming_mode}'...")
				self.current_task.cancel()
				try:
					await asyncio.wait_for(self.current_task, timeout=2)
				except asyncio.CancelledError:
					logger.info(f"Successfully stopped '{self.current_streaming_mode}'.")
				except asyncio.TimeoutError:
					logger.warning(f"Timeout while stopping '{self.current_streaming_mode}'.")

			logger.info(f"Starting mode '{mode}'...")

			self.current_streaming_mode = mode
			loop = asyncio.get_running_loop()
			self.current_task = loop.create_task(self.stream_modes[mode]())
			return True, f"Switched to '{self.current_streaming_mode}' mode"

	def get_status(self):
		if self.current_task and not self.current_task.done():
			return {"current_mode": self.current_streaming_mode, "status": "running"}
		return {"current_mode": None, "status": "stopped"}

async def create_api_server(manager: StreamSupervisor, host: str, port: int):
	"""Setups an aio http server and runs it"""
	app = web.Application()

	async def handle_switch(request):
		"""Handles the /switch endpoint."""
		# Check if dual mode is enabled
		if not settings.enable_dual_mode[0]:
			return web.json_response({"error": "Can't switch to the requested mode. Mode switching is disabled."}, status=403)

		data = await request.json()
		app_name = data.get("mode")
		logger.info(f"Received request to switch to '{app_name}'.")

		success, message = await manager.switch_to_mode(app_name)
		if success:
			return web.json_response({"message": message})
		else:
			return web.json_response({"error": message}, status=409)

	async def handle_status(request):
		return web.json_response(manager.get_status())

	app.add_routes([
		web.post('/switch', handle_switch),
		web.get('/status', handle_status)
	])
	runner = web.AppRunner(app)
	await runner.setup()

	site = web.TCPSite(runner, 'localhost', port)

	try:
		await site.start()
		logger.info(f"API server running at http://{host}:{port}")
		await asyncio.Future()  # Run forever untils cancelled
	except asyncio.CancelledError:
		pass
	except Exception as e:
		logger.info("Error occured in API server: {e}", exc_info=True)
	finally:
		await runner.cleanup()
		logger.info("API server shut down successfully.")

async def run():
    mode = getattr(settings, "mode", None)
    if mode not in ["websockets", "webrtc"]:
        logger.error(f"Invalid mode '{mode}' specified in settings. Choose 'websockets' or 'webrtc'.")
        return

    managed_stream_modes = {
      "websockets": start_websockets_mode,
      "webrtc": start_webrtc_mode,
    }

    manager = StreamSupervisor(managed_stream_modes)
    api_task = None
    if settings.enable_dual_mode[0]:
        api_task = asyncio.create_task(create_api_server(manager, host='localhost', port=8082))
        logger.info("Dual mode enabled: Supervisor API server started on port 8082")
    await manager.switch_to_mode(mode)
    logger.info(f"Starting Selkies in '{mode}' mode.")
    try:
        if api_task:
            await api_task
        elif manager.current_task:
            await manager.current_task
    except asyncio.CancelledError:
        logger.error("run interrupted, exiting the process")
    except Exception as e:
        logger.error(f"Unexpected error in run: {e}", exc_info=True)

def main():
	try:
		asyncio.run(run())
	except KeyboardInterrupt:
		logger.info("Selkies Supervisor interrupted, exiting...")

if __name__ == "__main__":
	main()
