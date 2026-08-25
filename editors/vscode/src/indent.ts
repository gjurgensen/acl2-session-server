// Lisp indentation, computed from structure rather than from regexes.
//
// VSCode's own auto-indent is driven by `indentationRules` in a language
// configuration, which are regexes matched against the previous line. That
// model cannot express Lisp indentation at any quality: where a line belongs
// depends on the paren nesting above it, on the head symbol of the enclosing
// form, and on the column the head's first argument happens to sit in --
// none of which a regex over one line can see. So the rules are computed
// here and delivered through the formatting providers instead, which is the
// one route VSCode leaves open for a language that needs to think.
//
// Kept free of `vscode`, like `positions.ts` and `policy.ts`, for two
// reasons. The obvious one is that it can then be tested without an editor,
// against whole books. The other is that indentation must work in a file
// with no session: the daemon is spawned lazily and may not be installed at
// all, and a person typing into a `.lisp` file is owed a working Tab either
// way. Nothing in here may reach for the daemon.
//
// The rule set is not invented. It is Emacs' `lisp-indent-function` model --
// the one every ACL2 book in the community checkout was indented by -- with
// the community's own tables transcribed into `FORMS`. See that constant for
// where each entry came from.

/**
 * How a form's arguments are indented, in Emacs' `lisp-indent-function`
 * vocabulary.
 *
 * `'defun` means "like a definition": the body sits at the open paren's
 * column plus `bodyIndent`, however many arguments the head line carries. A
 * number `k` means the form has `k` distinguished arguments, indented deeper
 * than the body that follows them -- `(mv-let (a b) <distinguished> body)`.
 */
export type IndentSpec = 'defun' | number;

export interface IndentOptions {
  /** Extra entries, and overrides, for {@link FORMS}. */
  forms?: Record<string, IndentSpec>;
  /** Emacs' `lisp-body-indent`: how far a body sits inside its form. */
  bodyIndent?: number;
  /** How much deeper a distinguished argument sits than the body. */
  distinguished?: number;
  /**
   * Where the first argument of a plain function call goes when the head is
   * alone on its line.
   *
   * Emacs' own answer is the open paren's column plus one, so the argument
   * lines up just inside the paren. `acl2-indent.el` makes this adjustable
   * and defaults it to one more than that; its `indent-first-function-arg`
   * is this number minus one.
   *
   * Emacs' bare default is the right one to keep. Widening it to 2 looks
   * attractive from the `(must-fail` / `(must-succeed*` wrappers, whose
   * argument does start two columns in -- but those are macros whose body is
   * a body, and naming them as such is the fix. Applied globally the wider
   * indent costs eighteen points of corpus agreement, because it also moves
   * every `(xdoc::p "..."` whose string begins on the next line.
   */
  firstArg?: number;
  /**
   * Treat an unknown head beginning `def` as a definition.
   *
   * ACL2 is written in macros, and a book's own definers outnumber the ones
   * any table can name. `acl2-indent.el` settled on this heuristic years ago
   * (`indent-def-beginning-functions-like-defun`) and the books bear it out.
   */
  defPrefix?: boolean;
  /**
   * What a line with nothing on it should get.
   *
   * `empty` for a whole-buffer pass: a blank line carries no structure and
   * trailing spaces on it are litter. `indent` for the keystroke path, and
   * it is the whole of what pressing Enter means -- at the moment the
   * on-type provider is asked, the new line holds a copy of the previous
   * line's whitespace and nothing else, so treating it as blank is treating
   * every Enter as nothing to do.
   */
  blankLines?: 'empty' | 'indent';
}

const DEFAULTS: Required<Omit<IndentOptions, 'forms'>> = {
  bodyIndent: 2,
  distinguished: 2,
  firstArg: 1,
  defPrefix: true,
  blankLines: 'empty',
};

/**
 * The indent table, transcribed rather than invented.
 *
 * Three sources, in increasing order of specificity, merged here:
 *
 *   - Emacs' own `lisp-mode` defaults, for the forms Common Lisp and ACL2
 *     share (`let`, `cond`, `when`, ...). These are what a book gets when
 *     nobody has loaded anything ACL2-specific, so they are the floor.
 *   - `emacs/emacs-acl2.el` in the ACL2 sources, which is what the ACL2
 *     developers load. Twenty-four `put` forms, mostly `with-...` wrappers.
 *   - `books/interface/emacs/acl2-indent.el` (Stephen Westfold), the richest
 *     of the three at sixty-four entries, and the only one that knows the
 *     Kestrel definers -- `defiso`, `defmapping`, `defbyte`.
 *
 * Where they disagree the most specific wins, which is how `encapsulate`
 * ends up at 1 rather than `defun`. Those disagreements are real and few;
 * `indent.mjs` measures each of them against the books rather than settling
 * it by argument.
 */
