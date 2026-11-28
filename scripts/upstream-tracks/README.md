# Pi target and regression tracks

`pi.json` is the read-only source-of-truth for the v0.1.9 Pi tracks. The Git tree IDs are the source-input hashes: they identify the complete trees independently of a mutable branch or working-copy path.

| Track | Tag | Commit | Tree |
|---|---|---|---|
| regression | v1.4.0 | `4a080252f06caf63a525d7873c5d94d3c23887e0` | `56ea45bfd3ef589feaddfaf504cfc08d838d812b` |
| target | v1.5.2 | `b4afdddf44e059c67ccf5097e5317d913521b470` | `ed418d79cb44d3690f110e1077c4612db033f2c9` |

Verify both tags, their peeled commits and trees, ancestry, and the 703-commit / 3211-file range:

```bash
python3 scripts/verify-pi-tracks.py
```

Read a target source file or create an out-of-repository snapshot without changing the submodule checkout:

```bash
git -C external/pi show b4afdddf44e059c67ccf5097e5317d913521b470:<path>
git -C external/pi archive b4afdddf44e059c67ccf5097e5317d913521b470 | tar -x -C <empty-out-of-repo-dir>
```

Revalidate the remote tag before relying on a newly fetched object. `main` and post-tag commits are not part of either track:

```bash
git -C external/pi ls-remote origin refs/tags/v1.5.2 refs/tags/v1.5.2^{}
```

The target tag was checked against ADR-0048 during V19-00. Any newer stable tag or changed target requires another ADR review; do not edit this configuration to follow `main`.
