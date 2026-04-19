# blinc_lottie — Backlog

Outstanding work, grouped by phase. The README tracks completion status
at a glance; this file has the implementation notes needed to pick any
item up cold.

Items are ordered roughly by impact on visual fidelity, not strict
dependency. Each entry notes **why** it matters and **how** to approach
it.

---

## Phase 1 follow-up — Bezier easing

- [x] **Bezier easing (`i` / `o` tangents on keyframes)** — shipped.
  Each `ScalarKey` / `Vec2Key` / `Vec4Key` carries optional in/out
  `BezierTangent` control points parsed from `i` / `o` on the
  keyframe. `solve_bezier_ease` runs a bounded Newton's method
  (8 iterations, early-exits on degenerate tangents) to resolve
  `bezier_x(t) = u` before each `sample_*` mix. Per-component
  tangents (`x: [..]`, `y: [..]`) collapse to a shared curve via
  `scalar_or_first` — covers After Effects' "Easy Ease" preset
  plus every authored curve we've seen in the wild. Different
  eases per axis remains a follow-up (would require per-component
  `sample_*` rather than a shared `u`).

- [x] **Per-axis bezier easing** — shipped. `Vec2Key` / `Vec4Key`
  now carry `[Option<BezierTangent>; N]` per-component tangents.
  `sample_vec2` / `sample_vec4` evaluate `eased_u` once per axis
  from the per-component `(out[i], in[i])` pair. Shared-curve
  inputs (the common case, `{ x: 0.5, y: 0.0 }` scalar form) fold
  into identical slots at parse time via `tangents_from_key_per_axis`,
  so the per-axis path runs as fast as the old shared path on
  that input. Authored per-axis curves (`{ x: [0.833, 0.0], y: [0.0, 1.0] }`)
  now resolve distinctly per component.

---

## Phase 2 — Path geometry

- [x] **Path shape (`sh`)** — shipped. `Geometry::Path(AnimatedPath)`
  parses `sh.ks.k` as `{ v, i, o, c }` vertices + per-vertex tangent
  offsets, then emits a Blinc `Path` walking vertex N→N+1 via
  `cubic_to(v[n]+o[n], v[n+1]+i[n+1], v[n+1])`. Close flag `c`
  stitches the last segment back to the first.

- [x] **Animated path morphing** — shipped. `AnimatedPath::Keyframed`
  interpolates between keyframes with matching vertex counts via
  per-vertex `PathShape::lerp`. Keyframes with different vertex
  counts fall back to the earlier keyframe (documented as
  unsupported — authoring tools rarely emit vertex-count mismatches).

- [x] **Trim paths (`tm`)** — shipped. `ShapeGroup.trim` carries
  parsed start / end / offset animatable scalars. At render the
  group flattens each geometry's cubic-bezier path into a polyline
  (24 samples per curve), computes cumulative arc length, and
  emits only the slice `[start + offset, end + offset]` back as a
  polyline Path. Offset wrap (start > end after folding) produces
  two separate slice emissions. Full-range and zero-range windows
  fast-path to identity / skip to avoid the flatten cost. Mode
  `m: 1` (Simultaneously) currently trims each geometry
  individually rather than on the concatenation of all the group's
  paths — equivalent for single-path groups (the common case),
  differs on multi-path groups. Output is polyline — loses the
  cubic smoothness of the original but is imperceptible at the
  default 24-sample density.

- [ ] **Trim paths — true Simultaneously (`m: 1`) + cubic output**
  - **Why:** Multi-path groups with `m: 1` currently render with
    per-geometry trim windows instead of the concatenation author
    expected. Visual difference is clear only when the paths have
    different arc lengths.
  - **How:** Concatenate all group polylines into a single buffer,
    compute one cumulative arc length, then emit slices. For cubic
    output rather than polyline: track which original segments a
    flattened sample came from and emit partial cubic beziers via
    de Casteljau subdivision.

---

## Phase 3 — Visual effects

- [x] **Gradient fill (`gf`) and gradient stroke (`gs`)** — shipped.
  `parse_gradient_fill` / `parse_gradient_stroke` produce
  `Paint::LinearGradient` / `Paint::RadialGradient` from the
  flattened `[t0, r0, g0, b0, …]` stop array. Alpha stops fold into
  the per-stop alpha at parse time. Animatable `s` / `e` endpoint
  points sample per-frame through the same
  `sample_paint_brush` path that solid fills use.

