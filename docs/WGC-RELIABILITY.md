# WGC Reliability — Why Capture Gets "Stuck" and What LuminalVGD Does About It

WGC "getting stuck" on Windows 11 24H2 and Insider Preview builds is not one
bug. It is three distinct failure classes that present identically to the
user (stream freezes; sometimes black after reconnect). Each needs a
different countermeasure. LuminalVGD's job is to make class 1 structurally
impossible and to give the host a deterministic recovery ladder for 2 and 3.

## Class 1 — Display-topology failures (24H2+ API break)

**Symptom:** capture/DXGI initialization fails outright, or a running
capture dies, when the targeted display powers off, deep-sleeps (LG OLED TVs
are the classic trigger), or disconnects. Community workarounds literally
watch host logs and force-activate a dummy display when this happens.

**LuminalVGD countermeasure (structural):**
- Primary mode doesn't use WGC at all — direct-to-encoder reads the
  driver's own swapchain, so display power state of physical outputs is
  irrelevant.
- When WGC *is* used with the driver present (e.g., capture-compat mode for
  debugging), the host attaches the virtual monitor **first**, confirms
  arrival via display-change notification, and only then creates the
  `GraphicsCaptureItem` **for the virtual display**. Our virtual output is
  never "powered off."
- Pure fallback (driver absent, physical display target): before creating
  the session, verify the target `HMON` is active in the current display
  config; if the primary is offline, pick another active output or fail
  fast with a user-visible reason instead of a wedged session.

## Class 2 — Frame-pool lifecycle deadlocks (host-side bugs)

These are the documented ways applications freeze WGC, and they are all
ours to avoid, not Windows':

- Calling `Stop`/`Close` from inside the `FrameArrived` handler.
- Holding `Direct3D11CaptureFrame` (or its underlying texture) references
  past handler scope — frames are checked out on `TryGetNextFrame` and only
  return to the pool when released.
- Closing the frame pool before unsubscribing `FrameArrived`/`Closed`.
- Calling from threads without a proper COM apartment.
- Skipping the `ID3D11Multithread` lock on the device shared with the pool.

**Binding rules for the WGC backend implementation:**
1. `Direct3D11CaptureFramePool.CreateFreeThreaded` only — never the
   dispatcher-bound variant (no UI-thread coupling in a streaming host).
2. `FrameArrived` does exactly: TryGetNextFrame → copy/share texture to the
   encoder queue → release frame → return. No teardown, no blocking calls.
3. Teardown order is a single function, the only place allowed to destroy
   anything: unsubscribe events → drain/release all outstanding frames →
   `session.Close()` → `framePool.Close()` → release item → release device.
4. Encoder queue holds *copies or shared handles*, never pool-owned frames.
5. Backend thread is MTA-initialized; `ID3D11Multithread::SetMultithreadProtected(true)`
   on the capture device.

## Class 3 — Silent frame starvation (mode changes, TDRs, Insider regressions)

**Symptom:** session and pool remain "valid" but `FrameArrived` simply stops
firing — after a resolution/HDR toggle, a GPU TDR, or for no visible reason
on some Insider builds.

**Countermeasure: watchdog + escalation.** Detection is a frame-sequence
watchdog: no new frame for `3 × expected_frame_interval` (floor 250 ms)
while the desktop is known-active trips the ladder.

## 4. The recovery ladder

Each rung is attempted once, with a keyframe forced after any success.
Every trip logs rung + reason code (these logs are the Insider-build
regression early-warning system).

```
R1  Recreate frame pool        Direct3D11CaptureFramePool.Recreate(same
                               device, current content size/format).
                               Drain pending frames first — Recreate
                               discards all outstanding frames by design.
                               Fixes: stale size after mode change.

R2  Rebuild capture session    New GraphicsCaptureItem for the same target,
                               new session + pool. Fixes: item invalidated
                               by topology change.

R3  Rebuild D3D device         Check GetDeviceRemovedReason; new device,
                               then R2. Fixes: TDR / driver update / reset.

R4  Cycle the virtual display  Driver present only: DESTROY_MONITOR +
                               CREATE_MONITOR (fresh connector), then R2.
                               Fixes: compositor-side wedge against the
                               previous connector instance.

R5  Drop to DDA (if enabled)   Desktop Duplication on the same output.
                               Fixes: WGC broken wholesale on this build.

R6  Fail the session loudly    Structured error to the client + log bundle.
                               Never leave a silently frozen stream.
```

When the driver is present, R1–R3 failures also flip the session's primary
path back to direct-to-encoder if it was only in WGC for compatibility
reasons — the driver ring does not depend on any of the WGC machinery.

## 5. HDR notes for the fallback

- Pool format `R16G16B16A16_FLOAT` end-to-end when the stream is HDR
  (prevents the washed-out overclipping documented for HDR capture);
  `B8G8R8A8_UNORM` for SDR.
- After an HDR on/off toggle, treat it as a mode change: R1 immediately,
  don't wait for the watchdog.

## 6. Test matrix (fallback-specific)

| Scenario | Expected |
|---|---|
| Power off target TV mid-WGC-stream (24H2) | Ladder R2/R4 recovers or fails loudly ≤ 3 s; no frozen stream |
| Resolution change mid-stream | R1 recovers; one keyframe hitch |
| HDR toggle mid-stream | Immediate R1; colors correct |
| Force TDR (dxcap -forcetdr) | R3 recovers |
| Kill/restart LuminalVGD device mid-stream | Host swaps to WGC ≤ 1 s; R4 available next session |
| Insider build with known WGC starvation | R5 (DDA) keeps session alive; telemetry flags the build |
