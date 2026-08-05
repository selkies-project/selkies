# -*- coding: utf-8 -*-
"""Minimal TCP <-> Unix stream reverse proxy (stand-in for an nginx/forwarder)."""
import asyncio
import sys


async def pipe(reader, writer):
    try:
        while True:
            data = await reader.read(65536)
            if not data:
                break
            writer.write(data)
            await writer.drain()
    except Exception:
        pass
    finally:
        try:
            writer.close()
        except Exception:
            pass


async def handle(reader, writer, sock_path):
    try:
        ureader, uwriter = await asyncio.open_unix_connection(sock_path)
    except Exception:
        writer.close()
        return
    try:
        await asyncio.gather(
            pipe(reader, uwriter),
            pipe(ureader, writer),
        )
    finally:
        try:
            writer.close()
        except Exception:
            pass


async def main(host, port, sock_path):
    server = await asyncio.start_server(
        lambda r, w: handle(r, w, sock_path), host, port)
    print(f"tcp2unix proxy on {host}:{port} -> {sock_path}", flush=True)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main(sys.argv[1], int(sys.argv[2]), sys.argv[3]))