export const FORMS: Record<string, IndentSpec> = {
  // Emacs `lisp-mode`, for what the dialects share.
  //
  // `if` is the one entry the three sources disagree on outright:
  // `acl2-mode.el` sets it to nil (a plain call, arguments aligned under the
  // test), while `books/doc/practices.lisp` tells readers to
  // `(put 'if 'common-lisp-indent-function 2)`. The books settle it -- 2 is
  // what they are written in, and assuming nil costs about a point of
  // agreement across the corpus on its own.
  if: 2,
  lambda: 'defun',
  let: 1,
  'let*': 1,
  flet: 1,
  labels: 1,
  when: 1,
  unless: 1,
  while: 1,
  // `cond` is deliberately absent. Emacs' *elisp* mode indents its clauses
  // at the body indent, and copying that costs eight points of corpus
  // agreement on its own -- more than any other single entry is worth.
  // Common Lisp, and every book here, aligns the clauses under the first
  // clause, which is what ordinary call handling already does.
  progn: 0,
  prog1: 1,
  prog2: 2,
  catch: 1,
  'multiple-value-bind': 2,
  'destructuring-bind': 2,

  // emacs/emacs-acl2.el.
  case: 'defun',
  'case!': 'defun',
  'case-match': 'defun',
  dolist: 'defun',
  'er@par': 'defun',
  'warning$@par': 'defun',
  'er-let*': 1,
  'with-acl2-channels-bound': 'defun',
  'with-fast-alist': 'defun',
  'with-fast-alists': 'defun',
  'with-global-stobj': 'defun',
  'with-local-stobj': 'defun',
  'with-open-file': 'defun',
  'with-output': 'defun',
  'with-output!': 'defun',
  'with-output-to': 'defun',
  'with-stdout': 'defun',
  'pattern-match': 'defun',
  'pattern-match-list': 'defun',
  'verify-termination': 'defun',

  // books/interface/emacs/acl2-indent.el.
  'b*': 1,
  'mv-let': 1,
  encapsulate: 1,
  'verify-guards': 1,
  defun: 'defun',
  'defun$': 'defun',
  defund: 'defun',
  'defun-sk': 'defun',
  'defund-sk': 'defun',
  deflist: 'defun',
  defprojection: 'defun',
  deftheory: 'defun',
  theorem: 'defun',
  axiom: 'defun',
  simplify: 'defun',
  spec: 'defun',
  op: 'defun',
  defthm: 1,
  defthmd: 1,
  'defthm-std': 1,
  defaxiom: 1,
  defchoose: 2,
  defconst: 1,
  defconstant: 1,
  defparameter: 1,
  defvar: 1,
  defpkg: 1,
  defstobj: 1,
  defrec: 1,
  defaggregate: 1,
  defxdoc: 1,
  'defxdoc+': 1,
  defbyte: 1,
  defubytelist: 1,
  deflist2: 1,
  defset: 1,
  defoset: 1,
  defsum: 1,
  defresult: 1,
  defsubtype: 1,
  definj: 1,
  defsurj: 1,
  defisar: 1,
  defiso: 5,
  defmapping: 1,
  'propagate-iso': 1,
  the: 1,
  'er-soft': 3,
  'er-soft+': 3,
  'define-skeleton': 3,
  'define-abbrev-table': 1,

  // The testing macros -- `must-fail`, `must-succeed*`, `assert!` and the
  // rest -- were tried here as body-indenting forms and are deliberately
  // absent. They looked like they belonged while `cond` was still wrong, and
  // once that was fixed every one of them cost agreement rather than earning
  // it: the books write them as ordinary calls. Nothing goes in this table
  // that the corpus does not pay for.
};

/** An open delimiter still unclosed at some point in the text. */
interface Open {
  /** Offset of the delimiter itself. */
  offset: number;
  /** Its column, in characters from the start of its line. */
  column: number;
}

const isSpace = (c: string): boolean => c === ' ' || c === '\t' || c === '\n' || c === '\r';

