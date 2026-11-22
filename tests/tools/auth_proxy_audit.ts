/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The authenticated HTTPS proxy fixture keeps to what a deployment behind
 * one does (`tests/lib/authproxy.ts`).
 *
 * The audit runs it over real HTTPS requests and raw upgrade sockets against
 * a plain HTTP upstream of its own, with no server and no browser. What is
 * held: a request or an upgrade with no credentials, wrong ones, or another
 * scheme is denied with a challenge before the upstream sees a byte; a valid
 * request reaches the upstream with its method, path, query, body, and
 * headers, the proxy's credential stripped and the forwarding headers set
 * over any the client forged, and the response comes back with its status,
 * headers, and body; a header only the hop's `Connection` nominates crosses
 * in neither direction, and a chunked body's trailers cross in both; a valid
 * upgrade carries its protocol headers, its `Connection` and `Upgrade`, and the
 * bytes sent after them to the upstream, without the hop headers or what its
 * `Connection` nominates, and then relays both ways intact;
 * an upstream that cannot be reached is a 502 and an error the proxy
 * records; and stopping the proxy destroys every open socket and frees the
 * port.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */

import { createHash, randomBytes } from 'node:crypto';
import { request as httpRequest } from 'node:http';
import { createServer as createNetServer } from 'node:net';
import { AuthProxy } from '../lib/authproxy.ts';
import { Checks } from '../lib/checks.ts';
import {
    ask, basic, deadPort, denied, EARLY, GREETING, leaks, PUBLIC_HOST, pause, portOf, Raw, SETTLE, startUpstream, statusOf,
    type Upstream, until, upgrade, WAIT,
} from './auth_proxy_peer.ts';

const checks = new Checks('auth-proxy');

/** The hop's transfer-extensions request header: its name is escaped so that codespell does not read it as a typo. */
const TRANSFER_EXT = 'T\u0045';

/** Denials over HTTP: each credential refused with a challenge, the upstream untouched. */
async function auditHttpDenied(proxy: AuthProxy, up: Upstream): Promise<void> {
    const before = proxy.rejected;
    for (const { name, header } of denied(proxy)) {
        const answer = await ask(proxy, {
            method: 'POST', path: '/secret?x=1', headers: header === null ? {} : { Authorization: header }, body: Buffer.from('body'),
        });
        checks.check(`HTTP with ${name} is denied with a Basic challenge`,
            answer.status === 401 && /^Basic /i.test(String(answer.headers['www-authenticate'] ?? '')),
            `status ${answer.status}`);
    }
    await pause(SETTLE);
    checks.check('a denied HTTP request never reaches the upstream', up.seen.length === 0, `${up.seen.length} seen`);
    checks.check('every denial over HTTP is counted as rejected', proxy.rejected - before === denied(proxy).length, `${proxy.rejected - before} counted`);
}

/** A valid request over HTTP: forwarded whole, answered whole. */
async function auditHttpValid(proxy: AuthProxy, up: Upstream): Promise<void> {
    const { username, password } = proxy.httpCredentials;
    const body = randomBytes(256 * 1024);
    const answer = await ask(proxy, { method: 'POST', path: '/a/b%20c?x=1&y=%C3%A9', headers: {
        Authorization: basic(username, password), Host: PUBLIC_HOST, 'X-Custom': 'kept',
        'X-Forwarded-Proto': 'http', 'X-Forwarded-For': '203.0.113.9', 'Content-Type': 'application/octet-stream',
        'Content-Length': String(body.length),
    }, body });
    const got = up.seen[0];
    checks.check('a valid request reaches the upstream once', up.seen.length === 1 && got !== undefined, `${up.seen.length} seen`);
    if (got === undefined) return;
    checks.check('the method, path, and query are preserved', got.method === 'POST' && got.url === '/a/b%20c?x=1&y=%C3%A9', `${got.method} ${got.url}`);
    checks.check('the body arrives intact', got.body.equals(body), `${got.body.length} of ${body.length} bytes`);
    checks.check('the request headers are preserved', got.headers['x-custom'] === 'kept' && got.headers['content-type'] === 'application/octet-stream');
    checks.check('the public Host is preserved', got.headers.host === PUBLIC_HOST, String(got.headers.host));
    checks.check('the proxy credential is stripped', got.headers.authorization === undefined && !leaks(up, password));
    checks.check('X-Forwarded-Proto is https whatever the client sent', got.headers['x-forwarded-proto'] === 'https', String(got.headers['x-forwarded-proto']));
    checks.check('the forwarding headers name the loopback client and the public host',
        got.headers['x-forwarded-for'] === '127.0.0.1' && got.headers['x-forwarded-host'] === PUBLIC_HOST,
        `${String(got.headers['x-forwarded-for'])} ${String(got.headers['x-forwarded-host'])}`);
    const cookies = answer.headers['set-cookie'] ?? [];
    checks.check('the response status, headers, and body come back', answer.status === 201 && answer.headers['x-upstream'] === 'yes'
        && cookies.length === 2 && answer.body.equals(body), `status ${answer.status}, ${cookies.length} cookies, ${answer.body.length} bytes`);
    const missing = await ask(proxy, { method: 'GET', path: '/missing', headers: { Authorization: basic(username, password) } });
    checks.check('an upstream error status comes back as it is', missing.status === 404 && missing.body.toString() === 'nope', `status ${missing.status}`);
    checks.check('valid requests are counted', proxy.requests === 2, `${proxy.requests} counted`);
}

