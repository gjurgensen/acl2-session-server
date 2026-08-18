// Re-flow an ACL2 term to the width it is actually being read at.
//
// ACL2 pretty-prints for a 77-column terminal. The panel is a side pane and
// is usually much narrower, so a term that fitted on one line for ACL2
// arrives as one long line here and gets soft-wrapped by CSS at whatever
// column the pane happens to be -- mid-argument, with no indentation to show
// what belongs to what. This lays it out again at the real width.
//
// Deliberately in the client rather than in the core or the daemon: the
// width is a property of the window, nobody else knows it, and it changes
// when the pane is dragged. It is also the only place that can re-flow a
// term from *any* source uniformly -- checkpoints from a probe, a scheme
// from the log, a statement from a hover -- and the only one that keeps
// working while a proof is grinding.
//
// Total, like the induction parser: anything that does not parse comes back
// exactly as it arrived. A term this cannot read is a term ACL2 printed and
// we would rather show verbatim than mangle.

import { MAX_TERM } from './policy';

/** A parsed term: an atom's text, or a list of nodes. */
type Node = string | Node[];

/**
 * Heads whose arguments always go one per line, whatever the width.
 *
 * The one rule here that is a style choice rather than a consequence of the
 * width, and deliberately unconditional: a goal is read as its structure,
 * not as text that happens to fit. `implies` breaks after the hypotheses so
 * the conclusion starts a line of its own; a conjunction puts each conjunct
 * on its own line so they can be counted and compared down the page. That a
 * short one would fit is not a reason to run them together.
 *
 * Extending this is a one-line change; the binding forms are the obvious
 * candidates if it ever feels short.
 */
const BREAK_HEADS = new Set(['implies', 'and', 'or', 'iff', 'cond', 'case']);

/** Longer heads stop earning an aligned column; their arguments go under. */
const ALIGN_LIMIT = 12;

/**
 * How far right an argument may be indented, whatever its nesting depth.
 *
 * The bound that keeps layout linear in the size of the term; see `render`.
 * Comfortably past anything read at a normal pane width, so ordinary terms
 * never reach it and look exactly as they did.
 */
const INDENT_LIMIT = 40;

export function pretty(term: string, cols: number): string {
  // Total in the other direction too: a term too big to read comes back
  // exactly as it arrived, for the same reason one that will not parse does.
  // Laying it out is only worth doing if the result can be read, and the
  // layout *grows* the text -- 6.83 MB in gave 15.3 MB out. See `MAX_TERM`.
  if (term.length > MAX_TERM) {
    return term;
  }
  const node = parse(term);
  if (node === undefined) {
    return term;
  }
  return render(node, 0, Math.max(20, cols));
}

// ---- Reading -----------------------------------------------------------

/**
 * Parse one term, or give up.
 *
 * Gives up on anything left over, on unbalanced parens, and on anything it
 * does not recognise -- all of which mean "show it as ACL2 printed it".
 */
function parse(src: string): Node | undefined {
  const toks = lex(src);
  if (!toks) {
    return undefined;
  }
  let i = 0;
  const node = (): Node | undefined => {
    const t = toks[i];
    if (t === undefined) {
      return undefined;
    }
    if (t === ')') {
      return undefined;
    }
    if (t !== '(') {
      i++;
      return t;
    }
    i++;
    const out: Node[] = [];
    for (;;) {
      if (i >= toks.length) {
        return undefined; // unbalanced
      }
      if (toks[i] === ')') {
        i++;
        return out;
      }
      const child = node();
      if (child === undefined) {
        return undefined;
      }
      out.push(child);
    }
  };
  const first = node();
  return first !== undefined && i === toks.length ? first : undefined;
}

/**
 * Split into parens and atoms.
 *
 * Strings, `#\(` character literals and `|bar quoted|` symbols each hide
 * characters that must not be read as structure -- the same set the console's
 * balance check and the core's term reader have to know about.
 */
function lex(src: string): string[] | undefined {
  const out: string[] = [];
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    if (/\s/.test(c)) {
      i++;
      continue;
    }
    if (c === '(' || c === ')') {
      out.push(c);
      i++;
      continue;
    }
    // An atom runs to the next delimiter, with the quoting forms opaque.
    const start = i;
    while (i < src.length && !/[\s()]/.test(src[i])) {
      if (src[i] === '"' || src[i] === '|') {
        const q = src[i];
        i++;
        while (i < src.length && src[i] !== q) {
          i += src[i] === '\\' ? 2 : 1;
        }
        if (i >= src.length) {
          return undefined; // unterminated
        }
        i++;
      } else if (src[i] === '#' && src[i + 1] === '\\') {
        i += 3;
      } else {
        i++;
      }
    }
    if (i === start) {
      return undefined;
    }
    out.push(src.slice(start, i));
  }
  return out;
}

