// Framed JSON-RPC 2.0 over the daemon's stdio -- the acl2-session-editor
// wire. Hand-rolled rather than vscode-jsonrpc so the extension has zero
// runtime dependencies; the framing is the same Content-Length wire, so
// swapping a library in later stays possible.
//
// Kept free of any vscode import on purpose: this module also runs under
// plain node, which is how smoke.mjs exercises the whole client-side
// protocol against a real daemon without an editor.

import { ChildProcessWithoutNullStreams, spawn } from 'child_process';
import { EventEmitter } from 'events';

export interface UnitState {
  index: number;
  line_start: number;
  line_end: number;
  status: string;
}

export interface StateEvent {
  book: string;
  revision: number;
  conditional_from: number | null;
  running: number | null;
  units: UnitState[];
}

export interface CheckpointEvent {
  kind: string;
  goal?: string;
  term: string;
}

export interface InnerEventWire {
  name: string;
  kind?: string;
  time?: string;
  prover_steps?: number;
  rules?: string[];
  abort_causes?: string;
}

export interface UnitEvent {
  book: string;
  revision: number;
  index: number;
  outcome: string;
  line_start: number;
  line_end: number;
  elapsed_ms?: number;
  output_ref: string;
  output_lines: number;
  retry_output_ref?: string;
  status?: string;
  error_line?: string;
  abort_cause?: string;
  prover_steps?: number;
  warnings?: string;
  time?: string;
  hint_events?: string[];
  splitter_rules?: string[];
  rules?: string[];
  checkpoints?: CheckpointEvent[];
  pre_revert?: boolean;
  forced?: boolean;
  inner?: InnerEventWire[];
  inner_total?: number;
  n_top?: number;
  n_sub?: number;
}

export interface TailEvent {
  book: string;
  unit: number | null;
  elapsed_ms: number;
  text: string;
}

export interface SettledEvent {
  book: string;
  revision: number;
  reason: string;
  message?: string;
}

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
}

/**
 * Events: 'state' | 'unit' | 'tail' | 'settled' (the daemon's stream),
 * 'daemon-error' (the daemon's own error notification -- renamed because a
 * bare 'error' event throws when unhandled), 'stderr', 'spawn-error', 'exit'.
 */
export class DaemonClient extends EventEmitter {
  private proc: ChildProcessWithoutNullStreams;
  private buf = Buffer.alloc(0);
  private nextId = 1;
  private pending = new Map<number, Pending>();

  constructor(command: string, args: string[]) {
    super();
    this.proc = spawn(command, args, { stdio: ['pipe', 'pipe', 'pipe'] });
    this.proc.stdout.on('data', (b: Buffer) => this.feed(b));
    this.proc.stderr.on('data', (b: Buffer) => this.emit('stderr', b.toString('utf8')));
    this.proc.on('exit', (code, signal) => {
      for (const p of this.pending.values()) p.reject(new Error('daemon exited'));
      this.pending.clear();
      this.emit('exit', code, signal);
    });
    this.proc.on('error', (e: Error) => this.emit('spawn-error', e));
  }

  get alive(): boolean {
    return this.proc.exitCode === null && !this.proc.killed;
  }

  /** For last-resort cleanup by a host that is exiting. */
  get pid(): number | undefined {
    return this.proc.pid;
  }

  request(method: string, params: unknown): Promise<any> {
    const id = this.nextId++;
    const p = new Promise<any>((resolve, reject) => this.pending.set(id, { resolve, reject }));
    this.frame({ jsonrpc: '2.0', id, method, params });
    return p;
  }

  notify(method: string, params: unknown): void {
    this.frame({ jsonrpc: '2.0', method, params });
  }

  /** Close stdin (the daemon's EOF) and make sure the process dies. */
  dispose(): void {
    try {
      this.proc.stdin.end();
    } catch {
      /* already gone */
    }
    // On EOF the daemon aborts in-flight proofs, joins its drivers, and
    // exits with every session dead; give that a moment before the hammer.
    const p = this.proc;
    setTimeout(() => {
      if (p.exitCode === null && !p.killed) {
        p.kill('SIGKILL');
      }
    }, 3000);
  }

  private frame(v: unknown): void {
    const body = Buffer.from(JSON.stringify(v), 'utf8');
    this.proc.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
    this.proc.stdin.write(body);
  }

  private feed(b: Buffer): void {
    this.buf = Buffer.concat([this.buf, b]);
    for (;;) {
      const headerEnd = this.buf.indexOf('\r\n\r\n');
      if (headerEnd < 0) {
        return;
      }
      const header = this.buf.subarray(0, headerEnd).toString('utf8');
      const m = /Content-Length:\s*(\d+)/i.exec(header);
      if (!m) {
        this.buf = this.buf.subarray(headerEnd + 4);
        continue;
      }
      const len = parseInt(m[1], 10);
      const start = headerEnd + 4;
      if (this.buf.length < start + len) {
        return;
      }
      const body = this.buf.subarray(start, start + len).toString('utf8');
      this.buf = this.buf.subarray(start + len);
      try {
        this.dispatch(JSON.parse(body));
      } catch {
        /* a malformed frame is the daemon's bug; skip it */
      }
    }
  }

  private dispatch(v: any): void {
    if (typeof v.id === 'number' && this.pending.has(v.id)) {
      const p = this.pending.get(v.id)!;
      this.pending.delete(v.id);
      if (v.error) {
        p.reject(new Error(v.error.message ?? 'daemon error'));
      } else {
        p.resolve(v.result);
      }
      return;
    }
    if (typeof v.method === 'string') {
      this.emit(v.method === 'error' ? 'daemon-error' : v.method, v.params);
    }
  }
}
