"""Gunicorn settings for the TURN REST container.

The published port is the container's boundary, so the service listens on
every interface of both families through the IPv6 wildcard, which is
dual-stack on Linux and takes IPv4 connections too; gunicorn sets no
IPV6_V6ONLY, so a `0.0.0.0` socket beside it would collide on the port. A
kernel without IPv6 gets the IPv4 wildcard instead.
"""
import socket

try:
    socket.socket(socket.AF_INET6).close()
    bind = "[::]:8008"
except OSError:
    bind = "0.0.0.0:8008"