// ---- Laying out --------------------------------------------------------

/** The one-line form, ignoring width. */
/**
 * Cached per node, because `render` asks every node whether it fits -- and
 * answering meant re-flattening everything beneath it. One walk per node
 * over its whole subtree is quadratic in the size of the term, and on a
 * deeply nested one it is worse: 43 KB took 0.7 s to lay out and 88 KB took
 * 13.5 s. Nodes are the arrays built by `parse` and are not mutated, so
 * caching against identity is sound; a `WeakMap` keeps it from holding
 * anything alive.
 */
const flatCache = new WeakMap<object, string>();

function flat(n: Node): string {
  if (typeof n === 'string') {
    return n;
  }
  const hit = flatCache.get(n);
  if (hit !== undefined) {
    return hit;
  }
  const out = `(${n.map(flat).join(' ')})`;
  flatCache.set(n, out);
  return out;
}

/**
 * Whether this form is laid out down the page whatever the width.
 *
 * Propagates upwards: a form containing one cannot itself be flat, or the
 * result is a broken conjunction sitting inside an unbroken parent, which
 * reads worse than either.
 */
const breakCache = new WeakMap<object, boolean>();

function mustBreak(n: Node): boolean {
  if (typeof n === 'string') {
    return false;
  }
  // Cached for the same reason `flat` is: `render` asks this of every node,
  // and answering walks the whole subtree beneath it.
  const hit = breakCache.get(n);
  if (hit !== undefined) {
    return hit;
  }
  const out = breaks(n);
  breakCache.set(n, out);
  return out;
}

function breaks(n: Node): boolean {
  if (typeof n === 'string') {
    return false;
  }
  const [head, ...args] = n;
  if (
    typeof head === 'string' &&
    BREAK_HEADS.has(head.toLowerCase().replace(/^acl2::/, '')) &&
    args.length >= 2
  ) {
    return true;
  }
  return n.some(mustBreak);
}

/**
 * Lay `n` out starting at column `col`.
 *
 * The first line carries no indentation -- the caller has already placed it
 * -- while every later line is indented absolutely, so a nested call's output
 * can be dropped in as it stands.
 */
function render(n: Node, col: number, cols: number): string {
  const one = flat(n);
  if (col + one.length <= cols && !mustBreak(n)) {
    return one;
  }
  if (typeof n === 'string' || n.length === 0) {
    return one; // an atom, or `()`: nothing to break
  }
  const [head, ...args] = n;
  const headText = flat(head);
  // Arguments line up after "(head ", which is what makes a broken form
  // readable. A head that is itself a form, or a long one, would push that
  // column so far right that the arguments have no room, so those indent by
  // one instead.
  const aligned = typeof head === 'string' && headText.length <= ALIGN_LIMIT;
  // Indentation stops growing past a point, and that is what keeps this
  // linear. Aligning every level under its head is what makes a term
  // readable, but it also means a deeply nested one indents every line below
  // it: lines grow with depth while their number grows with size, so the
  // output is quadratic in the input. Measured on nested conjunctions --
  // 5 KB in gave 84 KB out, 21 KB gave 1.2 MB, 43 KB gave 4.9 MB in 680 ms.
  // A `defines` clique produces exactly that shape, and a few such terms
  // built an extension host heap of 1.8 GB that spent its life in GC while
  // every other extension's keystrokes went unanswered.
  //
  // Past the cap, arguments still break onto their own lines; they just stop
  // marching rightwards. Nothing is lost that a reader could have used: an
  // argument indented two hundred columns is off the side of the pane
  // anyway.
  const want = aligned ? col + headText.length + 2 : col + 1;
  const argCol = Math.min(want, INDENT_LIMIT);
  if (args.length === 0) {
    return `(${headText})`;
  }
  const pad = ' '.repeat(argCol);
  const parts = args.map((a) => render(a, argCol, cols));
  // The first argument shares the head's line when it is aligned there;
  // otherwise every argument starts on its own.
  return aligned
    ? `(${headText} ${parts[0]}${parts.slice(1).map((p) => `\n${pad}${p}`).join('')})`
    : `(${headText}${parts.map((p) => `\n${pad}${p}`).join('')})`;
}