- [x] **Stroke dash patterns, line caps, line joins** — shipped.
  `StrokeSpec` carries `dash_pattern` (parsed from `st.d` alternating
  dash/gap entries), animatable `dash_offset` (the `"o"` entry in
  `st.d`), `cap` (`lc` enum), `join` (`lj` enum), and `miter_limit`
  (`ml`). All feed Blinc's `Stroke` builder verbatim.

- [x] **Drop shadow / blur effect layers** — shipped. Each
  `Layer` parses `ef` into `Vec<EffectSpec>`; rendering wraps the
  transform + content pass in `push_layer` with the sampled
  [`LayerEffect`]s (shadow offset/blur/color, blur radius). At
  sample time Drop Shadow direction / distance fold into
  `offset_x / offset_y` using AE's "north = 0°" convention; opacity
  normalises 0–255 → 0–1; shadow color alpha multiplies through.
  Gaussian Blur maps directly to `LayerEffect::blur`. Unsupported
  effect types (Tritone, Fill, Slider Control, Glow) drop
  silently with the rest of the permissive-parse posture. Spread
  / Blur Dimensions / quality are parsed-and-ignored — every blur
  is both-axis with the default quality.

- [ ] **Outer Glow effect**
  - **Why:** Lottie's native "Glow" is an AE compound effect that
    dotLottie doesn't standardise into a stable type number; some
    authoring tools emit it as a Tritone + Gaussian Blur chain.
    When the format settles, map to `LayerEffect::Glow`.

- [x] **Masks (`masksProperties`)** — shipped. `MaskSpec` carries
  mode (`add` / `subtract` / `intersect` / `lighten` / `darken` /
  `difference`), an `AnimatedPath`, animatable opacity, and an
  invert flag. `Layer::render` pushes each mask's sampled path as
  a `ClipShape::Path` onto the DrawContext's clip stack; sequential
  pushes already intersect at the renderer level so the default
  Add-mode "all masks combine with AND" semantics hold. Non-Add
  modes collapse to Add as the forgiving fallback — proper
  subtract / intersect / luma modes need an offscreen composite
  pass (tracked alongside track mattes).

---

## Phase 4 — Advanced layer types

- [x] **Text layers (`ty: 5`)** — shipped, per-layer text.
  `LayerKind::Text(TextSpec)` parses the first text-document
  keyframe: content, font family, fill color, font size,
  justification (`j` → `TextAlign::{Left,Right,Center}`),
  tracking (letter-spacing), and line height (`lh`, defaults to
  `1.2 × size`). Rendering delegates to
  [`blinc_core::DrawContext::draw_text`] so the same glyph atlas
  / SDF pipeline handles both layout-text and lottie-text —
  `blinc_text` as a direct dep isn't needed because the canvas
  text path (fixed separately) already shapes + rasterises. Each
  line in a multi-line document renders on its own baseline at
  `line_index × line_height`. Follow-ups:
  - Animated-text keyframing (different strings at different
    scene times) — parse only uses the first keyframe's `s`.
  - Text-range animators (`t.a`) — the per-character bounce /
    wave effects; needs per-character transform composition.
  - Right-to-left justification modes (`j: 3` / `4`) collapse to
    Left.

- [ ] **Image layers (`ty: 2`)**
  - **Why:** Hybrid vector/raster compositions.
  - **How:** Requires `blinc_image`. Asset references live in
    `assets` array at root; layer's `refId` points to one. Support
    base64-inline assets first (`u: ""`, `p: "data:..."`), external
    file references (`u: "images/", p: "img_0.png"`) after (needs a
    file-loader trait the caller can implement).

- [x] **Null layer (`ty: 3`)** — shipped. `LayerKind::Null`
  renders nothing and keeps `ind` / `transform` so children can
  reference it in their `parent_chain`. Named separately from
  `LayerKind::Unknown` so intent reads clearly in debug output.

