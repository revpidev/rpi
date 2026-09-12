// Golden-frame recorder for the questionnaire dialog (TE30 G3).
//
//   node scripts/ask-user-question-parity/gen-golden-frames.mjs
//
// Runs the crate's `ask_user_question_parity_runner golden` command (the native fixture
// component rendered at 80/100/120 columns) and writes one JSONL file per
// scenario/width into
// `fixtures/generated/ask-user-question-parity/golden-frames/`.
//
// The files are COMMITTED; `run-parity.mjs` re-renders and compares
// byte-for-byte, so re-record deliberately (Q3's rich-interaction pass
// re-records this baseline) and review the diff like any other fixture.
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "../..");
const RUNNER = resolve(REPO, "target/debug/examples/ask_user_question_parity_runner");
const GOLDEN_DIR = resolve(REPO, "fixtures/generated/ask-user-question-parity/golden-frames");

if (!existsSync(RUNNER)) {
	const build = spawnSync(
		"cargo",
		["build", "-p", "rpi-ext-ask-user-question", "--example", "ask_user_question_parity_runner"],
		{ cwd: REPO, encoding: "utf-8" },
	);
	if (build.status !== 0) {
		console.error(build.stderr || build.stdout);
		process.exit(1);
	}
}

const result = spawnSync(RUNNER, ["golden"], { encoding: "utf-8" });
if (result.status !== 0) {
	console.error(result.stderr || result.stdout);
	process.exit(1);
}

const byFile = new Map();
for (const line of result.stdout.trim().split("\n").filter(Boolean)) {
	const entry = JSON.parse(line);
	if (!byFile.has(entry.file)) byFile.set(entry.file, []);
	byFile.get(entry.file).push(JSON.stringify(entry.frame));
}

mkdirSync(GOLDEN_DIR, { recursive: true });
for (const stale of readdirSync(GOLDEN_DIR).filter((file) => file.endsWith(".jsonl"))) {
	if (!byFile.has(stale)) rmSync(resolve(GOLDEN_DIR, stale));
}
for (const [file, frames] of byFile) {
	writeFileSync(resolve(GOLDEN_DIR, file), `${frames.join("\n")}\n`);
}
console.log(`recorded ${byFile.size} golden frame files into ${GOLDEN_DIR.replace(`${REPO}/`, "")}`);
