// Throwing stub for the Pi host packages referenced by upstream modules.
// Upstream imports them type-only (`import type { ExtensionUIContext } …`),
// which tsx erases — reaching this stub at runtime means the driver started
// depending on host code that has no rpi equivalent here.

throw new Error(
  "parity-host-stub: host package reached at runtime — the parity drivers must keep host imports type-only",
);