/**
 * Whether a character can be part of a symbol.
 *
 * Deliberately permissive: ACL2 symbols carry `$`, `*`, `+`, `<`, `!` and
 * package colons, and the only thing that reliably is *not* part of one is
 * whitespace, a delimiter, or a reader character.
 */
const isAtom = (c: string): boolean =>
  !isSpace(c) && !'()[]{}";\'`,'.includes(c);

/**
 * Skip whatever non-datum thing starts at `i`, or return `i` unchanged.
 *
 * The four things that hide parens from a naive scan -- strings, line
 * comments, block comments, and character literals -- plus `|bar symbols|`,
 * which can hide anything at all. Getting these wrong is not a cosmetic
 * matter: a `;` inside a string that swallowed the rest of a line would
 * silently shift every form below it.
 */
function skipNonDatum(text: string, i: number): number {
  const c = text[i];
  if (c === ';') {
    const nl = text.indexOf('\n', i);
    return nl === -1 ? text.length : nl;
  }
  if (c === '"') {
    let j = i + 1;
    while (j < text.length) {
      if (text[j] === '\\') {
        j += 2;
        continue;
      }
      if (text[j] === '"') {
        return j + 1;
      }
      j++;
    }
    return text.length;
  }
  if (c === '|') {
    // A backslash escapes the next character inside a bar symbol, as it does
    // everywhere else the reader looks: `|a\|b|` is one symbol whose name
    // holds a bar. Read as a plain search for the next `|`, it was two
    // symbols and a stray `b`, and every paren after it counted at the wrong
    // depth.
    let j = i + 1;
    while (j < text.length) {
      if (text[j] === '\\') {
        j += 2;
        continue;
      }
      if (text[j] === '|') {
        return j + 1;
      }
      j++;
    }
    return text.length;
  }
  if (c === '#') {
    const n = text[i + 1];
    // A fancy string: `#{"""` to the first `"""}`, with everything between
    // them content -- quotes, parens, backslashes and all. The grammar and
    // the README both advertise these; the indenter read `#` as an atom, `{`
    // as an open delimiter and each `"` as an ordinary quote toggle, so one
    // holding an odd number of quotes or an unbalanced paren desynchronised
    // the open stack and mis-indented every line after it.
    if (n === '{' && text.startsWith('"""', i + 2)) {
      const end = text.indexOf('"""}', i + 5);
      return end === -1 ? text.length : end + 4;
    }
    if (n === '|') {
      // Nested, per the standard: `#| a #| b |# c |#` closes once, not twice.
      let depth = 1;
      let j = i + 2;
      while (j < text.length && depth > 0) {
        if (text[j] === '#' && text[j + 1] === '|') {
          depth++;
          j += 2;
        } else if (text[j] === '|' && text[j + 1] === '#') {
          depth--;
          j += 2;
        } else {
          j++;
        }
      }
      return j;
    }
    if (n === '\\') {
      // `#\(` is a character, not an open paren. One character always, then
      // any name that follows it -- `#\Space`, `#\Newline`.
      let j = i + 3;
      while (j < text.length && isAtom(text[j])) {
        j++;
      }
      return j;
    }
  }
  return i;
}

/**
 * The delimiters still open at `limit`, outermost first.
 *
 * A plain left-to-right scan, which is the only honest way: whether a `(` is
 * a paren at all depends on everything before it. What keeps that affordable
 * is not scanning from the top of the file -- see {@link topLevelStarts}.
 */
export function openStack(text: string, from: number, limit: number, lineStart: number): Open[] {
  const stack: Open[] = [];
  let bol = lineStart;
  let i = from;
  while (i < limit) {
    const c = text[i];
    if (c === '\n') {
      bol = i + 1;
      i++;
      continue;
    }
    const skipped = skipNonDatum(text, i);
    if (skipped !== i) {
      // A skipped run can span lines -- a long string, a block comment --
      // and the column of everything after it depends on where it ended.
      for (let k = i; k < skipped && k < limit; k++) {
        if (text[k] === '\n') {
          bol = k + 1;
        }
      }
      i = skipped;
      continue;
    }
    if (c === '(' || c === '[' || c === '{') {
      stack.push({ offset: i, column: i - bol });
    } else if (c === ')' || c === ']' || c === '}') {
      // An unmatched close is not an error to report from here. The file is
      // being edited; it is unbalanced most of the time.
      stack.pop();
    }
    i++;
  }
  return stack;
}