/** Denials of upgrades: each refused with a challenge and a close, the upstream untouched. */
async function auditUpgradeDenied(proxy: AuthProxy, up: Upstream): Promise<void> {
    const before = proxy.rejected;
    for (const { name, header } of denied(proxy)) {
        const raw = await upgrade(proxy, header);
        const closed = await until(() => raw.closed);
        checks.check(`an upgrade with ${name} is denied with a Basic challenge and a close`,
            statusOf(raw.head) === 401 && /^www-authenticate: Basic /im.test(raw.head ?? '') && closed, `status ${statusOf(raw.head)}, closed ${closed}`);
        await raw.shut();
    }
    await pause(SETTLE);
    checks.check('a denied upgrade never reaches the upstream', up.upgrades.length === 0 && up.seen.length === 2, `${up.upgrades.length} upgrades`);
    checks.check('every denial of an upgrade is counted as rejected', proxy.rejected - before === denied(proxy).length, `${proxy.rejected - before} counted`);
}

/**
 * A valid upgrade: protocol headers and head bytes carried, both directions intact.
 * @returns The open connection, for the stop audit to find destroyed.
 */
async function auditUpgradeValid(proxy: AuthProxy, up: Upstream): Promise<Raw> {
    const { username, password } = proxy.httpCredentials;
    const raw = await upgrade(proxy, basic(username, password), {
        Connection: 'Upgrade, X-Hop-Upgrade', 'X-Hop-Upgrade': 'leak', 'Keep-Alive': 'timeout=5', [TRANSFER_EXT]: 'trailers', Expect: '100-continue',
        'Proxy-Connection': 'keep-alive', 'X-Kept-Upgrade': 'kept',
        'X-Forwarded-For': '203.0.113.9', 'X-Forwarded-Host': 'forged.example', 'X-Forwarded-Proto': 'http',
    });
    checks.check('a valid upgrade is answered 101 with the upstream\'s protocol',
        statusOf(raw.head) === 101 && /^sec-websocket-protocol: selkies\.token\.audit, binary$/im.test(raw.head ?? ''), `status ${statusOf(raw.head)}`);
    const got = up.upgrades[0];
    checks.check('the upgrade reaches the upstream once', up.upgrades.length === 1 && got !== undefined, `${up.upgrades.length} upgrades`);
    if (got === undefined) return raw;
    const h = got.headers;
    checks.check('the upgrade keeps its protocol headers, host, and origin',
        h.upgrade === 'websocket' && h['sec-websocket-key'] === 'dGhlIHNhbXBsZSBub25jZQ==' && h['sec-websocket-version'] === '13'
        && h['sec-websocket-protocol'] === 'selkies.token.audit, binary' && h.host === PUBLIC_HOST && h.origin === 'https://selkies.example',
        String(h['sec-websocket-protocol']));
    checks.check('the upgrade has the proxy credential stripped and the forwarding headers set',
        h.authorization === undefined && !leaks(up, password) && h['x-forwarded-proto'] === 'https'
        && h['x-forwarded-for'] === '127.0.0.1' && h['x-forwarded-host'] === PUBLIC_HOST,
        `${String(h['x-forwarded-proto'])} ${String(h['x-forwarded-for'])}`);
    const hops = [h['x-hop-upgrade'], h['keep-alive'], h[TRANSFER_EXT.toLowerCase()], h.expect, h['proxy-connection']];
    checks.check('the upgrade drops the hop headers and what its Connection nominates, and keeps the end-to-end ones',
        hops.every((v) => v === undefined) && h['x-kept-upgrade'] === 'kept', JSON.stringify([...hops, h['x-kept-upgrade']]));
    checks.check('the upgrade keeps Connection and Upgrade as the client sent them, a usable WebSocket handshake',
        h.connection === 'Upgrade, X-Hop-Upgrade' && h.upgrade === 'websocket', `${String(h.connection)} ${String(h.upgrade)}`);
    const payload = randomBytes(128 * 1024);
    raw.socket.write(payload);
    const expected = Buffer.concat([GREETING, EARLY, payload]);
    const whole = await until(() => raw.rest.length >= expected.length);
    checks.check('the bytes that came with the head reach the upstream first', Buffer.concat(got.received).subarray(0, EARLY.length).equals(EARLY));
    checks.check('the upstream\'s bytes, the echoed head, and the echoed payload come back intact',
        whole && raw.rest.equals(expected), `${raw.rest.length} of ${expected.length} bytes`);
    checks.check('the payload reaches the upstream intact', Buffer.concat(got.received).equals(Buffer.concat([EARLY, payload])),
        `${Buffer.concat(got.received).length} bytes`);
    checks.check('a valid upgrade is counted', proxy.upgrades === 1, `${proxy.upgrades} counted`);
    checks.check('the audited flows raised no proxy or upstream errors', proxy.errors.length === 0 && up.errors.length === 0,
        [...proxy.errors, ...up.errors].join('; '));
    return raw;
}

