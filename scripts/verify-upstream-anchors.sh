#!/usr/bin/env bash
# Verifies the upstream line anchors cited by the codemode/tool_search source
# comments (V16-07 review O-E).
#
# The v0.99.2 -> v1.0.0 pin bump moved most of these lines; a partial refresh
# left ~50 stale references behind. This check binds each comment to the
# pinned checkout in both directions:
#   1. the rpi source still cites the expected `(<basename>:<range>)`, and
#   2. the cited range in external/pi still contains the expected symbol.
#
# Row format: <rpi file>@<path under external/pi>@<cited anchor>@<regex>
# Lines starting with `#` and empty lines are skipped. The one anchor outside
# external/pi (quickjs-wasi `src/index.ts:1041-1046` in
# runtime/worker.rs) is intentionally not in the table: the vendored package
# ships no TypeScript source to check against.
#
# Exit code 0 on success, 1 with a diagnostic per failure.
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ ! -d external/pi ]]; then
    echo "error: external/pi is not checked out (run from a full checkout)" >&2
    exit 1
fi

failures=0

check() {
    local rpi="$1" up="$2" anchor="$3" regex="$4"
    # The anchor may sit bare in a parenthetical (`(..., host.ts:22)`) or on a
    # continuation line, so match the literal anchor followed by a
    # non-digit/non-hyphen boundary instead of requiring `(anchor)`.
    local anchor_pattern
    anchor_pattern="$(printf '%s' "$anchor" | sed 's/\./\\./g')([^0-9-]|$)"
    if ! grep -qE "$anchor_pattern" "$rpi"; then
        echo "error: $rpi does not cite $anchor" >&2
        failures=$((failures + 1))
    fi
    local range="${anchor#*:}"
    local start="${range%%-*}"
    local end="${range##*-}"
    if ! awk -v s="$start" -v e="$end" 'NR >= s && NR <= e' "external/pi/$up" | grep -qE "$regex"; then
        echo "error: external/pi/$up:$start-$end does not contain /$regex/" >&2
        failures=$((failures + 1))
    fi
}