/**
 * Offsets where a top-level form begins.
 *
 * The point of this is bounding the work. Indentation only ever needs the
 * enclosing top-level form, and re-scanning an 8000-line book on every
 * keystroke is not affordable -- `kestrel/c/syntax/validator.lisp` has a
 * single `defines` spanning five thousand lines. Scanning from the nearest
 * start above the cursor makes the cost proportional to one form.
 *
 * A `(` in column zero is the usual signal, but only the usual one: the same
 * shape appears inside a multi-line string. So this is a real scan, and the
 * result is worth caching against a document version.
 */
export function topLevelStarts(text: string): number[] {
  const starts: number[] = [];
  let depth = 0;
  let i = 0;
  while (i < text.length) {
    const skipped = skipNonDatum(text, i);
    if (skipped !== i) {
      i = skipped;
      continue;
    }
    const c = text[i];
    if (c === '(' || c === '[' || c === '{') {
      if (depth === 0) {
        starts.push(i);
      }
      depth++;
    } else if (c === ')' || c === ']' || c === '}') {
      depth = Math.max(0, depth - 1);
    }
    i++;
  }
  return starts;
}

/** A datum's extent, and enough about it to indent against. */
interface Datum {
  start: number;
  end: number;
  column: number;
  /** The text, when it is an atom; absent for a list. */
  atom?: string;
}

/**
 * Read the datum starting at or after `i`, stopping before `limit`.
 *
 * Prefix characters belong to the datum they precede -- `'(a b)` indents as
 * the list does, one column right -- which is what Emacs' own
 * `backward-prefix-chars` is for.
 */
function readDatum(text: string, i: number, limit: number, bolOf: (o: number) => number): Datum | null {
  let j = i;
  while (j < limit) {
    const c = text[j];
    if (isSpace(c)) {
      j++;
      continue;
    }
    // A comment is not a datum; skip it and keep looking.
    if (c === ';' || (c === '#' && text[j + 1] === '|')) {
      j = skipNonDatum(text, j);
      continue;
    }
    break;
  }
  if (j >= limit) {
    return null;
  }
  const start = j;
  // Prefixes first, so `,@foo` and `#'f` come out as one datum.
  while (j < limit && ("'`,".includes(text[j]) || (text[j] === '#' && text[j + 1] === "'"))) {
    j += text[j] === '#' ? 2 : 1;
  }
  const c = text[j];
  if (c === '(' || c === '[' || c === '{') {
    let depth = 0;
    while (j < text.length) {
      const skipped = skipNonDatum(text, j);
      if (skipped !== j) {
        j = skipped;
        continue;
      }
      const d = text[j];
      if (d === '(' || d === '[' || d === '{') {
        depth++;
      } else if (d === ')' || d === ']' || d === '}') {
        depth--;
        if (depth === 0) {
          j++;
          break;
        }
      }
      j++;
    }
    return { start, end: j, column: start - bolOf(start) };
  }
  const skipped = skipNonDatum(text, j);
  if (skipped !== j) {
    // A string or a `|bar symbol|` -- a datum, but not one with a head name.
    return { start, end: skipped, column: start - bolOf(start) };
  }
  let k = j;
  while (k < text.length && isAtom(text[k])) {
    k++;
  }
  if (k === j) {
    k++;
  }
  return { start, end: k, column: start - bolOf(start), atom: text.slice(j, k) };
}

/**
 * The head symbol's lookup key.
 *
 * The package qualifier is dropped so that `std::define` and `define` share
 * an entry -- with one exception, kept from `acl2-indent.el`: a name already
 * beginning `def` keeps whatever it has, because the `def` heuristic is
 * about the name a person reads, not the package it lives in.
 */
export function lookupKey(atom: string): string {
  const lower = atom.toLowerCase();
  if (lower.startsWith('def')) {
    return lower;
  }
  const colon = lower.lastIndexOf(':');
  return colon === -1 || colon === lower.length - 1 ? lower : lower.slice(colon + 1);
}

