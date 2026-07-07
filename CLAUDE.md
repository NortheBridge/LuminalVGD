# CLAUDE.md — LuminalVGD implementation guide

You are working in the LuminalVGD repository: a Rust UMDF IddCx virtual
display driver for LuminalShine. Read `docs/DESIGN.md`,
`docs/WGC-RELIABILITY.md`, and `docs/FEATURE-MATRIX.md` before writing code.
They are the specification; do not contradict them without flagging it.

## Ground rules
- Language: Rust (windows-drivers-rs / wdk-sys). Do NOT vendor or translate
  SudoVDA C++ source — implement the behaviors specified in
  FEATURE-MATRIX.md. pf-vdisplay Rust code MAY be inherited with notices
  (THIRD-PARTY-NOTICES.md).
- The ABI between driver and host lives ONLY in `crates/luminal-driver-proto`.
  Any layout change bumps `PROTO_VERSION_MAJOR` (breaking) or `_MINOR`
  (additive). Both sides must import this crate; never redefine structs.
- Every wait in driver code has a timeout. No IddCx callback does D3D work
  inline. See DESIGN.md §3.3 — these rules are the project's reason to exist.
- WGC fallback code follows the binding rules in WGC-RELIABILITY.md §Class 2
  verbatim (free-threaded pool, single teardown function, no frame refs
  escaping the handler).
- All commits: SPDX header `AGPL-3.0-only` on new files; the human reviews
  every diff before push.

## Phased plan
1. **Proto crate** — finish `luminal-driver-proto` (handshake, caps, ring
   header, IOCTL codes). Unit-test layout with `static_assertions` on
   size/alignment.
2. **Driver skeleton** — IddCx device that enumerates zero monitors, exposes
   the control device + `HANDSHAKE`/`GET_STATUS`. Installable via
   deploy-dev script on a test VM.
3. **Session model** — `CREATE_MONITOR`/`DESTROY_MONITOR`/`PING`, watchdog,
   max-monitors cap, exact single-mode lists, adapter selection.
4. **Transport** — swapchain acquisition → shared-texture ring with
   keyed-mutex protocol, generation counter, drop-oldest policy.
5. **Host integration** — `luminalvgd` backend in LuminalShine behind
   `virtual_display_backend`; probe → handshake → create → map; encoder
   consumes shared handles.
6. **WGC fallback hardening** — implement the recovery ladder (R1–R6),
   watchdog, reason-coded logging.
7. **Packaging** — INF, catalog, OV signing, FORCE_INTEGRITY clear,
   TrustedPublisher-only installer steps, uninstaller.
8. **Test matrices** — DESIGN.md and WGC-RELIABILITY.md tables become
   scripted/manual test checklists under `tests/`.

## Environment notes
- Driver builds need the WDK + eWDK toolchain on Windows; document exact
  versions in `docs/BUILDING.md` when established.
- Test hardware includes an RTX 5080 host on Insider builds — treat Insider
  regressions as first-class test input, not noise.
