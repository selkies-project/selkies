/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Where an image under test runs, and how the tier reaches into it.
//
// A target is a published desktop image brought up by tag or digest, as a
// Kubernetes pod or a Docker container (on this host or over ssh), or a session
// someone already runs. The tier reaches it two ways: a browser opens its HTTPS
// port, and `sh` runs a shell command inside it as the session user, with the
// session's display, D-Bus, and runtime directory exported, which is how the
// checks act on the desktop and read what the server and the session saw.
//
//     E2E_IMAGE_WHERE   kube (default), docker, ssh:HOST, or url:https://HOST:PORT
//     E2E_IMAGE_EXEC    url targets: the command prefix that runs `bash -c` inside
//                       the session (`docker exec -i NAME`, `kubectl exec -i POD --`)
//     E2E_IMAGE_LOGS    url targets: the command printing the server's log, given --tail=N
//                       (`kubectl logs POD`, `docker logs NAME`)
//     E2E_IMAGE_REACH   kube targets: `forward` reaches the pod through kubectl port-forward
//                       (a runner outside the cluster) rather than at its own address
//     E2E_IMAGE_KUBECTL the kubectl command (default `kubectl`), for a runner whose
//                       HOME is not the one holding the kubeconfig and its tokens
import { spawn, type ChildProcess } from 'node:child_process';
import { accessSync, constants, statSync } from 'node:fs';
import { createServer, connect } from 'node:net';
import { delimiter, isAbsolute, join, resolve } from 'node:path';
import { Agent, request } from 'node:https';

import { fields, isRecord, parseJson, stringOr } from '../../web/selkies-web-core/lib/narrow.ts';
import { sleep } from '../lib/env.ts';

// The desktop's own environment, all of it, as an application started from its
// shell or panel gets it: a client of whichever compositor the session runs (so
// its WAYLAND_DISPLAY is the one apps use), with the LD_PRELOAD interposers that
// hand apps the session's gamepads and webcam, and the toolkit settings a
// browser started without them can die on. The images' defaults stand where no
// such process runs yet.
const DESKTOP_PROCS = 'plasmashell selkies-desktop lxqt-panel xfce4-panel pcmanfm-qt mate-panel gnome-shell kwin_x11 kwin_wayland labwc openbox';
const SESSION_ENV = 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-$(id -un)}"; '
    + `for n in ${DESKTOP_PROCS}; do p=$(pgrep -u "$(id -u)" -x -o "$n") || continue; `
    + 'while IFS= read -r -d "" kv; do case "$kv" in PWD=*|OLDPWD=*|SHLVL=*|_=*) ;; *) export "$kv";; esac; '
    + 'done < /proc/$p/environ; break; done 2>/dev/null; '
    + 'export DISPLAY="${DISPLAY:-:20}"; ';

// The images serve HTTPS on a certificate of their own making.
const NO_VERIFY = new Agent({ rejectUnauthorized: false });

/** A finished command: its exit status (124 on a timeout) and output. */
export interface Ran {
    code: number;
    stdout: Buffer;
    stderr: Buffer;
}

/**
 * An executable file `name` names, found on PATH by name, by its absolute path.
 * @throws {Error} When it is not one.
 */
function executable(name: string): string {
    const candidates = isAbsolute(name) || name.includes('/')
        ? [resolve(name)]
        : (process.env.PATH ?? '').split(delimiter).filter(Boolean).map((dir) => join(dir, name));
    for (const path of candidates) {
        try {
            if (statSync(path).isFile()) {
                accessSync(path, constants.X_OK);
                return path;
            }
        } catch {
            // the next one
        }
    }
    throw new Error(`'${name}' is not an executable file`);
}

/**
 * Run a local command, capturing bytes; a timeout reads as exit 124.
 * @param cmd The command and its arguments.
 * @param timeoutS How long it may take.
 * @param data What it reads on its input.
 */
export function run(cmd: readonly string[], timeoutS = 60, data?: Buffer): Promise<Ran> {
    // Run by its path, so a link to a multi-call binary keeps its name.
    const child = spawn(executable(cmd[0]), cmd.slice(1), { stdio: ['pipe', 'pipe', 'pipe'] });
    const out: Buffer[] = [];
    const err: Buffer[] = [];
    child.stdout.on('data', (b: Buffer) => { out.push(b); });
    child.stderr.on('data', (b: Buffer) => { err.push(b); });
    child.stdin.on('error', () => { /* a command that reads nothing */ });
    child.stdin.end(data ?? Buffer.alloc(0));
    return new Promise((resolve) => {
        let timedOut = false;
        const timer = setTimeout(() => { timedOut = true; child.kill('SIGKILL'); }, timeoutS * 1000);
        child.once('close', (code) => {
            clearTimeout(timer);
            resolve({ code: timedOut ? 124 : (code ?? 1), stdout: Buffer.concat(out), stderr: Buffer.concat(err) });
        });
        child.once('error', () => {
            clearTimeout(timer);
            resolve({ code: 127, stdout: Buffer.concat(out), stderr: Buffer.concat(err) });
        });
    });
}

