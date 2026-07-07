# LuminalVGD

**Luminal Video Graphics Display Driver** — a first-party virtual display driver for
[LuminalShine](https://github.com/NortheBridge/luminalshine), developed in-house by the
NortheBridge Foundation.

> **Status: pre-development.** This repository is the future home of LuminalVGD.
> Nothing here is buildable or shippable yet.

## Why

LuminalShine creates per-client virtual monitors so headless hosts and mismatched-display
setups can stream at the client's exact resolution, refresh rate, and HDR configuration.
Today that relies on the third-party SudoVDA driver, which has known reliability issues on
current Windows 11 release and Insider Preview builds (WUDFHost hangs). LuminalVGD replaces
it with a driver we design, sign, and service ourselves — built specifically for
LuminalShine's session lifecycle instead of adapted to it.

## Planned scope

- IddCx (Indirect Display Driver) UMDF driver targeting Windows 11.
- Per-client virtual monitors with exact mode lists (resolution / refresh / HDR10 metadata)
  driven by the streaming session, including frame-generation-aware refresh doubling.
- First-class recovery: survive GPU TDRs and driver restarts without wedging WUDFHost,
  with a control interface designed for LuminalShine's watchdog/recovery ladder.
- Render-adapter selection (hybrid-GPU laptops) at creation time.
- Signed and serviced through the LuminalShine installer as the default backend,
  superseding SudoVDA.

## Relationship to LuminalShine

LuminalShine's backend selector (`virtual_display_backend`) already anticipates this driver:
SudoVDA remains the shipped default until LuminalVGD lands, at which point it takes over the
default slot. Integration points live under `src/platform/windows/` in the LuminalShine
repository.

## License

[GPL-3.0](LICENSE) © NortheBridge Foundation
