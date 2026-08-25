// Framed JSON-RPC 2.0 over the daemon's stdio -- the acl2-session-editor
// wire. Hand-rolled rather than vscode-jsonrpc, so the wire itself pulls in
// nothing: the extension's three runtime dependencies are the icon font and
// the two halves of the TextMate tokenizer, and none of them is here. The
// framing is the same Content-Length wire, so swapping a library in later
// stays possible.
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
  /**
   * The form's two columns, 1-based and counted in **bytes**: `col_end` is
   * one past the closing paren, so the pair reads as a range. They are the
   * only thing that tells two forms written on one line apart. See
   * `positions.ts` for turning them into editor positions.
   */
  col_start: number;
  col_end: number;
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

/**
 * What ACL2 had to say about one form: the log handle, and -- when the form
 * went red -- why. A file's form and an eval'd one carry the same envelope
 * (the daemon flattens both the same way), so one renderer serves both; the
 * fields either adds are its own.
 */
export interface Envelope {
  output_ref: string;
  output_lines: number;
  status?: string;
  error_line?: string;
  abort_cause?: string;
  prover_steps?: number;
  warnings?: string;
  time?: string;
  /** What the form evaluated to. Absent when there is nothing worth showing. */
  value?: string;
  /** Whether that value arrived as an error triple rather than a single value. */
  value_triple?: boolean;
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

export interface UnitEvent extends Envelope {
  book: string;
  revision: number;
  index: number;
  /** `admitted`, `skipped`, or `failed`. */
  outcome: string;
  line_start: number;
  line_end: number;
  col_start: number;
  col_end: number;
  elapsed_ms?: number;
  retry_output_ref?: string;
}

/** The reply to an `eval` request. */
export interface EvalReply extends Envelope {
  /** `ok` or `failed`. */
  outcome: string;
  /**
   * `none`, `undone`, or `dirty`. `undone` is the contract working -- the
   * form introduced events and they were rolled back. `dirty` means the
   * rollback did not verify, so the next sync rebuilds by replay.
   */
  world_change: string;
  elapsed_ms: number;
}

/** `queued` while the session is busy, then `running` when the form goes. */
export interface EvalStateEvent {
  book: string;
  state: string;
  ahead: number;
}

export interface TailEvent {
  book: string;
  unit: number | null;
  elapsed_ms: number;
  text: string;
  /**
   * The running form's log handle, once it has one. Readable while the form
   * runs, so a view wanting more than the tail carried here asks for it by
   * handle instead of waiting for the form to land.
   */
  output_ref?: string;
}

export interface SettledEvent {
  book: string;
  revision: number;
  reason: string;
  message?: string;
  /**
   * Where the trouble starts, 1-based. Only `split_error` carries one: the
   * line of the `(` that is never closed, so everything from there to the end
   * of the buffer is unread rather than merely unchecked.
   */
  line?: number;
}

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
  /** Cleared on every settle, so a finished request holds no timer. */
  timer?: ReturnType<typeof setTimeout>;
}

/**
 * Events: 'state' | 'unit' | 'tail' | 'settled' | 'stale_deps' | 'eval_state'
 * (the daemon's stream), 'daemon-error' (the daemon's own error notification
 * -- renamed because a bare 'error' event throws when unhandled), 'stderr',
 * 'spawn-error', 'exit'.
 */
export class DaemonClient extends EventEmitter {
  private proc: ChildProcessWithoutNullStreams;
  private buf = Buffer.alloc(0);
  /**
   * Bytes the daemon has sent, ever.
   *
   * Cheap to keep and the one number that says whether the client is being
   * flooded. Traffic should scale with the book, not with how long a form
   * runs: a client that asked for a slice on every `tail` once took 245 MB in
   * eleven minutes where the events themselves were ~1.5 MB.
   */
  bytesReceived = 0;
  private nextId = 1;
  private pending = new Map<number, Pending>();

  constructor(command: string, args: string[]) {
    super();
    // ACL2_FRONTEND reaches the ACL2 under the daemon, which inherits this
    // environment, and is there for an acl2-customization.lsp to read: a
    // session driven from here has its output captured and re-rendered by
    // the extension, so settings a person would want at a terminal -- gag
    // mode, inhibited output -- are the wrong ones here. Only the frontend
    // knows which it is; the daemon serves several.
    const env = { ...process.env, ACL2_FRONTEND: 'vscode' };
    this.proc = spawn(command, args, { stdio: ['pipe', 'pipe', 'pipe'], env });
    this.proc.stdout.on('data', (b: Buffer) => this.feed(b));
    this.proc.stderr.on('data', (b: Buffer) => this.emit('stderr', b.toString('utf8')));
    this.proc.on('exit', (code, signal) => {
      for (const p of this.pending.values()) {
        clearTimeout(p.timer);
        p.reject(new Error('daemon exited'));
      }
      this.pending.clear();
      this.emit('exit', code, signal);
    });
    this.proc.on('error', (e: Error) => this.emit('spawn-error', e));
  }