/**
 * How far the line beginning at `lineStart` should be indented.
 *
 * `from` bounds the scan; pass the enclosing top-level form's start, or 0.
 * The answer is a column, and it is the caller's business to decide whether
 * a line inside a string or a block comment should be moved at all -- see
 * {@link reindent}, which does not.
 *
 * `fromLine` is the offset of the *line* `from` sits on, and is not `from`:
 * it seeds the column counter, so passing the anchor's own offset makes every
 * delimiter on that line report `column = offset - anchor` -- zero for the
 * anchor itself -- and every branch that reads a column then computes from a
 * number that is short by the anchor's real column. It only matters when a
 * top-level form does not start in column 0, which is exactly the
 * two-forms-on-one-line case. Callers that supply `open0` are unaffected:
 * the stack is already built and this seeds nothing.
 */
export function indentFor(
  text: string,
  lineStart: number,
  from = 0,
  fromLine = 0,
  opts: IndentOptions = {},
  open0?: Open[]
): number {
  const o = { ...DEFAULTS, ...opts };
  const forms = opts.forms ? { ...FORMS, ...opts.forms } : FORMS;
  // A caller walking the whole buffer already knows the stack and has kept
  // it up to date line by line; re-deriving it here would make a
  // whole-file pass quadratic. `kestrel/c/syntax/validator.lisp`, whose
  // largest form spans five thousand lines, took 9.8 s that way.
  const stack = open0 ?? openStack(text, from, lineStart, fromLine);
  const open = stack[stack.length - 1];
  if (!open) {
    // Top level. Every form in every book starts in column zero, and the
    // one thing worse than not indenting is indenting these.
    return 0;
  }
  const bolOf = (offset: number): number => {
    const nl = text.lastIndexOf('\n', offset - 1);
    return nl + 1;
  };
  const head = readDatum(text, open.offset + 1, lineStart, bolOf);
  if (!head) {
    // `(` with nothing after it yet, which is what a half-typed form looks
    // like. One column in, so the first thing typed lands where its
    // siblings will.
    return open.column + 1;
  }
  if (head.atom === undefined) {
    // The first element is a list or a string, so this is data rather than a
    // call -- a `let` binding list, a `cond` clause, an alist. Align under
    // it, which is what makes columns of clauses line up.
    return head.column;
  }

  const key = lookupKey(head.atom);
  const spec = forms[key];
  // The `def` heuristic is what makes this workable at all: a book's own
  // definers outnumber every table's entries, and turning it off costs
  // thirty-eight points of corpus agreement. The obvious companion rule --
  // treat `..._case` the same, for fty's generated case macros -- was tried
  // and *costs* three points, because those are written inline as
  // expressions and take ordinary call alignment. It is not here for that
  // reason, and should not be added back without the measurement.
  const effective: IndentSpec | undefined =
    spec ?? (o.defPrefix && key.startsWith('def') ? 'defun' : undefined);

  const body = open.column + o.bodyIndent;
  if (effective === 'defun') {
    return body;
  }
  if (typeof effective === 'number') {
    // Count the arguments that finish before this line: that is the index of
    // the one starting here. Distinguished arguments sit deeper than the
    // body, so that `(mv-let (a b) (f x) body)` shows at a glance which of
    // its parts is which.
    // Stop as soon as the answer is settled. Only the comparison against
    // `effective` matters, and `effective` is a small number -- so this walks
    // a handful of arguments rather than every argument of a form that may
    // be thousands of lines long.
    let count = 0;
    let at = head.end;
    while (count < effective) {
      const d = readDatum(text, at, lineStart, bolOf);
      if (!d) {
        break;
      }
      count++;
      at = d.end;
    }
    return count < effective ? body + o.distinguished : body;
  }

  // A plain call. If an argument shares the head's line, everything below
  // lines up under it; that alignment is the single most recognisable thing
  // about how Lisp is written.
  const first = readDatum(text, head.end, lineStart, bolOf);
  if (first && bolOf(first.start) === bolOf(head.start)) {
    return first.column;
  }
  return open.column + o.firstArg;
}

/**
 * Whether a line's indentation is the author's business rather than ours.
 *
 * A comment opened with a single `;` is, by a convention ACL2 has kept since
 * before the books were books: those comments sit where they were typed --
 * very often in column zero, inside a form nested twenty columns deep -- and
 * carry meaning by it. Emacs' own `lisp-mode` would move them to
 * `comment-column`; the corpus says plainly that nobody does. `;;` and
 * `;;;` are ordinary and get the structural indent.
 *
 * Exported because both the whole-buffer pass and the editor's per-line
 * providers have to agree about it, and it is the sort of rule that is worth
 * being able to point at.
 */