- [x] **Parenting (`parent`)** — shipped. Each `Layer` carries
  `ind` + `parent_ind` parsed from JSON plus a resolved
  `parent_chain: Vec<usize>` (outermost ancestor first). The player
  walks the chain per-frame, pushing each ancestor's
  position/rotation/scale/anchor via `push_parent_transform` before
  the child's own `push_layer_transform`. Ancestor opacity does not
  propagate (matches After Effects convention). Forward refs
  resolve correctly; cycles and dangling `ind` values silently
  drop the chain so malformed exports still render.

- [x] **Precomp layers (`ty: 0`)** — shipped.
  `LottieRoot.assets` now parses; entries with a `layers` array
  become precomposition sources keyed by `id`. `Layer::from_value_with_assets`
  threads an `AssetContext` through parsing so `ty: 0` layers
  resolve their `refId` into a parsed child layer vec. Each child
  vec runs its own `resolve_parent_chains` pass (precomps have
  their own `ind` namespace). At render, the precomp pushes a
  rect clip at `[0, 0, w, h]`, remaps scene time via
  `scene_t - start_seconds` (or the animatable `tm` track when
  present), and walks the child layers back-to-front with their
  own parent-chain transforms. `MAX_PRECOMP_DEPTH` (8) bounds
  authored cycles without paying a HashSet walk per layer.

- [x] **Track mattes (`tt`)** — shipped, shape-clip
  approximation. `Layer.track_matte` parses the four `tt` modes
  (Alpha / AlphaInverted / Luma / LumaInverted) and `is_matte_source`
  reads from either the JSON `td` flag or the implicit "layer
  after a `tt`-bearing one" convention via `resolve_matte_pairs`.
  At render the matted layer pushes the matte source's union
  shape (composed from every geometry path in every group,
  recursed into nested children) as a `ClipShape::Path`, with
  the matte's parent-chain + own transform baked into the
  command coordinates. Matte source layers skip their own
  render — their alpha is consumed by the pair.

- [ ] **Track mattes — true offscreen composite**
  - **Why:** The current shape-clip shortcut only honours
    Alpha mode correctly. `AlphaInverted` / `Luma` / `LumaInverted`
    collapse to Alpha because a `ClipShape::Path` can't represent
    the matte's actual luminance gradient or inversion.
  - **How:** Push a layer group with the matte source rendered into
    an offscreen texture, use it as a `LayerEffect::MaskImage`
    against the matted layer's offscreen texture, composite both.
    Non-trivial — deferred until a real asset exercises non-Alpha
    mattes.

---

## Phase 5 — Format + performance

- [x] **dotLottie (`.lottie`)** — shipped, spec-2.0 layout.
  `DotLottieArchive` extracts `manifest.json` + `a/<id>.json` +
  `s/<id>.json`, honouring `manifest.initial.{animation,stateMachine}`.
  `LottiePlayer::from_dotlottie_bytes` resolves the initial animation
  through that path. Image / font / theme directories (`i/`, `f/`, `t/`)
  are parsed into the archive but not yet surfaced — raster layers that
  reference `i/` render vector content and skip the raster layer until
  Phase 4's image-layer work lands. Reference:
  <https://dotlottie.io/spec/2.0/>.

- [x] **dotLottie state machines** — shipped, spec-2.0 subset.
  `LottieStateMachine::from_dotlottie_bytes` decodes `s/<id>.json`
  into a [`blinc_core::fsm::StateMachine`] so transitions reuse
  the framework FSM primitive. Frame-based
  `segment: [start, end]` values are converted to seconds at load
  using `LottiePlayer::frame_rate`. Scoped subset (see
  `state_machine.rs` module docs for the full matrix):
  - **Applied**: PlaybackState + GlobalState, `segment`, immediate
    `Transition`, `Tweened` transitions (visual crossfade over
    `duration` with cubic-bezier `easing` — source pose freezes,
    destination plays forward, opacity ramp eased by authored
    curve), `Event` / `Numeric` / `String` / `Boolean` guards
    evaluated conjunctively against a shared input store,
    `SetNumeric` / `SetString` / `SetBoolean` / `Toggle` /
    `Increment` / `Decrement` actions mutate the store on
    successful transitions, `GlobalState` transitions expanded
    over every other state so they fire from any source.
  - **Parsed, no-op**: `Fire` / `Reset` actions; `interactions`;
    top-level `inputs` seeding; per-state `loop`, `loopCount`,
    `speed`, `autoplay`; `mode` other than Forward.

  Follow-ups still on the table:
  - Animated-segment markers (`"marker": "<name>"` pointing at a
    Lottie marker instead of explicit `[start, end]` frames).
  - Image-asset extraction from the archive for raster layers.

