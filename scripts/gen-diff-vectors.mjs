// Generate diffWords reference vectors with the vendored jsdiff 8.0.4 for
// the Rust port's unit tests (crates/rpi/src/modes/interactive/components/diff.rs).
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const Diff = require("../external/pi/node_modules/diff");

const cases = [
    ['const a = 2;', 'const a = 3;'],
    ['foo bar', 'foo baz'],
    ['foo bar baz', 'foo qux baz'],
    ['foo   bar baz', 'foo  baz'],
    ['hello world', 'hello there world'],
    ['x = 1', 'x = 2'],
    ['a.b.c', 'a.c'],
    ['let x = 10;', 'let x = 100;'],
    ['  const y = foo(a, b);', '  const y = foo(a, c);'],
    ['', 'new content'],
    ['old content', ''],
    ['same', 'same'],
    ['import { a } from "./m";', 'import { a, b } from "./m";'],
    ['fn(a, b)', 'fn(a, b, c)'],
    ['alpha beta gamma', 'alpha beta gamma delta'],
    ['\tconst z = 1;', '\tconst z = 2;'],
    ['// comment here', '// comment here!'],
];

for (const [a, b] of cases) {
    const parts = Diff.diffWords(a, b);
    const vec = parts.map((p) => {
        const kind = p.removed ? 'removed' : p.added ? 'added' : 'keep';
        return [p.value, kind];
    });
    console.log(JSON.stringify([a, b, vec]) + ',');
}
