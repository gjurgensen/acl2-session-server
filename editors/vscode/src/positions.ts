// Byte columns, and the two-line translation they need at this edge.
//
// Lines cross the daemon's wire untranslated: a line is a line in any
// encoding, which is why the protocol carried nothing else for so long. A
// column is the exception. The daemon counts bytes -- ACL2's model is
// Latin-1, its source is read as bytes, and the splitter's offsets are byte
// offsets -- while VSCode hands an extension UTF-16 code units. On the ASCII
// that ACL2 forms are written in the two agree exactly, so this is a no-op on
// almost every line in almost every book; it stops being one the moment a
// string literal or a comment holds anything above U+007F.
//
// Kept free of `vscode`, like `protocol.ts`, so both sides of the conversion
// are testable without an editor.
//
// The encoding is UTF-8, and the reason it is *safe* to fix it here is worth
// stating, because ACL2's own model is ISO-8859-1 and the file on disk may
// well be. The file is not what either side of this conversion is reading.
// `sendUpdate` sends the buffer, the daemon splits the UTF-8 encoding of
// that same buffer, and this measures the UTF-8 encoding of that same
// buffer again -- so the two agree by construction, for any file encoding at
// all, including one VSCode decoded wrongly. What matters is only that both
// sides are looking at the same string, and they are.
//
// The exception is the case `bufferCarriesFile` exists to find: a file whose
// bytes the buffer *cannot* carry, where the extension sends no text and the
// daemon reads the file itself. Then the two sides are looking at different
// bytes, and `sendUpdate` sends no column at all rather than a wrong one.

/**
 * The 1-based byte column of a character offset within one line's text.
 *
 * `character` is what a `vscode.Position` carries: a UTF-16 code-unit offset
 * from the start of the line.
 */
export function byteColumn(lineText: string, character: number): number {
  if (character <= 0) {
    return 1;
  }
  return Buffer.byteLength(lineText.slice(0, character), 'utf8') + 1;
}

/**
 * The inverse: the character offset a 1-based byte column lands on.
 *
 * A column past the end of the line clamps to the end, which is the right
 * answer for a stale one -- the file has been edited since, and pointing at
 * the end of the line beats throwing.
 */
export function charOffset(lineText: string, column: number): number {
  const want = column - 1;
  if (want <= 0) {
    return 0;
  }
  // Almost always true, and worth the one check: an all-ASCII line has one
  // byte per code unit, so the column is the offset.
  if (lineText.length === Buffer.byteLength(lineText, 'utf8')) {
    return Math.min(want, lineText.length);
  }
  let bytes = 0;
  for (let i = 0; i < lineText.length; i++) {
    if (bytes >= want) {
      return i;
    }
    // Surrogate pairs are one code point in two code units; stepping by code
    // point and counting both keeps the two in step.
    const code = lineText.codePointAt(i) ?? 0;
    const wide = code > 0xffff;
    bytes += Buffer.byteLength(String.fromCodePoint(code), 'utf8');
    if (wide) {
      i++;
    }
  }
  return lineText.length;
}