while IFS='@' read -r rpi up anchor regex; do
    if [[ -z "$rpi" || "$rpi" == \#* ]]; then
        continue
    fi
    check "$rpi" "$up" "$anchor" "$regex"
done <<'ANCHORS'
crates/rpi/src/extensions/codemode/execute.rs@packages/coding-agent/src/extensions/codemode/execute.ts@execute.ts:50-51@const MAX_CONCURRENT_MODEL_CALLS = 4
crates/rpi/src/extensions/codemode/execute.rs@packages/coding-agent/src/extensions/codemode/execute.ts@execute.ts:52-57@const CODEMODE_MEMORY_LIMIT_BYTES = 256
crates/rpi/src/extensions/codemode/execute.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:105-117@export interface CodemodeNestedCall \{
crates/rpi/src/extensions/codemode/execute.rs@packages/coding-agent/src/extensions/codemode/execute.ts@execute.ts:91-96@function toModelInfo
crates/rpi/src/extensions/codemode/execute.rs@packages/coding-agent/src/extensions/codemode/execute.ts@execute.ts:122-157@function checkClassifierContext
crates/rpi/src/extensions/codemode/execute.rs@packages/coding-agent/src/extensions/codemode/execute.ts@execute.ts:159-183@function checkImagesContext
crates/rpi/src/extensions/codemode/execute.rs@packages/coding-agent/src/extensions/codemode/execute.ts@execute.ts:272-300@function truncateOutput
crates/rpi/src/extensions/codemode/execute.rs@packages/coding-agent/src/extensions/codemode/execute.ts@execute.ts:219-230@function readCodemodeStore
crates/rpi/src/extensions/codemode/execute.rs@packages/coding-agent/src/extensions/codemode/execute.ts@execute.ts:436-445@function isNamespaceName
crates/rpi/src/extensions/codemode/execute.rs@packages/coding-agent/src/extensions/codemode/execute.ts@execute.ts:447-517@function createDiscoveryGlobals
crates/rpi/src/extensions/codemode/execute.rs@packages/coding-agent/src/extensions/codemode/execute.ts@execute.ts:316-434@export async function executeCodemode
crates/rpi/src/extensions/codemode/execute.rs@packages/coding-agent/src/extensions/codemode/execute.ts@execute.ts:302-313@function toScriptValue
crates/rpi/src/extensions/codemode/execute.rs@packages/coding-agent/src/extensions/codemode/execute.ts@execute.ts:519-630@function createModelGlobals
crates/rpi-codemode/src/source.rs@packages/codemode/src/source.ts@source.ts:11@CODEMODE_OPTIONS_PREFIX
crates/rpi-codemode/src/source.rs@packages/codemode/src/source.ts@source.ts:18-30@CODEMODE_SOURCE_GRAMMAR
crates/rpi-codemode/src/source.rs@packages/codemode/src/source.ts@source.ts:15-16@const MAX_TIMEOUT_MS
crates/rpi-codemode/src/source.rs@packages/codemode/src/source.ts@source.ts:32-37@export interface CodemodeSourceOptions
crates/rpi-codemode/src/source.rs@packages/codemode/src/source.ts@source.ts:39-43@export interface ParsedCodemodeSource
crates/rpi-codemode/src/source.rs@packages/codemode/src/source.ts@source.ts:45-50@export class CodemodeSourceError
crates/rpi-codemode/src/source.rs@packages/codemode/src/source.ts@source.ts:52-54@function isSafeInteger
crates/rpi-codemode/src/declarations.rs@packages/codemode/src/declarations.ts@declarations.ts:10@DEFAULT_INPUT_SCHEMA_MAX_CHARS
crates/rpi-codemode/src/declarations.rs@packages/codemode/src/declarations.ts@declarations.ts:12@MAX_REF_EXPANSIONS
crates/rpi-codemode/src/declarations.rs@packages/codemode/src/declarations.ts@declarations.ts:18-93@export const MCP_TYPESCRIPT_PREAMBLE
crates/rpi-codemode/src/declarations.rs@packages/codemode/src/declarations.ts@declarations.ts:95-98@export interface RenderDeclarationsOptions
crates/rpi-codemode/src/declarations.rs@packages/codemode/src/declarations.ts@declarations.ts:149-159@export function renderToolSample
crates/rpi-codemode/src/declarations.rs@packages/codemode/src/declarations.ts@declarations.ts:161-175@export function mcpStructuredContentSchema
crates/rpi-codemode/src/declarations.rs@packages/codemode/src/declarations.ts@declarations.ts:221-308@export function schemaToType
crates/rpi-codemode/src/types.rs@packages/codemode/src/types.ts@types.ts:14-41@export interface CodemodeTool \{
crates/rpi-codemode/src/types.rs@packages/codemode/src/declarations.ts@declarations.ts:132-147@export function renderToolSignature
crates/rpi-codemode/src/types.rs@packages/codemode/src/types.ts@types.ts:49@export type CodemodeCallStatus
crates/rpi-codemode/src/types.rs@packages/codemode/src/types.ts@types.ts:51-55@export interface CodemodeCall \{
crates/rpi-codemode/src/types.rs@packages/codemode/src/types.ts@types.ts:57-65@export type CodemodeErrorKind
crates/rpi-codemode/src/types.rs@packages/codemode/src/types.ts@types.ts:67-73@export interface CodemodeError \{
crates/rpi-codemode/src/types.rs@packages/codemode/src/types.ts@types.ts:82-90@export type CodemodeResult
crates/rpi-codemode/src/types.rs@packages/codemode/src/types.ts@types.ts:92-125@export interface CodemodeSandboxOptions
crates/rpi-codemode/src/types.rs@packages/codemode/src/types.ts@types.ts:127-136@export interface CodemodeExecuteOptions
crates/rpi-codemode/src/types.rs@packages/codemode/src/runtime/host.ts@host.ts:22@const DEFAULT_TIMEOUT_MS
crates/rpi-codemode/src/types.rs@packages/codemode/src/runtime/prelude-source.ts@prelude-source.ts:28-29@MAX_STORE_VALUE_CHARS = 256 \* 1024
crates/rpi-codemode/src/types.rs@packages/codemode/src/runtime/host.ts@host.ts:24-35@const RESERVED_GLOBALS
crates/rpi-codemode/src/runtime/host.rs@packages/codemode/src/runtime/host.ts@host.ts:63-67@interface PendingCall
crates/rpi-codemode/src/runtime/host.rs@packages/codemode/src/runtime/host.ts@host.ts:242-253@private finish
crates/rpi-codemode/src/runtime/host.rs@packages/codemode/src/runtime/host.ts@host.ts:49-56@function parseStoreWrites
crates/rpi-codemode/src/runtime/host.rs@packages/codemode/src/runtime/host.ts@host.ts:201-208@private handleDone
crates/rpi-codemode/src/runtime/worker.rs@packages/codemode/src/runtime/worker.ts@worker.ts:118-122@const drain = 
crates/rpi-codemode/src/runtime/worker.rs@packages/codemode/src/runtime/worker.ts@worker.ts:32-44@function discardOutput
crates/rpi-codemode/src/runtime/worker.rs@packages/codemode/src/runtime/worker.ts@worker.ts:65-100@const bridge = vm.newFunction
crates/rpi-codemode/src/quickjs.rs@packages/codemode/src/runtime/worker.ts@worker.ts:46-50@function describeException
crates/rpi/src/extensions/tool_search.rs@packages/coding-agent/src/extensions/tool-search/tool.ts@tool.ts:29-32@export interface ToolSearchMatch
crates/rpi/src/extensions/tool_search.rs@packages/coding-agent/src/extensions/tool-search/tool.ts@tool.ts:39-61@const STOP_WORDS
crates/rpi/src/extensions/tool_search.rs@packages/coding-agent/src/extensions/tool-search/tool.ts@tool.ts:63-69@function stem
crates/rpi/src/extensions/tool_search.rs@packages/coding-agent/src/extensions/tool-search/tool.ts@tool.ts:71-80@export function tokenize
crates/rpi/src/extensions/tool_search.rs@packages/coding-agent/src/extensions/tool-search/tool.ts@tool.ts:86-101@function schemaText
crates/rpi/src/extensions/tool_search.rs@packages/coding-agent/src/extensions/tool-search/tool.ts@tool.ts:103-116@export function createToolSearchDocument
crates/rpi/src/extensions/tool_search.rs@packages/coding-agent/src/extensions/tool-search/tool.ts@tool.ts:118-157@export class Bm25Ranker
crates/rpi/src/extensions/tool_search.rs@packages/coding-agent/src/extensions/tool-search/tool.ts@tool.ts:159-167@export const toolSearchSchema
crates/rpi/src/extensions/tool_search.rs@packages/coding-agent/src/extensions/tool-search/tool.ts@tool.ts:192-194@function isSearchable
crates/rpi/src/extensions/tool_search.rs@packages/coding-agent/src/extensions/tool-search/tool.ts@tool.ts:196-214@function searchAndLoad
crates/rpi/src/extensions/tool_search.rs@packages/coding-agent/src/extensions/tool-search/tool.ts@tool.ts:220@export const TOOL_SEARCH_DESCRIPTION
crates/rpi/src/extensions/tool_search.rs@packages/coding-agent/src/extensions/tool-search/tool.ts@tool.ts:222-247@export function createToolSearchToolDefinition
crates/rpi/src/extensions/tool_search.rs@packages/coding-agent/src/extensions/tool-search/tool.ts@tool.ts:233@async execute\(_toolCallId
crates/rpi/src/extensions/codemode/description.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:156@const CHARS_PER_TOKEN = 4
crates/rpi/src/extensions/codemode/description.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:133@export const CODEMODE_DOCS_PATH
crates/rpi/src/extensions/codemode/description.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:140-151@function describeGlobals
crates/rpi/src/extensions/codemode/description.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:173-185@export interface CodemodeDescriptionOptions
crates/rpi/src/extensions/codemode/description.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:188-192@function renderToolSection
crates/rpi/src/extensions/codemode/description.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:211-235@function selectCatalog
crates/rpi/src/extensions/codemode/description.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:237-291@export function createCodemodeDescription
crates/rpi/src/extensions/codemode/description.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:293-313@function describeOutput
crates/rpi/src/extensions/codemode/description.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:315-327@function describeScriptCall
crates/rpi/src/extensions/codemode/tool.rs@packages/coding-agent/src/core/settings-manager.ts@settings-manager.ts:103-108@export interface CodemodeSettings
crates/rpi/src/extensions/codemode/tool.rs@packages/coding-agent/src/extensions/codemode/index.ts@index.ts:20-30@function readMode
crates/rpi/src/extensions/codemode/tool.rs@packages/coding-agent/src/core/agent-session.ts@agent-session.ts:1465-1476@getAllTools
crates/rpi/src/extensions/codemode/tool.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:87-91@export const codemodeSchema
crates/rpi/src/extensions/codemode/tool.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:169-171@export function getCodemodeCallableTools
crates/rpi/src/extensions/codemode/tool.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:329-363@function prepareCodemodeLoadout
crates/rpi/src/extensions/codemode/tool.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:365-404@export function createCodemodeToolDefinition
crates/rpi/src/extensions/codemode/tool.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:377@exposure: "model-only"
crates/rpi/src/extensions/codemode/tool.rs@packages/coding-agent/src/extensions/codemode/tool.ts@tool.ts:380@constrainedSampling: \{ type: "grammar"
ANCHORS

if ((failures > 0)); then
    echo "error: $failures upstream anchor check(s) failed" >&2
    exit 1
fi
echo "ok: upstream anchors verified"