/**
 * The command `variable` names (`fallback` where unset): its first word an
 * executable file, found on PATH by name, and the rest words of letters,
 * digits, and `_./:=@,+-`.
 * @throws {Error} When it is not one.
 */
export function commandFrom(variable: string, fallback: string): string[] {
    const words = (process.env[variable] || fallback).split(/\s+/).filter(Boolean);
    if (!words.length) throw new Error(`${variable} names no command`);
    const rest = words.slice(1);
    if (!rest.every((w) => /^[A-Za-z0-9_./:=@,+-]+$/.test(w))) {
        throw new Error(`${variable}: ${JSON.stringify(rest)} holds a word outside letters, digits, and _./:=@,+-`);
    }
    return [executable(words[0]), ...rest];
}

/** A running session: its URL, and a shell inside it. */
export abstract class Target {
    name = '';
    url = '';
    image = '';
    digest = '';
    node = '';
    protected execPrefix: string[] = [];

    abstract up(timeoutS?: number): Promise<void>;

    /** Run `script` with bash inside the session, as its user. */
    sh(script: string, timeoutS = 60, data?: Buffer): Promise<Ran> {
        return run([...this.execPrefix, 'bash', '-c', SESSION_ENV + script], timeoutS, data);
    }

    /** `sh`'s stdout as text, stripped. */
    async out(script: string, timeoutS = 60): Promise<string> {
        return (await this.sh(script, timeoutS)).stdout.toString().trim();
    }

    /** Write `data` to `path` inside the session. */
    async put(path: string, data: Buffer): Promise<boolean> {
        const q = `'${path.replaceAll("'", "'\\''")}'`;
        return (await this.sh(`mkdir -p $(dirname ${q}) && cat > ${q}`, 120, data)).code === 0;
    }

    /** GET (or POST `body` as JSON to) one of the server's routes; null on failure. */
    api(path: string, body?: unknown, timeoutS = 10): Promise<unknown> {
        return new Promise((resolve) => {
            let url: URL;
            try {
                url = new URL(this.url + path);
            } catch {
                resolve(null);
                return;
            }
            const req = request(url, {
                method: body === undefined ? 'GET' : 'POST', agent: NO_VERIFY,
                headers: { 'Content-Type': 'application/json' }, timeout: timeoutS * 1000,
            }, (res) => {
                const chunks: Buffer[] = [];
                res.on('data', (b: Buffer) => { chunks.push(b); });
                res.on('end', () => {
                    if (!res.statusCode || res.statusCode >= 400) {
                        resolve(null);
                        return;
                    }
                    const raw = Buffer.concat(chunks).toString();
                    resolve(raw.startsWith('{') || raw.startsWith('[') ? parseJson(raw) : raw);
                });
                res.on('error', () => { resolve(null); });
            });
            req.on('timeout', () => { req.destroy(); });
            req.on('error', () => { resolve(null); });
            req.end(body === undefined ? undefined : JSON.stringify(body));
        });
    }

    /** Until /api/status answers. */
    async waitReady(timeoutS = 900): Promise<boolean> {
        for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
            if (isRecord(await this.api('/api/status'))) return true;
            await sleep(3000);
        }
        return false;
    }

    /** The tail of what the image's supervisor logged (the server's own lines among it). */
    abstract selkiesLog(lines?: number): Promise<string>;

    /** Whether the server still answers (a preempted pod's does not). */
    async alive(): Promise<boolean> {
        return isRecord(await this.api('/api/status'));
    }

    down(): Promise<void> {
        return Promise.resolve();
    }
}

/** A Kubernetes pod's settings beside its image, name and environment. */
export interface KubeOptions {
    namespace?: string;
    labels?: Record<string, string>;
    gpu?: string;
    affinity?: Record<string, unknown> | null;
    cpu?: string;
    memory?: string;
}

/**
 * The image as a pod, which the browser reaches at its IP from inside the
 * cluster (or through a port-forward, see `reach`).
 */