/** Headers that describe only the hop do not cross it in either direction, and a chunked body's trailers do. */
async function auditHopsAndTrailers(proxy: AuthProxy, up: Upstream): Promise<void> {
    const { username, password } = proxy.httpCredentials;
    const auth = { Authorization: basic(username, password) };
    const hop = await ask(proxy, {
        method: 'GET', path: '/hops', headers: { ...auth, Connection: 'close, X-Hop-Request', 'X-Hop-Request': 'leak', 'X-Kept': 'yes' },
    });
    const seen = up.seen.at(-1);
    checks.check('a header the request\'s Connection nominates is not forwarded, the others are',
        seen?.url === '/hops' && seen.headers['x-hop-request'] === undefined && seen.headers['x-kept'] === 'yes',
        `x-hop-request ${String(seen?.headers['x-hop-request'])}`);
    checks.check('a header the response\'s Connection nominates is not returned, the others are',
        hop.status === 200 && hop.headers['x-hop-response'] === undefined && hop.headers['x-resp-kept'] === 'yes',
        `x-hop-response ${String(hop.headers['x-hop-response'])}`);
    const body = randomBytes(64 * 1024);
    const sent = await ask(proxy, {
        method: 'POST', path: '/trailers', body, trailers: { 'X-Req-Sum': 'from-client' },
        headers: { ...auth, 'Transfer-Encoding': 'chunked', Trailer: 'X-Req-Sum' },
    });
    const got = up.seen.at(-1);
    checks.check('a request\'s trailers reach the upstream with its declaration and body',
        got?.url === '/trailers' && got.trailers['x-req-sum'] === 'from-client' && got.headers.trailer === 'X-Req-Sum' && got.body.equals(body),
        `trailer ${String(got?.trailers['x-req-sum'])}`);
    checks.check('a response\'s trailers reach the client with its declaration and body',
        sent.trailers['x-sum'] === 'from-upstream' && sent.headers.trailer === 'X-Sum' && sent.body.equals(body),
        `trailer ${String(sent.trailers['x-sum'])}`);
}

/** An upstream that cannot be reached: a 502 over HTTP, a close for an upgrade, and an error recorded. */
async function auditUnreachable(): Promise<void> {
    const proxy = await AuthProxy.start(await deadPort());
    try {
        const { username, password } = proxy.httpCredentials;
        const answer = await ask(proxy, { method: 'GET', path: '/', headers: { Authorization: basic(username, password) } });
        checks.check('an unreachable upstream is a 502 over HTTP', answer.status === 502, `status ${answer.status}`);
        const raw = await upgrade(proxy, basic(username, password));
        const closed = await until(() => raw.closed);
        checks.check('an unreachable upstream closes an upgrade with a 502', statusOf(raw.head) === 502 && closed, `status ${statusOf(raw.head)}, closed ${closed}`);
        checks.check('both failures are recorded by the proxy', await until(() => proxy.errors.length >= 2), `${proxy.errors.length} errors`);
        await raw.shut();
    } finally {
        await proxy.stop();
    }
}

