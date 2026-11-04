#!/usr/bin/env python3
"""Generate the rpi-statusline bundled tokenizer assets.

The statusline estimates streaming output tokens locally (providers report
exact usage only at message end). It used to rely on a user-local
``~/.rpi/agent/tokenizer`` registry; this script converts the official
tokenizer tables of the supported open model families into a compact,
zstd-compressed binary and checks the result in under
``crates/rpi-ext-statusline/assets/tokenizers/``. The plugin embeds those
assets with ``include_bytes!``, so it is self-contained regardless of how it
was installed.

The compact format stores token **ids** (not strings) for the merge table:

    magic  b"RPI-TOK1"
    u8     engine     0 = HF pair-merge BPE, 1 = tiktoken rank BPE
    u8     flags      bit0 = ignore_merges
    u8     normalizer 0 = none, 1 = NFC
    u8     reserved
    u16    pattern count
    ...    (u8 behavior, u8 invert, u16 reserved, u32 len, bytes regex)
    u32    added-token count
    ...    (u32 len, bytes token)
    u32    rank count
    HF:    (u32 left_id, u32 right_id, u32 result_id) per rank
    TIK:   (u32 len, bytes token) per rank
    u32    base ids[256]  (byte -> token id for the first BPE symbol)
    if ignore_merges:
      u32 vocab count, (u32 len, bytes token) in id order

The assets are only data; ``tokens.rs`` implements the two BPE engines and
the HF pretokenizer/normalizer semantics.

Usage:
    pip install tokenizers tiktoken zstandard
    python3 scripts/gen-statusline-tokenizers.py --fetch
    python3 scripts/gen-statusline-tokenizers.py --check

The source revisions and sha256 values below are pinned. ``--fetch``
downloads into ``--cache`` (default: ``/tmp/rpi-statusline-tokenizers``) and
verifies every hash before writing anything.
"""

from __future__ import annotations

import argparse
import ast
import base64
import hashlib
import json
import re
import struct
import sys
import tempfile
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ASSET_DIR = ROOT / "crates/rpi-ext-statusline/assets/tokenizers"
VECTOR_PATH = (
    ROOT / "crates/rpi-ext-statusline/tests/fixtures/statusline_tokenizer_vectors.json"
)
MANIFEST_PATH = ASSET_DIR / "manifest.json"
NOTICE_PATH = ROOT / "crates/rpi-ext-statusline/NOTICE.md"

MAGIC = b"RPI-TOK1"
FORMAT_VERSION = 1

HF = 0
TIKTOKEN = 1

# `bytes_to_unicode` from GPT-2/ByteLevel tokenizers.
BYTE_ENCODER = None


def byte_encoder() -> dict[int, str]:
    global BYTE_ENCODER
    if BYTE_ENCODER is None:
        bs = (
            list(range(ord("!"), ord("~") + 1))
            + list(range(ord("¡"), ord("¬") + 1))
            + list(range(ord("®"), ord("ÿ") + 1))
        )
        cs = list(bs)
        n = 0
        for b in range(256):
            if b not in bs:
                bs.append(b)
                cs.append(256 + n)
                n += 1
        BYTE_ENCODER = {b: chr(c) for b, c in zip(bs, cs)}
    return BYTE_ENCODER