export class KubePod extends Target {
    private readonly ns: string;
    private readonly labels: Record<string, string>;
    private readonly gpu: string;
    private readonly affinity: Record<string, unknown> | null;
    private readonly cpu: string;
    private readonly memory: string;
    private readonly kc: string[];
    private forward: ChildProcess | null = null;
    private avoid: string[] = [];
    private groups: number[] = [];
    private readonly env: Record<string, string>;

    constructor(image: string, name: string, env: Record<string, string>, options: KubeOptions = {}) {
        super();
        this.env = env;
        this.image = image;
        this.name = name;
        this.ns = options.namespace || process.env.E2E_IMAGE_NAMESPACE || 'default';
        if (!/^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?$/.test(this.ns)) {
            throw new Error(`E2E_IMAGE_NAMESPACE=${JSON.stringify(this.ns)} is not a namespace name`);
        }
        this.labels = { app: name, ...options.labels };
        this.gpu = options.gpu ?? 'none';
        this.affinity = options.affinity ?? null;
        this.cpu = options.cpu ?? '4';
        this.memory = options.memory ?? '8Gi';
        this.kc = [...commandFrom('E2E_IMAGE_KUBECTL', 'kubectl'), '-n', this.ns];
        this.execPrefix = [...this.kc, 'exec', '-i', name, '--'];
    }

    manifest(): Record<string, unknown> {
        const limits: Record<string, string> = { cpu: String(Number.parseInt(this.cpu, 10) * 2), memory: this.memory };
        if (this.gpu.startsWith('nvidia')) limits['nvidia.com/gpu'] = '1';
        else if (this.gpu === 'amd') limits['amd.com/gpu'] = '1';
        const requests = { ...limits, cpu: this.cpu };
        const priority = process.env.E2E_IMAGE_PRIORITY_CLASS;
        const spec: Record<string, unknown> = {
            restartPolicy: 'Never',
            terminationGracePeriodSeconds: 10,
            ...(priority ? { priorityClassName: priority } : {}),
            containers: [{
                name: 'desktop', image: this.image, imagePullPolicy: 'IfNotPresent',
                env: Object.entries(this.env).map(([name, value]) => ({ name, value })),
                ports: [{ containerPort: 8080 }],
                resources: { requests, limits },
                volumeMounts: [{ name: 'dshm', mountPath: '/dev/shm' }],
            }],
            volumes: [{ name: 'dshm', emptyDir: { medium: 'Memory', sizeLimit: '2Gi' } }],
        };
        const affinity = fields(parseJson(JSON.stringify(this.affinity ?? {})));
        if (this.avoid.length) {
            // A node already tried and unreachable from here is left out (ANDed into every term).
            const node = fields(affinity.nodeAffinity);
            const required = fields(node.requiredDuringSchedulingIgnoredDuringExecution);
            const given: unknown = required.nodeSelectorTerms;
            const terms: Record<string, unknown>[] = Array.isArray(given) && given.length
                ? given.map((t: unknown) => fields(t))
                : [{ matchExpressions: [] }];
            for (const term of terms) {
                const listed: unknown = term.matchExpressions;
                const expressions: unknown[] = Array.isArray(listed) ? listed.map((e: unknown) => e) : [];
                term.matchExpressions = [...expressions, { key: 'kubernetes.io/hostname', operator: 'NotIn', values: this.avoid }];
            }
            required.nodeSelectorTerms = terms;
            node.requiredDuringSchedulingIgnoredDuringExecution = required;
            affinity.nodeAffinity = node;
        }
        if (Object.keys(affinity).length) spec.affinity = affinity;
        if (this.groups.length) spec.securityContext = { supplementalGroups: this.groups };
        return { apiVersion: 'v1', kind: 'Pod', metadata: { name: this.name, labels: this.labels }, spec };
    }

    async status(): Promise<Record<string, unknown>> {
        const p = await run([...this.kc, 'get', 'pod', this.name, '-o', 'json'], 30);
        return p.code === 0 ? fields(parseJson(p.stdout.toString())) : {};
    }

    /**
     * Delete the pod and wait until the cluster no longer has it: one still
     * terminating would take the next apply in its place and then vanish.
     */
    private async deleteGone(timeoutS = 200): Promise<void> {
        await run([...this.kc, 'delete', 'pod', this.name, '--ignore-not-found', '--wait=true', `--timeout=${timeoutS}s`], timeoutS + 20);
        for (const until = Date.now() + timeoutS * 1000; Date.now() < until;) {
            const p = await run([...this.kc, 'get', 'pod', this.name, '--ignore-not-found', '-o', 'name'], 30);
            if (p.code === 0 && !p.stdout.toString().trim()) return;
            await sleep(2000);
        }
    }

    private async nodeName(): Promise<string> {
        return stringOr(fields((await this.status()).spec).nodeName) ?? '';
    }