/** Stopping destroys every open socket, including an upgraded one, and frees the port. */
async function auditStop(): Promise<void> {
    const up = await startUpstream();
    const proxy = await AuthProxy.start(up.port);
    const port = portOf(proxy);
    try {
        const { username, password } = proxy.httpCredentials;
        const live = await upgrade(proxy, basic(username, password));
        const idle = await Raw.open(proxy);
        const ready = statusOf(live.head) === 101 && up.upgrades.length === 1;
        await Promise.race([proxy.stop(), pause(WAIT).then(() => { throw new Error('stop did not finish'); })]);
        checks.check('stop destroys the upgraded socket and the idle one',
            ready && await until(() => live.closed && idle.closed), `upgraded ${live.closed}, idle ${idle.closed}`);
        checks.check('stop closes the upstream side of an upgrade', await until(() => up.upgrades.every((u) => u.closed) && up.sockets.size === 0),
            `${up.sockets.size} upstream sockets`);
        const probe = createNetServer();
        const free = await new Promise<boolean>((resolve) => {
            probe.once('error', () => { resolve(false); });
            probe.listen(port, '127.0.0.1', () => { resolve(true); });
        });
        await new Promise<void>((resolve) => { probe.close(() => { resolve(); }); });
        checks.check('stop frees the port', free, `port ${port}`);
        checks.check('stopping twice is harmless', await proxy.stop().then(() => true, () => false));
    } finally {
        await proxy.stop();
        for (const s of up.sockets) s.destroy();
        await new Promise<void>((resolve) => { up.server.close(() => { resolve(); }); });
    }
}

/** The proxy's certificate and key are made in memory, and the credential is fresh per proxy. */
async function auditIdentity(proxy: AuthProxy): Promise<void> {
    const other = await AuthProxy.start(await deadPort());
    try {
        checks.check('the proxy listens on loopback over https', new URL(proxy.url).protocol === 'https:' && new URL(proxy.url).hostname === '127.0.0.1', proxy.url);
        checks.check('each proxy has a credential of its own', proxy.httpCredentials.password.length >= 16
            && proxy.httpCredentials.password !== other.httpCredentials.password
            && createHash('sha256').update(proxy.certificate).digest('hex') !== createHash('sha256').update(other.certificate).digest('hex'));
    } finally {
        await other.stop();
    }
}

/** Check that a plain HTTP request to the proxy's port is not served as HTTP. */
async function auditPlainRefused(proxy: AuthProxy, up: Upstream): Promise<void> {
    const before = up.seen.length;
    const outcome = await new Promise<string>((resolve) => {
        const req = httpRequest({ host: '127.0.0.1', port: portOf(proxy), path: '/', agent: false, signal: AbortSignal.timeout(WAIT) }, (res) => {
            res.resume();
            resolve(`served ${res.statusCode ?? 0}`);
        });
        req.on('error', () => { resolve('refused'); });
        req.end();
    });
    await pause(SETTLE);
    checks.check('a plain HTTP request to the proxy is never served or forwarded', outcome === 'refused' && up.seen.length === before, outcome);
}

const watchdog = setTimeout(() => {
    console.log('FAIL  [auth-proxy] the audit did not finish in time');
    process.exit(1);
}, 60_000);

const up = await startUpstream();
let proxy: AuthProxy | null = null;
let live: Raw | null = null;
try {
    proxy = await AuthProxy.start(up.port);
    await auditIdentity(proxy);
    await auditHttpDenied(proxy, up);
    await auditHttpValid(proxy, up);
    await auditUpgradeDenied(proxy, up);
    live = await auditUpgradeValid(proxy, up);
    await auditHopsAndTrailers(proxy, up);
    await auditPlainRefused(proxy, up);
    await auditUnreachable();
    await auditStop();
} catch (e) {
    checks.check('the audit ran to its end', false, e instanceof Error ? e.message : String(e));
} finally {
    await live?.shut();
    await proxy?.stop();
    for (const s of up.sockets) s.destroy();
    await new Promise<void>((resolve) => { up.server.close(() => { resolve(); }); });
    clearTimeout(watchdog);
}

process.exit(checks.summary().length ? 1 : 0);