SOURCES = {
    "deepseek": {
        "display": "DeepSeek",
        "patterns": ["deepseek"],
        "repo": "deepseek-ai/DeepSeek-V3.2",
        "commit": "a7e62ac04ecb2c0a54d736dc46601c5606cf10a6",
        "license": "MIT",
        "license_url": "https://huggingface.co/deepseek-ai/DeepSeek-V3.2/blob/main/LICENSE",
        "kind": "hf",
        "files": {
            "tokenizer.json": "cd050be35cae877f8f0aa847f45aa87e23835a56ca32b29b28545597852784e5",
        },
    },
    "kimi": {
        "display": "Kimi",
        "patterns": ["kimi", "moonshot"],
        "repo": "moonshotai/Kimi-K2.7-Code",
        "commit": "74797c9c62378b951a1f6fcf5c4631024e9b8bef",
        "license": "Modified MIT",
        "license_url": "https://huggingface.co/moonshotai/Kimi-K2.7-Code/blob/main/LICENSE",
        "kind": "tiktoken",
        "files": {
            "tiktoken.model": "b6c497a7469b33ced9c38afb1ad6e47f03f5e5dc05f15930799210ec050c5103",
            "tokenizer_config.json": "6dfafeacca3d44748fc4a2fcc6725be5ea616ecc5b95f5a8b4277077c06e7d9f",
            "tokenization_kimi.py": "88394687bd081f55928849b2d140e190c70297660d3fb8b7c54c0e5633b177e3",
        },
    },
    "glm": {
        "display": "GLM",
        "patterns": ["glm"],
        "repo": "zai-org/GLM-5",
        "commit": "c183ef8c61faee82855eca1ed9bb3a9a7ce3b0b2",
        "license": "MIT",
        "license_url": "https://huggingface.co/zai-org/GLM-5",
        "kind": "hf",
        "files": {
            "tokenizer.json": "19e773648cb4e65de8660ea6365e10acca112d42a854923df93db4a6f333a82d",
        },
    },
    "qwen": {
        "display": "Qwen",
        "patterns": ["qwen", "qwq"],
        "repo": "Qwen/Qwen3.5-397B-A17B",
        "commit": "8472618112abcbd45acbcdc58436aff4233c23f7",
        "license": "Apache-2.0",
        "license_url": "https://huggingface.co/Qwen/Qwen3.5-397B-A17B/blob/main/LICENSE",
        "kind": "hf",
        "files": {
            "tokenizer.json": "5f9e4d4901a92b997e463c1f46055088b6cca5ca61a6522d1b9f64c4bb81cb42",
        },
    },
    "hunyuan": {
        "display": "Hunyuan",
        "patterns": ["hy3", "hy4"],
        "repo": "tencent/Hunyuan-A13B-Instruct",
        "commit": "290ddb9a56ed23c2c83a1c8081533e58925df952",
        "license": "Tencent Hunyuan Community License",
        "license_url": "https://huggingface.co/tencent/Hunyuan-A13B-Instruct/blob/main/LICENSE",
        "kind": "hf",
        "files": {
            "tokenizer.json": "4185389a69284fdd3ebf737c6120bbeee36cbed9763151fa5bac5986a39e05d4",
        },
    },
    "minimax": {
        "display": "MiniMax",
        "patterns": ["minimax"],
        "repo": "MiniMaxAI/MiniMax-M2.7",
        "commit": "0a6536713b0b03f3be97494e5cf8d22f668a3164",
        "license": "MiniMax Non-Commercial License",
        "license_url": "https://huggingface.co/MiniMaxAI/MiniMax-M2.7/blob/main/LICENSE",
        "kind": "hf",
        "files": {
            "tokenizer.json": "757622126525aeeb131756849d93298070ff3f0319c455ec8c5bb0f6b1cebbe8",
        },
    },
}

# A shared corpus plus family-specific special tokens. The vectors are
# generated from the official tokenizer implementations; the Rust unit test
# replays them against the embedded assets.
CORPUS = [
    "",
    "hello world",
    "Hello, world!",
    "The quick brown fox jumps over the lazy dog.",
    "你好，世界！",
    "这是一个测试句子。",
    "中文English混合123",
    "a   b",
    "a\t\tb",
    "a\n\nb",
    "  leading",
    "trailing   ",
    "   \t\n  ",
    "1 22 333 4444 55555",
    "don't stop believin'",
    "I'll've done it",
    "fn main() { println!(\"hi\"); }",
    "{\"key\": [1, 2, 3], \"ok\": true}",
    "https://example.com/path?q=1&x=2",
    "emoji 😀🚀🎉 test",
    "ёжик café naïve",
    "def foo(x: int) -> int:\n    return x + 1\n",
    "SELECT * FROM t WHERE id = 42;",
    "line1\nline2\r\nline3",
    "A" * 100,
    "中" * 100,
    "😀" * 20,
    "repeated repeated repeated repeated",
    "0.123456789e-10",
    "'s 't 're 've 'm 'll 'd",
    "<|im_start|>user hello<|im_end|>",
    "<|begin_of_sentence|>hi<|end_of_sentence|>",
]