    /**
     * Create the pod and wait for its server; a node this host cannot exec
     * into or reach is left out of the next try.
     */
    async up(timeoutS = 1800, tries = 6): Promise<void> {
        this.avoid = [];
        // A pod left by a run that was killed carries its state; every run starts from the image.
        await this.deleteGone(180);
        for (let i = 0; i < tries; i += 1) {
            const why = await this.tryUp(timeoutS);
            if (why === null) return;
            const node = await this.nodeName();
            process.stdout.write(`      (pod ${this.name} on ${node || 'no node'}: ${why})\n`);
            await this.deleteGone(120);
            if (node) this.avoid.push(node);
        }
        throw new Error(`pod ${this.name} did not come up on ${JSON.stringify(this.avoid)}`);
    }

    /** Apply the pod and wait for its server; null once it answers, else why it did not. */
    private async tryUp(timeoutS: number): Promise<string | null> {
        const p = await run([...this.kc, 'apply', '-f', '-'], 60, Buffer.from(JSON.stringify(this.manifest())));
        if (p.code !== 0) throw new Error(`kubectl apply: ${p.stderr.toString().slice(-300)}`);
        const deadline = Date.now() + timeoutS * 1000;
        while (Date.now() < deadline) {
            const pod = await this.status();
            // Preempted or evicted before it ran: the caller tries again.
            if (!Object.keys(pod).length) return 'gone before it ran (preempted or evicted)';
            const st = fields(pod.status);
            const podIP = stringOr(st.podIP);
            if (st.phase === 'Running' && podIP) {
                const cs = fields(Array.isArray(st.containerStatuses) ? st.containerStatuses[0] : undefined);
                this.digest = (stringOr(cs.imageID) ?? '').split('@').at(-1) ?? '';
                this.node = await this.nodeName();
                const exec = await this.sh('true', 45);
                if (exec.code !== 0) return `no shell inside it (exit ${exec.code}: ${exec.stderr.toString().trim().slice(-200)})`;
                if (this.gpu !== 'none' && !this.groups.length) {
                    // A render node the host keeps to a group the session user is not in, which Docker's
                    // --group-add covers: the pod comes up again in that group, or every encoder is software.
                    const gids = (await this.out('stat -c %g /dev/dri/renderD* 2>/dev/null; echo :; id -G')).split(':');
                    const have = new Set((gids.at(-1) ?? '').split(/\s+/).filter(Boolean));
                    const missing = [...new Set(gids[0].split(/\s+/).filter(Boolean))].filter((g) => !have.has(g)).sort();
                    if (missing.length) {
                        this.groups = missing.map(Number);
                        await this.deleteGone(120);
                        return this.tryUp(Math.max(60, (deadline - Date.now()) / 1000));
                    }
                }
                this.url = await this.reach(podIP);
                if (!this.url) return `its address ${podIP} cannot be reached from here`;
                return await this.waitReady(Math.max(60, Math.min(900, (deadline - Date.now()) / 1000))) ? null : `${this.url} never answered /api/status`;
            }
            if (st.phase === 'Failed' || st.phase === 'Succeeded') return `the pod ${String(st.phase)}`;
            await sleep(5000);
        }
        return 'not running before the deadline';
    }

    /**
     * The pod's own address (a refusal is the server still starting); '' when
     * the cluster routes nowhere from here to that node, which the caller then
     * leaves out. With E2E_IMAGE_REACH=forward, a local port-forward instead,
     * for a runner outside the cluster (WebRTC then needs a TURN server).
     */
    private async reach(ip: string): Promise<string> {
        if (process.env.E2E_IMAGE_REACH !== 'forward') {
            const reached = await new Promise<boolean>((resolve) => {
                const socket = connect({ host: ip, port: 8080, timeout: 5000 });
                socket.once('connect', () => { socket.destroy(); resolve(true); });
                socket.once('timeout', () => { socket.destroy(); resolve(false); });
                socket.once('error', (e: NodeJS.ErrnoException) => { resolve(e.code === 'ECONNREFUSED'); });
            });
            return reached ? `https://${ip.includes(':') ? `[${ip}]` : ip}:8080` : '';
        }
        const port = await new Promise<number>((resolve, reject) => {
            const server = createServer();
            server.listen(0, '127.0.0.1', () => {
                const address = server.address();
                const free = typeof address === 'object' && address ? address.port : 0;
                server.close(() => { resolve(free); });
            });
            server.once('error', reject);
        });
        this.forward = spawn(this.kc[0], [...this.kc.slice(1), 'port-forward', `pod/${this.name}`, `${port}:8080`], { stdio: 'ignore' });
        await sleep(3000);
        return `https://127.0.0.1:${port}`;
    }

