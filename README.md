# blinc_lottie

Lottie animation player for [Blinc](https://github.com/project-blinc/Blinc) sketches.

Implements `blinc_canvas_kit::Player` so a parsed Lottie scene can be dropped
directly into any `Sketch`:

```rust
use blinc_canvas_kit::prelude::*;
use blinc_core::layer::Rect;
use blinc_lottie::LottiePlayer;

struct Hero {
    logo: LottiePlayer,
}

impl Sketch for Hero {
    fn draw(&mut self, ctx: &mut SketchContext, t: f32, _dt: f32) {
        ctx.play(&mut self.logo, Rect::new(40.0, 40.0, 200.0, 200.0), t);
    }
}

fn build() -> Div {
    let logo = LottiePlayer::from_json(include_str!("logo.json")).unwrap();
    sketch("hero", Hero { logo })
}
```

## Status

Early skeleton — loads JSON, exposes metadata. Rendering is stubbed: `draw_at`
paints a placeholder rect with a playback-progress tick.

## Roadmap

Tracked here so the `draw_at` stub doesn't stay a stub by accident. Rough
ordering — each phase depends on the primitives from the one before.

### Phase 0 — Correctness fixes on the existing surface

- [ ] **Pause-pose:** `set_playing(false)` currently freezes at `0.0`. Store
      a `last_t: f32` on `LottiePlayer` updated inside `draw_at` so
      `paused_at` can freeze at the actual last-rendered scene time.
- [ ] **Marker/event emission:** Lottie supports named markers on the
      timeline. Expose a callback / event queue so host sketches can react
      to them (plays well with Blinc's reactive model).

### Phase 1 — Minimum viable rendering

- [ ] **Solid layer** (`ty: 1`) — single color fill. Easiest layer type;
      unlocks basic backgrounds.
- [ ] **Transform keyframes** (`ks.p`, `ks.s`, `ks.r`, `ks.o`, `ks.a`) —
      position, scale, rotation, opacity, anchor-point. Linear interpolation
      first; bezier easing (`i.x`/`o.x`) after.
- [ ] **Shape layer** (`ty: 4`) — rectangles (`rc`), ellipses (`el`),
      polystars (`sr`), polygons (`sr` with `sy: 2`).
- [ ] **Fill** (`fl`) and **Stroke** (`st`) shape items — map to
      `DrawContext::fill_path` / `stroke_path`.

### Phase 2 — Path geometry

- [ ] **Bezier path shape** (`sh`) with vertex/in-tangent/out-tangent arrays
      → Blinc `Path` with `cubic_to` segments.
- [ ] **Animated path morphing** — keyframed `sh` where vertex count is
      stable. Interpolate per-vertex positions.
- [ ] **Trim paths** (`tm`) — start/end/offset animation for "drawn-on"
      effects. Requires per-segment length parameterization.
- [ ] **Group** (`gr`) — nested transform + children.

### Phase 3 — Visual effects

- [ ] **Gradients** (`gf`, `gs`) — linear + radial. Map to Blinc's
      `Gradient` brush.
- [ ] **Dash patterns** on strokes.
- [ ] **Drop shadow / blur / glow** — map to `LayerEffect::DropShadow` /
      `Blur` / `Glow` where possible.
- [ ] **Masks** (`masksProperties`) — single-mask + additive/subtractive
      modes. Maps to Blinc's clip-path system.

### Phase 4 — Advanced layer types

- [ ] **Text layers** (`ty: 5`) — requires `blinc_text` bridge for font
      resolution and layout. Skip animated per-character text for MVP.
- [ ] **Image layers** (`ty: 2`) — requires `blinc_image` bridge. Support
      base64-inline assets first, external `u`/`p` references after.
- [ ] **Null layers** (`ty: 3`) — transform-only parents.
- [ ] **Precomp layers** (`ty: 0`) — nested compositions. Recursive render
      with its own timeline.
- [ ] **Track mattes** (`tt`) — alpha/luma mattes between adjacent layers.

### Phase 5 — Format + performance

- [ ] **dotLottie** (`.lottie`) — zip bundle containing `animation.json` +
      image assets. Needs a `zip` dep.
- [ ] **Keyframe lookup acceleration** — binary search + per-property
      cursor so 60fps playback of long timelines doesn't re-scan.
- [ ] **Off-screen layer culling** — skip `draw_at` work for layers whose
      transformed bounds fall outside `rect`.
- [ ] **GPU path caching** — tessellate static shape geometry once and
      reuse across frames.

### Explicit non-goals

- **Expression layers** (`x: "..."` JS expressions). Porting the AE
  expression runtime roughly doubles the library footprint for an
  edge-case feature most exports don't use. Revisit only if a real
  consumer demands it.
- **After Effects-only effects** (Glow, 3D light, particle effects beyond
  basic emitters). Out of scope — users can composite these themselves
  using Blinc's own filter/effect stack on top of the Lottie player.

## License

Apache-2.0.