SPECIAL_CORPUS = {
    "deepseek": [
        "<\uff5cbegin\u2581of\u2581sentence\uff5c>hi<\uff5cend\u2581of\u2581sentence\uff5c>",
        "<\uff5c\u2581pad\u2581\uff5c>",
        "<\uff5ctool\u2581sep\uff5c><\uff5cAssistant\uff5c>",
    ],
    "kimi": [
        "[BOS]hello[EOS]",
        "<|im_end|>user<|im_user|>test",
    ],
    "glm": [
        "[gMASK]<sop><|user|>hello<|assistant|>",
        "[MASK][sMASK]",
    ],
    "qwen": [
        "<|im_start|>user hello<|im_end|>",
        "<|endoftext|>",
    ],
    "hunyuan": [
        "<|startoftext|>hello<|endoftext|>",
        "<|extra_0|>test<|extra_204|>",
    ],
    "minimax": [
        "]!p~[<fim_prefix>hello<fim_middle>",
        "<fim_suffix><fim_pad>",
    ],
}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def fetch(cache: Path, repo: str, commit: str, name: str, expected: str) -> Path:
    path = cache / repo.replace("/", "--") / commit / name
    if not path.is_file():
        url = f"https://huggingface.co/{repo}/resolve/{commit}/{name}"
        path.parent.mkdir(parents=True, exist_ok=True)
        print(f"fetch {url}")
        request = urllib.request.Request(url, headers={"User-Agent": "rpi-statusline-assets/1"})
        with urllib.request.urlopen(request, timeout=120) as response:
            data = response.read()
        path.write_bytes(data)
    actual = sha256(path)
    if actual != expected:
        path.unlink(missing_ok=True)
        raise SystemExit(f"{path}: sha256 {actual} != pinned {expected}")
    return path


def parse_merges(raw_merges: list) -> list[tuple[str, str]]:
    merges = []
    for item in raw_merges:
        if isinstance(item, str):
            left, right = item.split(" ", 1)
        elif isinstance(item, list) and len(item) == 2:
            left, right = item
        else:
            raise SystemExit(f"unsupported merge entry: {item!r}")
        merges.append((left, right))
    return merges


def parse_normalizer(raw) -> int:
    if raw is None:
        return 0
    if raw.get("type") == "NFC":
        return 1
    if raw.get("type") == "Sequence" and not raw.get("normalizers"):
        return 0
    raise SystemExit(f"unsupported normalizer: {raw!r}")


def parse_pretokenizer(raw) -> list[tuple[int, int, str]]:
    if raw is None:
        return []
    if raw.get("type") != "Sequence":
        raise SystemExit(f"unsupported pre_tokenizer: {raw!r}")
    patterns = []
    saw_byte_level = False
    for pretok in raw.get("pretokenizers", []):
        kind = pretok.get("type")
        if kind == "ByteLevel":
            # The runtime byte-maps each Split piece; a ByteLevel step that
            # runs before a later Split would change the split input.
            if pretok.get("use_regex"):
                raise SystemExit("ByteLevel.use_regex=true is not supported")
            if pretok.get("add_prefix_space"):
                raise SystemExit("ByteLevel.add_prefix_space=true is not supported")
            saw_byte_level = True
            continue
        if kind != "Split":
            raise SystemExit(f"unsupported pretokenizer step: {pretok!r}")
        if saw_byte_level:
            raise SystemExit("Split after ByteLevel is not supported")
        pattern = pretok.get("pattern", {}).get("Regex")
        if not isinstance(pattern, str):
            raise SystemExit(f"unsupported Split pattern: {pretok!r}")
        behavior = pretok.get("behavior")
        invert = bool(pretok.get("invert"))
        # tiktoken-converted tokenizers ship `(Removed, invert)` for a
        # whole-covering GPT regex; HF canonicalizes that pair to
        # `(Isolated, !invert)` (see tokenizers' Split::canonicalized_for_pipeline).
        if behavior == "Removed" and invert:
            behavior, invert = "Isolated", False
        if behavior != "Isolated" or invert:
            raise SystemExit(f"unsupported Split behavior: {behavior} invert={invert}")
        patterns.append((0, 0, pattern))
    return patterns