    override async selkiesLog(lines = 2000): Promise<string> {
        return (await run([...this.kc, 'logs', this.name, `--tail=${lines}`], 60)).stdout.toString();
    }

    override async down(): Promise<void> {
        this.forward?.kill('SIGTERM');
        await run([...this.kc, 'delete', 'pod', this.name, '--wait=false'], 60);
    }
}

/** The image as a Docker container here, or on `host` over ssh (published on a free port there). */
export class DockerContainer extends Target {
    private readonly ssh: string[];
    private readonly host: string;
    private readonly port: number;
    private readonly env: Record<string, string>;
    private readonly gpus: string | undefined;

    constructor(image: string, name: string, env: Record<string, string>, host?: string, gpus?: string, port = 0) {
        super();
        this.env = env;
        this.gpus = gpus;
        this.image = image;
        this.name = name;
        this.ssh = host ? ['ssh', '-o', 'BatchMode=yes', host] : [];
        this.host = host ? (host.split('@').at(-1) ?? host) : '127.0.0.1';
        this.port = port || 18000 + (process.pid % 1000);
        this.execPrefix = [...this.ssh, 'docker', 'exec', '-i', '-u', '1000', name];
    }

    private docker(args: readonly string[], timeoutS = 120): Promise<Ran> {
        if (this.ssh.length) {
            const quoted = ['docker', ...args].map((a) => (/^[A-Za-z0-9_./:=@,+-]+$/.test(a) ? a : `'${a.replaceAll("'", "'\\''")}'`)).join(' ');
            return run([...this.ssh, quoted], timeoutS);
        }
        return run(['docker', ...args], timeoutS);
    }

    async up(timeoutS = 1800): Promise<void> {
        await this.docker(['rm', '-f', this.name]);
        const args = ['run', '-d', '--name', this.name, '--shm-size', '2g', '-p', `${this.port}:8080`];
        if (this.gpus) args.push('--gpus', this.gpus);
        for (const [k, v] of Object.entries(this.env)) args.push('-e', `${k}=${v}`);
        const p = await this.docker([...args, this.image], timeoutS);
        if (p.code !== 0) throw new Error(`docker run: ${p.stderr.toString().slice(-300)}`);
        const info = (await this.docker(['inspect', '--format', '{{.Image}}', this.name])).stdout.toString().trim();
        const repo = (await this.docker(['image', 'inspect', '--format', '{{join .RepoDigests " "}}', info])).stdout.toString();
        this.digest = /sha256:[0-9a-f]{64}/.exec(repo)?.[0] ?? info;
        this.url = `https://${this.host}:${this.port}`;
        if (!await this.waitReady(timeoutS)) throw new Error(`container ${this.name} never answered on ${this.url}`);
    }

    override async selkiesLog(lines = 2000): Promise<string> {
        const p = await this.docker(['logs', `--tail=${lines}`, this.name], 60);
        return p.stdout.toString() + p.stderr.toString();
    }

    override async down(): Promise<void> {
        await this.docker(['rm', '-f', this.name]);
    }
}

/** A session that is already up: its URL and the prefix that execs into it. */
export class Existing extends Target {
    constructor(url: string, execPrefix: string) {
        super();
        this.url = url.replace(/\/+$/, '');
        this.name = this.url;
        this.image = this.url;
        this.execPrefix = execPrefix.split(/\s+/).filter(Boolean);
    }

    async up(timeoutS = 120): Promise<void> {
        if (!await this.waitReady(timeoutS)) throw new Error(`${this.url} does not answer /api/status`);
    }

    override async selkiesLog(lines = 2000): Promise<string> {
        if (!process.env.E2E_IMAGE_LOGS) return '';
        return (await run([...commandFrom('E2E_IMAGE_LOGS', ''), `--tail=${lines}`], 60)).stdout.toString();
    }
}

/** The target E2E_IMAGE_WHERE asks for, not yet up. */
export function make(image: string, name: string, env: Record<string, string>, kube: KubeOptions = {}): Target {
    const where = process.env.E2E_IMAGE_WHERE ?? 'kube';
    const gpus = process.env.E2E_IMAGE_DOCKER_GPUS;
    if (where === 'docker') return new DockerContainer(image, name, env, undefined, gpus);
    if (where.startsWith('ssh:')) return new DockerContainer(image, name, env, where.slice(4), gpus);
    if (where.startsWith('url:')) return new Existing(where.slice(4), process.env.E2E_IMAGE_EXEC ?? '');
    return new KubePod(image, name, env, kube);
}