- [x] **Per-state playback modifiers** — shipped.
  Each `PlaybackState` with a segment compiles into a `StatePlayback`
  (segment + mode + loop + loopCount + speed + autoplay).
  `LottieStateMachine` now implements its own scene-time clock
  (`state_scene_t`) and calls `LottiePlayer::draw_frame` directly
  so the per-state config drives the pose on every frame. Modes
  Forward / Reverse / Bounce / ReverseBounce all applied; non-
  looping and `loopCount: N` freeze at the mode-appropriate
  terminal pose; `autoplay: false` pins at the starting pose
  until `Player::set_playing(true)`. `Player::seek` / `set_playing`
  on the state machine rebase the entered timestamp so pause /
  resume / seek stay scene-time-exact.

- [x] **State machine `Fire` / `Reset` + top-level `inputs[]` seeding** — shipped.
  Top-level `inputs` array is parsed into a typed
  `InputSpec::{Numeric|String|Boolean}` enum and seeds the shared
  `InputStore` at load so guards evaluate against author defaults
  without host setup. A snapshot of those same values (`InputDefaults`)
  backs the `Reset` action — resetting an input returns it to the
  declared default, or clears the key entirely when none was
  declared. Also exposed `reset_input` on `LottieStateMachine`
  for host-side lifecycle resets.
  `Fire` actions enqueue into a deferred `Vec<EventId>` shared
  with the FSM's action closures; `send()` drains the queue
  after the originating edge completes so cascades dispatch
  serially without re-entering the FSM. Cap at `MAX_CASCADE_DEPTH`
  (32 hops) bails out of authored cycles (A → fire(A)).

- [x] **Keyframe lookup acceleration** — shipped. `sample_scalar`
  / `sample_vec2` / `sample_vec4` walk keyframes via
  `partition_point` (binary search, O(log n)) instead of the old
  `.windows(2)` linear scan. Dense hand-authored timelines
  (dozens of keyframes per property) now sample in constant time
  per property regardless of position in the timeline. No cost
  on short keyframe arrays — the log₂ is already trivial.

- [x] **Opacity-zero early-out** — shipped. `Layer::render`
  samples the transform before allocating effect vecs or pushing
  mask clips, and returns early when the composed opacity is
  zero. Skips every downstream step for the common "fade-out
  tail" idiom where a layer's opacity keyframes to zero before
  its out-point.

- [x] **Off-screen layer culling** — shipped.
  `Layer::source_bounds(scene_t)` returns a local-space AABB per
  `LayerKind` (Solid → exact; Shape → union of geometry bounds
  walked through each group's `tr` transform; Null / Unknown →
  `None` so the layer always renders). At draw time, the player
  composes `DrawContext::current_transform()` (root + parent chain)
  with the layer's own `push_layer_transform` affine via
  `layer_local_affine` + `multiply_affines`, transforms the 4
  corners through it, and intersects the resulting AABB with the
  destination `Rect`. Culled layers skip `layer.render` entirely,
  avoiding the `push_layer` offscreen setup for effect wraps.
  Path bounds include in/out tangent handles so strongly-curved
  `sh` shapes can't false-cull. 3D parent transforms fall through
  to always-render — projecting an `Affine2D·Mat4` composition
  onto the 2D screen rect isn't worth the complexity.

- [ ] **GPU path caching**
  - Static shape geometry (non-animated `rc`/`el`/`sh`) tessellates to
    the same triangle mesh every frame today. Cache the tessellation
    once; reuse until inputs change.

---

## Non-goals (kept as is)

- **AE expression layers** (`x: "..."` JS).
- **AE-only effects** beyond the shadow/blur/glow trio above.
- **Scripting / runtime modification** past the `Player` trait's
  `seek` / `set_playing` controls.
