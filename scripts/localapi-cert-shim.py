#!/usr/bin/env python3
"""LocalAPI certificate shim for drills (docs/m2-implementation-plan.md).

Listens on a Unix socket and forwards every LocalAPI request to the real
tailscaled socket unchanged, except `GET /localapi/v0/cert/<domain>`, which
it answers with a key and certificate pair read from a file (the leaf of a
private drill CA). `portal serve` pointed at this socket through
DISTR_HNSW_TAILSCALE_SOCKET then binds the real tailnet address and resolves
real WhoIs identities, without a public certificate issuance and without
root. Each request is logged to stderr as `cert <path>` or `forward <path>`.

Drill use only: the pair file is served to any process that can reach the
listening socket, which is created mode 0600.

Usage: localapi-cert-shim.py LISTEN_SOCKET UPSTREAM_SOCKET PAIR_PEM
"""

import asyncio
import os
import sys

CERT_PREFIX = "/localapi/v0/cert/"


async def pipe(reader, writer):
    try:
        while data := await reader.read(65536):
            writer.write(data)
            await writer.drain()
    except ConnectionError:
        pass
    finally:
        writer.close()


async def handle(reader, writer, upstream, pair_path):
    try:
        head = await reader.readuntil(b"\r\n\r\n")
    except (asyncio.IncompleteReadError, asyncio.LimitOverrunError):
        writer.close()
        return
    method, _, rest = head.split(b"\r\n", 1)[0].decode("latin-1").partition(" ")
    path = rest.rsplit(" ", 1)[0]
    if method == "GET" and path.startswith(CERT_PREFIX):
        print(f"cert {path}", file=sys.stderr, flush=True)
        with open(pair_path, "rb") as pair:
            body = pair.read()
        writer.write(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n"
            + f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode()
            + body
        )
        await writer.drain()
        writer.close()
        return
    print(f"forward {path}", file=sys.stderr, flush=True)
    upstream_reader, upstream_writer = await asyncio.open_unix_connection(upstream)
    upstream_writer.write(head)
    await asyncio.gather(pipe(reader, upstream_writer), pipe(upstream_reader, writer))


async def main(listen, upstream, pair_path):
    os.umask(0o077)
    server = await asyncio.start_unix_server(
        lambda reader, writer: handle(reader, writer, upstream, pair_path), path=listen
    )
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    if len(sys.argv) != 4:
        sys.exit(__doc__.strip().splitlines()[-1])
    asyncio.run(main(*sys.argv[1:]))
