# wall. engineering notes

- Windows-native Rust utility. Do not add a browser runtime, server, accounts or telemetry.
- Preserve the centered, animated image-first carousel, genuine dark glass and explicit opaque fallback. No conventional header/footer, Current badge, numbering or large Apply button.
- Keep target selection separate from analytic spring position/velocity. Advance elapsed time before new input; reduced motion must preserve precision-scroll accumulation.
- Generic focus loss must not dismiss the overlay. Outside clicks dismiss with Explorer/taskbar exceptions; native pickers hide and restore it explicitly.
- No continuous rendering or polling while hidden. Wake the UI through events.
- Keep filesystem work, image decoding and wallpaper calls off the rendering thread.
- Bound decoder allocations, queued requests, resident textures and thumbnail cache growth.
- Persist references; never move or delete original wallpaper images.
- Preserve Windows fit settings. Do not enable startup automatically.
- Test wallpaper mutation only with a deliberate snapshot/apply/restore strategy.
- Run cargo fmt, relevant tests, clippy and a Windows release build. Inspect real UI when available.