def hf_asset(path: Path) -> dict:
    raw = json.loads(path.read_text())
    model = raw["model"]
    if model.get("type") != "BPE":
        raise SystemExit(f"unsupported model type: {model.get('type')!r}")
    if model.get("byte_fallback"):
        raise SystemExit("byte_fallback is not supported")
    for field in ("continuing_subword_prefix", "end_of_word_suffix", "unk_token"):
        if model.get(field):
            raise SystemExit(f"{field} is not supported: {model.get(field)!r}")
    if model.get("fuse_unk"):
        raise SystemExit("fuse_unk is not supported")
    vocab = model.get("vocab", {})
    if not vocab:
        raise SystemExit("tokenizer has no vocab")
    if sorted(vocab.values()) != list(range(len(vocab))):
        raise SystemExit("vocab ids are not sequential")
    id_to_token = [None] * len(vocab)
    for token, token_id in vocab.items():
        id_to_token[token_id] = token
    base_ids = [vocab[byte_encoder()[b]] for b in range(256)]

    merges = parse_merges(model.get("merges", []))
    pairs = []
    for left, right in merges:
        if left not in vocab or right not in vocab or left + right not in vocab:
            raise SystemExit(f"merge operands are missing from the vocab: {left!r}+{right!r}")
        pairs.append((vocab[left], vocab[right], vocab[left + right]))

    ignore_merges = bool(model.get("ignore_merges"))
    vocab_strings = id_to_token if ignore_merges else []
    normalizer = parse_normalizer(raw.get("normalizer"))
    added = []
    for token in raw.get("added_tokens", []):
        # The runtime extracts added tokens from the raw text with plain
        # substring semantics and counts each as one token. `normalized`
        # tokens are equivalent only when the normalizer is a no-op
        # (DeepSeek's twenty-one text tokens); an NFC tokenizer with
        # normalized added tokens would need a second matcher pass.
        if token.get("normalized") and normalizer != 0:
            raise SystemExit(f"normalized added token is not supported: {token['content']!r}")
        if token.get("lstrip") or token.get("rstrip") or token.get("single_word"):
            raise SystemExit(f"added-token flags are not supported: {token['content']!r}")
        added.append(token["content"])
    patterns = parse_pretokenizer(raw.get("pre_tokenizer"))
    return {
        "engine": HF,
        "flags": 1 if ignore_merges else 0,
        "normalizer": normalizer,
        "patterns": patterns,
        "added": added,
        "pairs": pairs,
        "base_ids": base_ids,
        "vocab": vocab_strings,
    }


def parse_tiktoken_pattern(path: Path) -> str:
    source = path.read_text()
    match = re.search(r'pat_str\s*=\s*"\|"\.join\(\s*(\[.*?\])\s*\)', source, re.S)
    if not match:
        raise SystemExit(f"cannot find pat_str in {path}")
    parts = ast.literal_eval(match.group(1))
    return "|".join(parts)


def tiktoken_asset(files: dict[str, Path]) -> dict:
    tokens = []
    rank_by_bytes: dict[bytes, int] = {}
    for line in files["tiktoken.model"].read_bytes().splitlines():
        if not line.strip():
            continue
        encoded, raw_id = line.split()
        token = base64.b64decode(encoded)
        token_id = int(raw_id)
        if token_id != len(tokens):
            raise SystemExit(f"tiktoken ids are not sequential at {token_id}")
        tokens.append(token)
        rank_by_bytes[token] = token_id
    if len(rank_by_bytes) != len(tokens):
        raise SystemExit("duplicate tokens in tiktoken.model")
    base_ids = []
    for byte in range(256):
        rank = rank_by_bytes.get(bytes([byte]))
        if rank is None:
            raise SystemExit(f"tiktoken.model has no single-byte token {byte}")
        base_ids.append(rank)

    config = json.loads(files["tokenizer_config.json"].read_text())
    decoder = {
        int(token_id): entry["content"]
        for token_id, entry in config.get("added_tokens_decoder", {}).items()
    }
    num_base = len(tokens)
    added = [
        decoder.get(num_base + i, f"<|reserved_token_{i}|>")
        for i in range(256)
    ]
    pattern = parse_tiktoken_pattern(files["tokenization_kimi.py"])
    return {
        "engine": TIKTOKEN,
        "flags": 0,
        "normalizer": 0,
        "patterns": [(0, 0, pattern)],
        "added": added,
        "tokens": tokens,
        "base_ids": base_ids,
        "vocab": [],
    }