  /**
   * `signalCode` as well as `exitCode`, because a process that died from a
   * signal has no exit code -- `exitCode` stays null and the signal lands in
   * `signalCode`. Without it a daemon that crashed, or that the OOM killer
   * took, reads as alive for ever; `killed` does not cover it either, since
   * that only records that *we* sent a signal, not that anything died. The
   * cost of getting this wrong is `ensureClient` handing back the corpse
   * instead of starting a new daemon.
   */
  get alive(): boolean {
    return this.proc.exitCode === null && this.proc.signalCode === null && !this.proc.killed;
  }

  /** For last-resort cleanup by a host that is exiting. */
  get pid(): number | undefined {
    return this.proc.pid;
  }

  /**
   * Ask the daemon something.
   *
   * `timeoutMs` is opt-in, and deliberately so. Daemon *exit* already rejects
   * everything in flight, so the case this covers is narrower and nastier: a
   * daemon that is alive and wedged, where a promise simply never settles. A
   * provider tolerates that -- a hover that never answers is a hover that
   * shows nothing -- but a command that awaits one hangs with no message
   * anywhere, which is what Start did.
   *
   * There is no default because a blanket one would be wrong on this wire.
   * `certify` and a queued `eval` are answered by the driver when the work
   * finishes, and that is minutes by design; a timeout that cancelled them
   * would break the two verbs whose whole point is taking a long time. So the
   * callers that have a deadline say what it is.
   */
  request(method: string, params: unknown, timeoutMs?: number): Promise<any> {
    const id = this.nextId++;
    const p = new Promise<any>((resolve, reject) => {
      const entry: Pending = { resolve, reject };
      if (timeoutMs !== undefined) {
        entry.timer = setTimeout(() => {
          this.pending.delete(id);
          reject(new Error(`the daemon did not answer ${method} within ${timeoutMs / 1000}s`));
        }, timeoutMs);
      }
      this.pending.set(id, entry);
    });
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
    this.bytesReceived += b.length;
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
      clearTimeout(p.timer);
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

/**
 * Why the prover inducted, recovered from the log.
 *
 * A different class of fact from the rest of this file: everything else is
 * probed out of the world or read from `get-event-data`, and the induction
 * scheme is *printed* by ACL2 and recorded nowhere. So every field is
 * optional -- a reworded ACL2 drops one rather than inventing it -- and an
 * empty list means "nothing recognised", not "no induction".
 */
/** One field of a rule, as ACL2 keys it: `:lhs`, `:trigger`, `:corollary`. */
export interface RuneFieldWire {
  key: string;
  value: string;
}

/** One rune a name introduced, with whatever its rule class carries. */
export interface RuneWire {
  rune: string;
  class?: string;
  enabled?: boolean;
  fields: RuneFieldWire[];
}

/**
 * What a form actually introduced, beyond what its text says.
 *
 * Served from the scrape the daemon took as the form was admitted, so it
 * needs no prover and arrives during a proof -- which is when a reader is
 * most likely to be looking at an earlier form.
 */
export interface RulesReply {
  found: boolean;
  name: string;
  symbol_class?: string;
  measure?: string;
  relation?: string;
  formals?: string;
  guard?: string;
  statement?: string;
  runes: RuneWire[];
}

export interface InductionWire {
  /** The pool name: `*1`, or `*1.1` for an induction beneath one. */
  goal?: string;
  /** The induction term from the conjecture, untranslated: `(append x y)`. */
  suggested_by?: string;
  /** Which rule suggested it: "the :induction rule binary-append". */
  attribution?: string;
  /** The scheme, with `:p` standing for the conjecture. */
  scheme?: string;
  /**
   * How long the scheme really is, when `scheme` is only its head.
   *
   * The daemon cuts it above its own limit and this is how the client knows
   * to say so. Absent means the whole scheme is there. A mutual recursion
   * inducts on its entire clique, and one measured here ran to 6.83 MB --
   * see `SCHEME_LIMIT` in the daemon's `wire.rs`, and `MAX_TERM` in
   * `policy.ts` for what the client does with a term that size.
   */
  scheme_chars?: number;
  /** The function whose admission licenses it, when one does. */
  justified_by?: string;
  /** ACL2's own phrasing: "two nontautological subgoals". */
  subgoals?: string;
}
