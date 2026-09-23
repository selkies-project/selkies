---
title: TURN
description: The coTURN container and the TURN-REST credential service for the opt-in WebRTC transport behind restrictive networks.
---

## coTURN

> Check the [WebRTC and Firewall Issues: coTURN](../firewall.md#coturn) section for installing and running coTURN on self-hosted standalone machines, cloud instances, or virtual machines. STUN/TURN is only relevant to the opt-in WebRTC transport.
>
> [Pion TURN](https://github.com/pion/turn)'s `turn-server-simple` executable or [eturnal](https://eturnal.net) are recommended alternative TURN server implementations that support Windows as well as Linux or MacOS. [STUNner](https://github.com/l7mp/stunner) is a Kubernetes native STUN and TURN deployment if Helm is possible to be used.

The [coTURN Container](https://github.com/selkies-project/selkies/tree/main/addons/coturn) is a reference container which provides the [coTURN](https://github.com/coturn/coturn) TURN server. Other than options including `-e TURN_SHARED_SECRET=`, `-e TURN_REALM=`, `-e TURN_PORT=`, `-e TURN_MIN_PORT=` (at least `49152`), and `-e TURN_MAX_PORT=` (at most `65535`), add more command-line options in `-e TURN_EXTRA_ARGS=`.

Run the Docker®/Podman container built from the [`coTURN Dockerfile`](https://github.com/selkies-project/selkies/tree/main/addons/coturn/Dockerfile) (**replace `main` to `latest` for the latest stable release**):

```bash
docker run --name coturn -it -d --rm -e TURN_SHARED_SECRET=n0TaRealCoTURNAuthSecretThatIsSixtyFourLengthsLongPlaceholdPlace -e TURN_REALM=example.com -e TURN_PORT=3478 -e TURN_MIN_PORT=65500 -e TURN_MAX_PORT=65535 -p 3478:3478 -p 3478:3478/udp -p 65500-65535:65500-65535 -p 65500-65535:65500-65535/udp ghcr.io/selkies-project/selkies/coturn:main
```

**The relay ports and the listening port must all be open to the internet.**

If the TURN relay port range is wide, it may take a very long time for the containers to start up. Simply using `--network=host` instead of specifying `-p 65500-65535:65500-65535` and `-p 65500-65535:65500-65535/udp` can also be plausible.

Modify the relay ports `-p 65500-65535:65500-65535` and `-p 65500-65535:65500-65535/udp` combined with `-e TURN_MIN_PORT=65500 -e TURN_MAX_PORT=65535` as appropriate (at least two relay ports are required per connection).

In addition, use the option `-e TURN_EXTRA_ARGS="--no-udp-relay"` if you cannot open the UDP `min-port=` to `max-port=` port ranges, or `-e TURN_EXTRA_ARGS="--no-tcp-relay"` if you cannot open the TCP `min-port=` to `max-port=` port ranges. Note that the `--no-udp-relay` option may not be supported with web browsers and may lead to the TURN server not working.

Consult the [WebRTC and Firewall Issues: TURN Server Authentication Methods](../firewall.md#turn-server-authentication-methods) and [TURN-REST](#turn-rest) sections for the difference between static auth secret/TURN REST API authentication and traditional long-term credential authentication.

## TURN-REST

**The below is an advanced concept likely required for multi-user WebRTC-mode environments.**

A TURN server is required with WebRTC when both the host and the client are under Symmetric NAT or are each under Port Restricted Cone NAT and Symmetric NAT.

In easier words, if both the host and client are behind restrictive firewalls, the web interface and signaling connection (delivered using HTTP(S) and WebSocket) are delivered and established, but the WebRTC video and audio stream does not establish. In this case, the TURN server relays the WebRTC stream so that the host and client can send the video and audio stream, as well as other data.

![TURN-REST.svg](../assets/TURN-REST.svg)

The recommended multi-user TURN server authentication mechanism is the [time-limited short-term credential/TURN REST API mechanism](https://datatracker.ietf.org/doc/html/draft-uberti-behave-turn-rest-00), where there is a single [shared secret](https://github.com/coturn/coturn/blob/master/README.turnserver) that is never exposed externally (only the TURN-REST Container and the coTURN TURN server know), but instead authenticates WebRTC clients (which are Selkies hosts and clients) based on generated credentials which are valid for only a short time (typically 24 hours).

The [TURN-REST Container](https://github.com/selkies-project/selkies/tree/main/addons/turn-rest) is an easy way to distribute short-term TURN server authentication credentials and the information of the TURN server based on the REST API to many Selkies host instances, particularly when behind a local area network (LAN), which may or may not have restricted firewalls.

Using the `selkies --turn-rest-uri=` option or `SELKIES_TURN_REST_URI` environment variable, the Selkies host periodically queries a URI such as `https://turn-rest.myinfrastructure.io/myturnrest` or `http://192.168.0.10/myturnrest`.

This URI is ideally behind a local area network (LAN) inaccessible from the outside and only accessible to the Python hosts inside the LAN, or alternatively behind authentication using any web server or reverse proxy, if accessible from the outside. This information is periodically sent to the web client (that is also preferably behind authentication with HTTP Basic Authentication or a web server/reverse proxy) through HTTP(S), thus the TURN server information and credentials being propagated to both the Python host and the web client without exposing the TURN server information outside.

Because the time-limited TURN credentials automatically expire after some time, they are not useful even if they are leaked outside, as long as the pathway to the air-gapped or authenticated TURN-REST Container REST HTTP endpoint is not exposed plainly to the internet. [app.py](https://github.com/selkies-project/selkies/tree/main/addons/turn-rest/app.py) may also be hosted standalone without a container using the same startup command in the [Dockerfile](https://github.com/selkies-project/selkies/tree/main/addons/turn-rest/Dockerfile), whose [gunicorn configuration](https://github.com/selkies-project/selkies/tree/main/addons/turn-rest/gunicorn.conf.py) binds the wildcard address, which is what opens it to other hosts; run directly with `python3 app.py`, the development server listens on the loopback addresses only (`127.0.0.1,::1`), at the addresses and port `TURN_REST_ADDR` (`0.0.0.0,::` for every interface) and `TURN_REST_PORT` name.

Other authentication methods such as TURN-REST over various types of REST API authentication (but adding support for TURN-REST behind Basic Authentication is trivial, so reach out with some funding) or TURN oAuth authentication are not supported as of now, and likely requires funding.

The TURN-REST Container (or similarly, Kubernetes Pod) should be triggered with the Docker®/Podman options `-e TURN_SHARED_SECRET=`, `-e TURN_HOST=`, `-e TURN_PORT=`, `-e TURN_PROTOCOL=`, `-e TURN_TLS=`, `-e STUN_HOST=`, `-e STUN_PORT=`, where the options are dependent on the TURN server configuration of [coTURN](#coturn) or other TURN server implementations.

Run the Docker®/Podman container built from the [`TURN-REST Dockerfile`](https://github.com/selkies-project/selkies/tree/main/addons/turn-rest/Dockerfile) (**replace `main` to `latest` for the latest stable release**):

```bash
docker run --name turn-rest -it -d --rm -e TURN_SHARED_SECRET=n0TaRealCoTURNAuthSecretThatIsSixtyFourLengthsLongPlaceholdPlace -e TURN_HOST=turn.myinfrastructure.io -e TURN_PORT=3478 -e TURN_PROTOCOL=udp -e TURN_TLS=false -p 8008:8008 ghcr.io/selkies-project/selkies/turn-rest:main
```

From Selkies, it is sufficient to use the `selkies --turn-rest-uri=` option or `export SELKIES_TURN_REST_URI=` environment variable, pointing to the HTTP(S) URI to the TURN REST API server.

Consult the [WebRTC and Firewall Issues: TURN Server Authentication Methods](../firewall.md#turn-server-authentication-methods) section for more information on TURN authentication methods.