def pack_asset(asset: dict) -> bytes:
    buffer = bytearray()
    buffer += MAGIC
    buffer += struct.pack(
        "<BBBBH",
        asset["engine"],
        asset["flags"],
        asset["normalizer"],
        0,
        len(asset["patterns"]),
    )
    for behavior, invert, pattern in asset["patterns"]:
        data = pattern.encode()
        buffer += struct.pack("<BBHI", behavior, invert, 0, len(data)) + data
    buffer += struct.pack("<I", len(asset["added"]))
    for token in asset["added"]:
        data = token.encode()
        buffer += struct.pack("<I", len(data)) + data
    if asset["engine"] == HF:
        buffer += struct.pack("<I", len(asset["pairs"]))
        for left, right, result in asset["pairs"]:
            buffer += struct.pack("<III", left, right, result)
    else:
        buffer += struct.pack("<I", len(asset["tokens"]))
        for token in asset["tokens"]:
            buffer += struct.pack("<I", len(token)) + token
    for token_id in asset["base_ids"]:
        buffer += struct.pack("<I", token_id)
    if asset["flags"] & 1:
        buffer += struct.pack("<I", len(asset["vocab"]))
        for token in asset["vocab"]:
            data = token.encode()
            buffer += struct.pack("<I", len(data)) + data
    return bytes(buffer)


def compress(raw: bytes) -> bytes:
    try:
        import zstandard
    except ImportError:
        raise SystemExit("zstandard is required: pip install zstandard")
    return zstandard.ZstdCompressor(level=19).compress(raw)


def generate_vectors(source: dict, files: dict[str, Path]) -> list[dict]:
    texts = CORPUS + SPECIAL_CORPUS[source["id"]]
    vectors = []
    if source["kind"] == "hf":
        from tokenizers import Tokenizer

        tokenizer = Tokenizer.from_file(str(files["tokenizer.json"]))
        for text in texts:
            count = len(tokenizer.encode(text, add_special_tokens=False).ids)
            vectors.append({"text": text, "count": count})
    else:
        import tiktoken

        tokens = []
        for line in files["tiktoken.model"].read_bytes().splitlines():
            if not line.strip():
                continue
            encoded, raw_id = line.split()
            token = base64.b64decode(encoded)
            tokens.append((token, int(raw_id)))
        ranks = {token: token_id for token, token_id in tokens}
        config = json.loads(files["tokenizer_config.json"].read_text())
        decoder = {
            int(token_id): entry["content"]
            for token_id, entry in config.get("added_tokens_decoder", {}).items()
        }
        num_base = len(tokens)
        specials = {
            decoder.get(num_base + i, f"<|reserved_token_{i}|>"): num_base + i
            for i in range(256)
        }
        pattern = parse_tiktoken_pattern(files["tokenization_kimi.py"])
        encoding = tiktoken.Encoding(
            name=source["id"],
            pat_str=pattern,
            mergeable_ranks=ranks,
            special_tokens=specials,
        )
        for text in texts:
            vectors.append({"text": text, "count": len(encoding.encode(text, allowed_special="all"))})
    return vectors