export function keepsOwnColumn(lineText: string): boolean {
  const t = lineText.trimStart();
  return t.startsWith(';') && !t.startsWith(';;');
}

/** A line's leading whitespace, in characters. */
function leading(line: string): number {
  let n = 0;
  while (n < line.length && (line[n] === ' ' || line[n] === '\t')) {
    n++;
  }
  return n;
}

/**
 * Lexical state carried from one line to the next.
 *
 * Re-indenting has to know two things at each line: how deep the parens are,
 * so it knows which top-level form it is inside, and whether the line begins
 * inside a string or a block comment, where leading whitespace is content
 * rather than layout. Both are answers about everything above the line, and
 * carrying them forward is what keeps a whole-buffer pass linear instead of
 * rescanning the form above every line.
 */
interface LineState {
  /** The delimiters still open, outermost first. */
  stack: Open[];
  inString: boolean;
  block: number;
  /**
   * Inside a `#{"""..."""}` fancy string.
   *
   * Its own flag rather than a case of `inString`, because the two end
   * differently and nothing else about them coincides: an ordinary string
   * ends at the first unescaped `"`, a fancy string ends only at `"""}` and
   * has no escapes at all -- which is what it is *for*. Carried across lines
   * because spanning them is the point of the syntax.
   */
  fancy: boolean;
}

/**
 * Advance the lexical state across one line.
 *
 * `lineStart` is the line's offset in the text being built, so that an open
 * delimiter can record where it is and what column it sits in -- which is
 * the whole of what indentation needs from the lines above.
 */
function stepLine(line: string, lineStart: number, st: LineState): LineState {
  const stack = st.stack.slice();
  let { inString, block, fancy } = st;
  let i = 0;
  while (i < line.length) {
    const c = line[i];
    if (fancy) {
      // Nothing but the closing delimiter means anything in here, escapes
      // included.
      const end = line.indexOf('"""}', i);
      if (end === -1) {
        break;
      }
      fancy = false;
      i = end + 4;
      continue;
    }
    if (inString) {
      if (c === '\\') {
        i += 2;
        continue;
      }
      if (c === '"') {
        inString = false;
      }
      i++;
      continue;
    }
    if (block > 0) {
      if (c === '#' && line[i + 1] === '|') {
        block++;
        i += 2;
      } else if (c === '|' && line[i + 1] === '#') {
        block--;
        i += 2;
      } else {
        i++;
      }
      continue;
    }
    if (c === ';') {
      break;
    }
    if (c === '"') {
      inString = true;
      i++;
      continue;
    }
    if (c === '#' && line[i + 1] === '{' && line.startsWith('"""', i + 2)) {
      fancy = true;
      i += 5;
      continue;
    }
    if (c === '#' && line[i + 1] === '|') {
      block++;
      i += 2;
      continue;
    }
    if (c === '#' && line[i + 1] === '\\') {
      i += 3;
      while (i < line.length && isAtom(line[i])) {
        i++;
      }
      continue;
    }
    if (c === '|') {
      // Line-local, and deliberately: a `|bar symbol|` closing on a later
      // line is not a thing any book does, and a stray `|` that carried
      // state forward would swallow the rest of the file. `skipNonDatum`
      // does let one span lines -- it scans text rather than lines and has
      // no state to carry -- so the two can in principle disagree about the
      // same text. Neither disagrees about anything in the corpus.
      //
      // The escape is honoured either way: `|a\|b|` is one symbol.
      let j = i + 1;
      while (j < line.length) {
        if (line[j] === '\\') {
          j += 2;
          continue;
        }
        if (line[j] === '|') {
          break;
        }
        j++;
      }
      i = j >= line.length ? line.length : j + 1;
      continue;
    }
    if (c === '(' || c === '[' || c === '{') {
      stack.push({ offset: lineStart + i, column: i });
    } else if (c === ')' || c === ']' || c === '}') {
      stack.pop();
    }
    i++;
  }
  return { stack, inString, block, fancy };
}

/**
 * Re-indent every line.
 *
 * Top down and against the running result, because each line's target
 * depends on the columns of the lines above it: re-indenting against the
 * original would compute every line after the first from stale columns.
 *
 * A line that begins inside a string or a block comment is emitted
 * untouched. Its leading whitespace is part of what the file says -- xdoc
 * documentation strings in the books are full of deliberately aligned prose
 * -- and moving it would be an edit, not a formatting.
 */
