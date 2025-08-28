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

from .webrtc_mode import wr_entrypoint
from .selkies import ws_entrypoint

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

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
	def __init__(self, applications: Dict):
		self.applications = applications
		self.current_app_name: Optional[str] = None
		self.current_task: Optional[asyncio.Task] = None
		self.lock = asyncio.Lock()

	async def switch_to_mode(self, app_name: str) -> Tuple[bool, str]:
		"""Switches to requested application task."""
		if not app_name:
			return False, "Application name cannot be empty."

		async with self.lock:
			if app_name not in self.applications:
				logger.error(f"Unknown application '{app_name}'. No change made.")
				return False, f"Unknown application '{app_name}'"

			if self.current_app_name == app_name and self.current_task and not self.current_task.done():
				logger.warning(f"Application '{app_name}' is already running.")
				return False, f"'{app_name}' is already running"

			logger.info(f"Request received to switch to '{app_name}'.")
			if self.current_task and not self.current_task.done():
				logger.info(f"Stopping '{self.current_app_name}'...")
				self.current_task.cancel()
				try:
					await self.current_task
				except asyncio.CancelledError:
					logger.info(f"Successfully stopped '{self.current_app_name}'.")

			logger.info(f"Starting application '{app_name}'...")
			self.current_app_name = app_name
			loop = asyncio.get_running_loop()
			self.current_task = loop.create_task(self.applications[app_name]())
			return True, f"Switched to '{app_name}'"

	def get_status(self):
		if self.current_task and not self.current_task.done():
			return {"running_app": self.current_app_name, "status": "running"}
		return {"running_app": None, "status": "stopped"}

async def create_api_server(manager: StreamSupervisor, host: str, port: int):
	"""Setups an aio http server and runs it"""
	app = web.Application()

	async def handle_switch(request):
		"""Handles the /switch endpoint."""
		data = await request.json()
		app_name = data.get("mode")
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

	site = web.TCPSite(runner, 'localhost', 8082)

	try:
		await site.start()
		logger.info(f"API server started on http://{host}:{port}")
		await asyncio.Future()  # Run forever untils cancelled
	except asyncio.CancelledError:
		pass
	except Exception as e:
		logger.info("Error occured in API server: {e}", exc_info=True)
	finally:
		logger.info("Shutting down API server.")
		await runner.cleanup()
		logger.info("API server shut down successfully.")

async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--mode', default=os.environ.get("SELKIES_MODE", "websockets"),
                        help="Specify the mode: 'webrtc' or 'websockets'; defaults to webrtc")
    args, _ = parser.parse_known_args()

    managed_applications = {
      "websockets": ws_entrypoint,
      "webrtc": wr_entrypoint,
    }

    manager = StreamSupervisor(managed_applications)
    api_task = asyncio.create_task(create_api_server(manager, host='localhost', port=8082))
    await manager.switch_to_mode(args.mode)

    try:
        await api_task
    except asyncio.CancelledError:
        logger.error("main interrupted, exiting the process")

if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        logger.info("Supervisor stopped by user")
