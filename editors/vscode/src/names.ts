/**
 * What the world knows about a name, and how it reads in a hover.
 *
 * Deliberately free of `vscode`, like `protocol.ts` and for the same reason:
 * the rendering is the part with decisions in it, so it is the part that has
 * to be checkable under plain node. The provider that wraps it is glue.
 */

export interface DescribeReply {
  /** False when the session was proving and the answer was not cached. */
  available: boolean;
  found: boolean;
  name: string;
  kind?: 'function' | 'macro' | 'theorem' | 'constant' | 'other';
  formals?: string;
  guard?: string;
  statement?: string;
  /** As ACL2 names it: `(:system . "std/util/bstar.lisp")`, or a full path. */
  book?: string;
  /** `book` resolved to something openable. */
  path?: string;
}

/** Where a name is defined. `unit` means this file; `path` means another. */
export interface DefinitionReply {
  available: boolean;
  found: boolean;
  unit?: number;
  path?: string;
  /** Absent means "somewhere in that file" -- open it at the top. */
  line?: number;
  /** `definer` when the name is written at that line. */
  how?: string;
}

/**
 * ACL2's own name for a book, as a human would say it.
 *
 * Two shapes, the same two the dependency work resolves: a bare absolute
 * path, and `(:system . "rel/path.lisp")` for anything reached by
 * `:dir :system`. The system form is shown relative, because that is how it
 * is written in the `include-book` that pulled it in -- an absolute path
 * under someone's checkout is longer and says less.
 */
export function bookLabel(book: string | undefined): string | undefined {
  if (!book) {
    return undefined;
  }
  const quoted = /"([^"]+)"/.exec(book);
  if (quoted) {
    return quoted[1];
  }
  const t = book.trim();
  return t.length > 0 ? t : undefined;
}

/** `(alpha x)` from the name and ACL2's `(x)`. */
function callForm(name: string, formals: string): string {
  const inner = formals.trim().replace(/^\(/, '').replace(/\)$/, '').trim();
  return inner.length > 0 ? `(${name} ${inner})` : `(${name})`;
}

function fence(code: string): string {
  return '```lisp\n' + code + '\n```';
}

/**
 * The hover body, as markdown -- or `undefined` when there is nothing worth
 * showing.
 *
 * `available: false` renders as nothing rather than as an apology. It means
 * the prover was busy and the client should ask again, which it will do by
 * itself the next time the cursor moves; a tooltip saying "busy" would be a
 * worse answer than no tooltip, because it occupies the space where the real
 * answer goes and invites a re-read rather than a re-hover.
 */
export function hoverMarkdown(d: DescribeReply): string | undefined {
  if (!d.available || !d.found) {
    return undefined;
  }
  const parts: string[] = [];
  parts.push(`**${d.name}** *${d.kind ?? 'name'}*`);

  if (d.kind === 'theorem' && d.statement) {
    parts.push(fence(d.statement));
  } else if (d.formals) {
    parts.push(fence(callForm(d.name, d.formals)));
  }

  // A guard of `t` is every function's default and says nothing; ACL2 prints
  // the trivial one as the symbol, so it is dropped rather than shown as noise.
  if (d.guard && d.guard.trim().toLowerCase() !== 't') {
    parts.push(`Guard\n\n${fence(d.guard)}`);
  }

  const where = bookLabel(d.book);
  parts.push(where ? `Defined in \`${where}\`` : '*Defined in this file*');
  return parts.join('\n\n');
}