export function reindent(text: string, opts: IndentOptions = {}): string {
  const lines = text.split('\n');
  let out = '';
  let st: LineState = { stack: [], inString: false, block: 0, fancy: false };
  /** Offset in `out` of the top-level form this line is inside. */
  let from = 0;
  for (let n = 0; n < lines.length; n++) {
    const line = lines[n];
    const lineStart = out.length;
    const opaque = st.inString || st.block > 0 || st.fancy;
    if (st.stack.length === 0 && !opaque) {
      // Between forms: whatever starts here is the next one, and nothing
      // above it can affect how anything inside it is indented.
      from = lineStart;
    }
    // A CRLF document splits on `\n` into lines that still carry their `\r`,
    // and that `\r` is the terminator rather than content: read as content it
    // made every blank line non-blank -- `\r` is not `''` -- so blank lines
    // inside a form were indented, gaining trailing whitespace, and the edit
    // built from the result embedded a bare `\r` that VSCode then read as a
    // line break of its own. Every Format Document grew the file.
    //
    // Split off and re-attached rather than special-cased, so nothing below
    // has to think about it and no later rule can forget.
    const cr = line.endsWith('\r') ? '\r' : '';
    const body = cr ? line.slice(0, -1) : line;
    const rest = body.slice(leading(body));
    let emitted: string;
    if (opaque || keepsOwnColumn(body)) {
      emitted = body;
    } else if (rest.length === 0) {
      emitted = '';
    } else {
      const want = indentFor(out, lineStart, from, from, opts, st.stack);
      emitted = ' '.repeat(Math.max(0, want)) + rest;
    }
    emitted += cr;
    out += emitted;
    if (n !== lines.length - 1) {
      out += '\n';
    }
    st = stepLine(emitted, lineStart, st);
  }
  return out;
}
/**
 * Lines `first` through `last`, each re-indented against the ones above it.
 *
 * The same rule {@link reindent} follows, over part of a buffer: top down,
 * and against the *running* result. Asking {@link plannedIndent} once per
 * line instead -- which is what this used to be -- plans every line against
 * the text as it was found, so a line whose parent is about to move is
 * planned at the column the parent has now. A `defun` whose first line is
 * over-indented by two gets its second line planned at column 12 where the
 * answer after the fix is 10, and the whole form lands internally
 * misaligned. It converges if you invoke it again, which is not a defence:
 * `editor.formatOnPaste` is on by default, so the commonest way to reach
 * this is a paste, and a paste happens once.
 *
 * Lines above the range are not this call's to change, so they are carried
 * verbatim -- and it is their real columns the range is measured against,
 * which is the difference between formatting a selection and formatting the
 * document.
 *
 * The walk starts at the top-level form enclosing the first line, the anchor
 * {@link plannedIndent} uses and for the reason {@link topLevelStarts}
 * exists: a book has forms five thousand lines long, and nothing above one
 * can affect a column inside it. That makes this cheaper than the per-line
 * loop it replaces, not dearer -- one scan of the enclosing form rather than
 * one per line of the range.
 *
 * Returns one entry per line in the range: the line as it should read, or
 * `undefined` for the three lines that are not ours -- blank, a lone `;`
 * comment keeping its own column, and one inside a string or block comment.
 * `undefined` rather than the line unchanged, so a caller can tell "leave it
 * alone" from "it is already right" and skip producing an edit at all.
 */
