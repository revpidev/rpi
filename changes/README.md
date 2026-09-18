# Version changelog (changes/)

One file per version: `v<version>.md`. The file's content is the single source of truth for that release's changelog.

- Timing: finalized before each release (derived from the git log and the task/ADR documents in the rpi-docs repository), merged with the release PR, then tagged.
- Sync: the revpi.dev changelog page (`changelog.html` in the rpi-pages repository) records the same content and is updated together with site generation at release time.
- History: this directory keeps a full record of every version; `v0.1.0.md` was backfilled during the v0.1.2 cleanup.