def notice(manifest: dict) -> str:
    lines = [
        "# Bundled tokenizer assets",
        "",
        "The `assets/tokenizers/*.tok.zst` files in this crate are compact,",
        "derived tokenizer tables (merge ranks, byte ids, added-token surfaces",
        "and pretokenizer patterns) generated by",
        "`scripts/gen-statusline-tokenizers.py`. They are embedded in the",
        "`rpi-statusline` plugin so live token counting does not depend on a",
        "user-local tokenizer installation.",
        "",
        "Each table is derived from the official tokenizer published with the",
        "model named below. The upstream model licenses apply to the derived",
        "data; the plugin source code remains MIT-licensed.",
        "",
        "Matching is family-based: a newer checkpoint that shares the family",
        "tokenizer uses the same table. Refresh the pinned revision when the",
        "upstream tokenizer changes. `hy3`/`hy4` currently map to the newest",
        "published Hunyuan tokenizer (Hunyuan-A13B).",
        "",
    ]
    for family in manifest["families"]:
        lines.extend(
            [
                f"## {family['display']} (`{family['id']}`)",
                "",
                f"- Source: <https://huggingface.co/{family['repo']}/tree/{family['commit']}>",
                f"- License: [{family['license']}]({family['licenseUrl']})",
                "- Files:",
            ]
        )
        for item in family["sources"]:
            lines.append(f"  - `{item['file']}` sha256 `{item['sha256']}`")
        lines.extend(
            [
                f"- Asset: `{family['asset']}` sha256 `{family['assetSha256']}`",
                "",
            ]
        )
    return "\n".join(lines).rstrip() + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cache",
        type=Path,
        default=Path(tempfile.gettempdir()) / "rpi-statusline-tokenizers",
    )
    parser.add_argument("--fetch", action="store_true", help="download missing sources")
    parser.add_argument("--check", action="store_true", help="fail when generated files differ")
    parser.add_argument("--skip-vectors", action="store_true")
    args = parser.parse_args()

    ASSET_DIR.mkdir(parents=True, exist_ok=True)
    VECTOR_PATH.parent.mkdir(parents=True, exist_ok=True)

    generated: list[tuple[Path, bytes]] = []
    families = []
    vectors = {}
    for family_id, spec in SOURCES.items():
        spec = dict(spec, id=family_id)
        files = {}
        for name, expected in spec["files"].items():
            path = args.cache / spec["repo"].replace("/", "--") / spec["commit"] / name
            if args.fetch:
                path = fetch(args.cache, spec["repo"], spec["commit"], name, expected)
            if not path.is_file():
                raise SystemExit(f"missing source {path}; rerun with --fetch")
            if sha256(path) != expected:
                raise SystemExit(f"{path}: sha256 does not match the pinned value")
            files[name] = path
        asset = hf_asset(files["tokenizer.json"]) if spec["kind"] == "hf" else tiktoken_asset(files)
        raw = pack_asset(asset)
        compressed = compress(raw)
        asset_name = f"{family_id}.tok.zst"
        generated.append((ASSET_DIR / asset_name, compressed))
        families.append(
            {
                "id": family_id,
                "display": spec["display"],
                "patterns": spec["patterns"],
                "repo": spec["repo"],
                "commit": spec["commit"],
                "license": spec["license"],
                "licenseUrl": spec["license_url"],
                "sources": [
                    {"file": name, "sha256": expected}
                    for name, expected in spec["files"].items()
                ],
                "asset": asset_name,
                "assetSha256": hashlib.sha256(compressed).hexdigest(),
                "uncompressedBytes": len(raw),
                "engine": "hf-bpe" if asset["engine"] == HF else "tiktoken-bpe",
                "ignoreMerges": bool(asset["flags"] & 1),
                "normalizer": "nfc" if asset["normalizer"] == 1 else "none",
                "pretokenizers": len(asset["patterns"]),
                "addedTokens": len(asset["added"]),
                "ranks": len(asset["pairs"]) if asset["engine"] == HF else len(asset["tokens"]),
            }
        )
        if not args.skip_vectors:
            vectors[family_id] = generate_vectors(spec, files)
        print(f"{family_id}: {len(raw)} bytes -> {len(compressed)} bytes")

    manifest = {
        "formatVersion": FORMAT_VERSION,
        "generatedBy": "scripts/gen-statusline-tokenizers.py",
        "families": families,
    }
    manifest_bytes = (json.dumps(manifest, indent=2) + "\n").encode()
    generated.append((MANIFEST_PATH, manifest_bytes))
    generated.append((NOTICE_PATH, notice(manifest).encode()))
    if not args.skip_vectors:
        vectors_bytes = (json.dumps(vectors, indent=2, ensure_ascii=False) + "\n").encode()
        generated.append((VECTOR_PATH, vectors_bytes))

    if args.check:
        failed = False
        for path, data in generated:
            if not path.is_file() or path.read_bytes() != data:
                print(f"stale: {path}")
                failed = True
        return 1 if failed else 0

    for path, data in generated:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        print(f"wrote {path.relative_to(ROOT)} ({len(data)} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())