export function reindentRange(
  text: string,
  starts: number[],
  first: number,
  last: number,
  opts: IndentOptions = {}
): (string | undefined)[] {
  const lines = text.split('\n');
  const lo = Math.max(0, first);
  const hi = Math.min(lines.length - 1, last);
  if (hi < lo) {
    return [];
  }
  const offset: number[] = new Array(hi + 1);
  for (let n = 0, at = 0; n <= hi; n++) {
    offset[n] = at;
    at += lines[n].length + 1;
  }

  const anchor = enclosingStart(starts, offset[lo]);
  // The line the anchor sits on. `enclosingStart` never answers past the
  // offset it was asked about, so this walks down, never up, and lands on a
  // line whose start is at or before the anchor.
  let a = lo;
  while (a > 0 && offset[a] > anchor) {
    a--;
  }

  // Absolute offsets throughout: the text above the walk is carried verbatim
  // so that a position in `out` means what it means in `text`, which is what
  // lets the open stack, `bolOf` and `readDatum` work here unchanged. Below
  // the anchor they are the *running* result's offsets, which is the point.
  let out = text.slice(0, offset[a]);
  let st: LineState = { stack: [], inString: false, block: 0, fancy: false };
  let from = anchor;
  const planned: (string | undefined)[] = [];

  for (let n = a; n <= hi; n++) {
    const line = lines[n];
    const lineStart = out.length;
    const opaque = st.inString || st.block > 0 || st.fancy;
    if (st.stack.length === 0 && !opaque) {
      from = lineStart;
    }
    let emitted = line;
    if (n >= lo) {
      const blank = !line.trim();
      if (opaque || keepsOwnColumn(line) || (blank && (opts.blankLines ?? DEFAULTS.blankLines) === 'empty')) {
        planned.push(undefined);
      } else {
        const want = indentFor(out, lineStart, from, from, opts, st.stack);
        emitted = ' '.repeat(Math.max(0, want)) + line.slice(leading(line));
        planned.push(emitted);
      }
    }
    out += emitted;
    if (n !== lines.length - 1) {
      out += '\n';
    }
    // The anchor's own line is stepped from the anchor rather than from its
    // start: `topLevelStarts` promises the anchor is not inside a string or a
    // comment, and promises nothing at all about the text to its left. Blanked
    // rather than sliced, because `stepLine` reads a delimiter's column off
    // its index in the string it is given -- slicing would put every open
    // paren on that line at the wrong column, which is `openStack`'s reason
    // for taking a `lineStart` of its own. Spaces are inert to the lexer.
    const skip = n === a ? anchor - lineStart : 0;
    st = stepLine(skip > 0 ? ' '.repeat(skip) + emitted.slice(skip) : emitted, lineStart, st);
  }
  return planned;
}


/**
 * Whether `limit` sits inside a string or a block comment.
 *
 * Both are places where leading whitespace is content rather than layout,
 * and moving it changes what the file means.
 */
export function insideOpaque(text: string, from: number, limit: number): boolean {
  let i = from;
  while (i < limit) {
    const skipped = skipNonDatum(text, i);
    if (skipped !== i) {
      if (skipped > limit) {
        // The run swallows the target, so the target is inside it -- unless
        // it is a line comment, which ends at the newline anyway.
        return text[i] !== ';';
      }
      i = skipped;
      continue;
    }
    i++;
  }
  return false;
}

/**
 * The top-level form start at or before `offset`.
 *
 * Binary search, because the list has one entry per form in the book and
 * this is asked on the keystroke path.
 */
export function enclosingStart(starts: number[], offset: number): number {
  let lo = 0;
  let hi = starts.length - 1;
  let best = 0;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (starts[mid] <= offset) {
      best = starts[mid];
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return best;
}

/**
 * What one line's indentation should be, or nothing if it is not ours.
 *
 * The whole decision for a single line, in one place: this is what the
 * editor's providers ask, so that what they contain is the editor API and
 * nothing else. Three answers are `undefined` -- a blank line, a line whose
 * column belongs to its author (see {@link keepsOwnColumn}), and a line
 * inside a string or block comment -- and they are `undefined` rather than
 * "the current indent" so a caller can tell "leave it alone" from "it is
 * already right", and skip producing an edit at all.
 */
export function plannedIndent(
  text: string,
  lineStart: number,
  starts: number[],
  opts: IndentOptions = {}
): number | undefined {
  const nl = text.indexOf('\n', lineStart);
  const line = text.slice(lineStart, nl === -1 ? text.length : nl);
  if (keepsOwnColumn(line)) {
    return undefined;
  }
  if (!line.trim() && (opts.blankLines ?? DEFAULTS.blankLines) === 'empty') {
    return undefined;
  }
  const from = enclosingStart(starts, lineStart);
  if (insideOpaque(text, from, lineStart)) {
    return undefined;
  }
  // The anchor's *line*, not the anchor. `(defun a (x) x) (defun b (y)` puts
  // a top-level form at column 16, and seeding the column counter at the form
  // rather than at its line made every column on that line 16 too small: the
  // body of `b` was planned at 2 where the whole-buffer pass says 18. On-type
  // Enter, Tab, and on-type `)` all came through here, so the keystroke path
  // and Format Selection disagreed about the same text.
  const fromLine = text.lastIndexOf('\n', from - 1) + 1;
  return indentFor(text, lineStart, from, fromLine, opts);
}
