# blinc_lottie — Backlog

Outstanding work, grouped by phase. The README tracks completion status
at a glance; this file has the implementation notes needed to pick any
item up cold.

Items are ordered roughly by impact on visual fidelity, not strict
dependency. Each entry notes **why** it matters and **how** to approach
it.

---

## Phase 1 follow-up — Bezier easing

- [ ] **Bezier easing (`i` / `o` tangents on keyframes)**
  - **Why:** Linear keyframe interp is off-shape from the AE-authored
    curve. Hand-animated Lotties look noticeably wrong without eases.
  - **How:** Each keyframe carries `i: { x: [..], y: [..] }` and
    `o: { x, y }` arrays describing the cubic bezier that maps
    `u ∈ [0, 1]` (linear progress between keyframes) to eased progress.
    Solve `bezier_x(t_param) = u` for `t_param` via Newton's method,
    then evaluate `bezier_y(t_param)`. Per-component tangents
    (`x[i]` / `y[i]` arrays) allow different eases per axis — start with
    a shared easing (take `x[0]` / `y[0]`) and expand later.
  - **Touches:** `src/layer.rs` — `ScalarKey` / `Vec2Key` / `Vec4Key`
    gain optional `(in, out)` bezier control points; `sample_*` replaces
    the linear `u` with the eased progress before the mix.

---

## Phase 2 — Path geometry

- [ ] **Path shape (`sh`)**
  - **Why:** Hand-drawn shapes (logos, icons, organic forms) are all
    path-based. Rect/ellipse covers only boxy iconography.
  - **How:** `sh.ks.k` is a `{ v, i, o, c }` struct where `v` is an
    array of vertex positions, `i`/`o` are per-vertex in/out tangent
    offsets (relative to the vertex), and `c` is a close flag. Build a
    Blinc `Path` walking vertex N to N+1 as
    `cubic_to(v[n]+o[n], v[n+1]+i[n+1], v[n+1])`.
  - **Animatable:** when `a: 1`, `k` is keyframed — interpolate per
    vertex (same vertex count across keys is common; add assertion +
    warn otherwise, and pin to the last keyframe that matches).
  - **Touches:** new `Geometry::Path { ... }` variant in `src/shape.rs`;
    new `AnimatedPath` type (needs its own keyframe struct since
    linear interpolation of `Vec<VertexCubicSegment>` is not a flat
    `[f32; N]`).

- [ ] **Animated path morphing**
  - Subset of the path shape once `sh` lands: keyframes with matching
    vertex counts lerp per-vertex. Different vertex counts between
    keyframes are out of scope (rare, documented as unsupported).

- [ ] **Trim paths (`tm`)**
  - **Why:** "Drawn-on" and "progress arc" animations rely on this.
  - **How:** `tm` carries animatable `s` (start %), `e` (end %),
    `o` (offset %). At render, walk the group's path, parameterize by
    arc length, emit only the `[s, e]` slice (mod 1.0 with offset).
    Needs a path-length sampler — Blinc's Path doesn't expose one yet;
    either compute locally (flatten to segments + cumulative length) or
    contribute the helper upstream.

---

## Phase 3 — Visual effects

- [ ] **Gradient fill (`gf`) and gradient stroke (`gs`)**
  - **Why:** Replaces flat colors in most stylized designs.
  - **How:** Lottie encodes gradient stops as a flattened
    `[t0, r0, g0, b0, t1, r1, g1, b1, ...]` array inside `g.k.k`. Type
    `t` is 1 (linear) or 2 (radial); endpoints `s`/`e` are animatable
    points. Map to `Gradient::Linear` / `Gradient::Radial` brushes on
    the Blinc side. Stop alphas live after the color stops in the same
    array — parse both and attach.

- [ ] **Stroke dash patterns, line caps, line joins**
  - **Why:** Matters for dashed outlines, rounded pen strokes.
  - **How:** `st.d` is an array of `{ n: "d"/"g", v: { k: <num> } }`
    entries (dash, gap, offset). `lc` / `lj` fields carry enum indices
    for caps (`butt`/`round`/`square`) and joins (`miter`/`round`/
    `bevel`). Blinc's `Stroke` builder already supports all three —
    just plumb the values through.

- [ ] **Drop shadow / blur / glow**
  - **Why:** Common in "UI motion" templates.
  - **How:** Lottie exposes these as *effect layers* (`ef` array on a
    layer), not shape items. Blinc has `LayerEffect::DropShadow` /
    `Blur` / `Glow` — map via `push_effect` / `pop_effect` before /
    after the affected layer's draw call.

- [ ] **Masks (`masksProperties`)**
  - **Why:** Required for any scene with clipped content.
  - **How:** Each mask is a path + mode (`add` / `subtract` /
    `intersect`). For the single-mask case, push it through Blinc's
    clip-path system (`ClipShape::Polygon`). Multi-mask + track-matte
    (adjacent-layer alpha/luma) is Phase 4.

---

## Phase 4 — Advanced layer types

- [ ] **Text layers (`ty: 5`)**
  - **Why:** Text callouts, lower thirds, kinetic typography.
  - **How:** Font resolution and shaping go through `blinc_text`; add
    it as a dependency. Animated per-character text (`t.p.a`) is a
    big feature — stub with per-layer text first, add character
    animation later.

- [ ] **Image layers (`ty: 2`)**
  - **Why:** Hybrid vector/raster compositions.
  - **How:** Requires `blinc_image`. Asset references live in
    `assets` array at root; layer's `refId` points to one. Support
    base64-inline assets first (`u: ""`, `p: "data:..."`), external
    file references (`u: "images/", p: "img_0.png"`) after (needs a
    file-loader trait the caller can implement).

- [ ] **Null layer (`ty: 3`)**
  - Transform-only parent. Zero-effort once parenting is in.

- [ ] **Parenting (`parent`)**
  - **Why:** Nearly every non-trivial Lottie uses it — transforms
    compose up the parent chain.
  - **How:** Resolve `parent` index at parse time into a `Option<usize>`
    on each `Layer`. At render, compose transforms up the chain
    before applying the layer's own. Cache composed transforms per
    frame if perf matters later.

- [ ] **Precomp layers (`ty: 0`)**
  - **Why:** Nested compositions are AE's main reuse mechanism.
  - **How:** `assets` array can contain precomp assets (same shape
    as root `layers`). A precomp layer renders its child composition
    with its own timeline (clipped by the layer's `ip`/`op`). Recursive
    render into a child `LottiePlayer`-like struct, or flatten at parse
    time.

- [ ] **Track mattes (`tt`)**
  - **Why:** Alpha / luma masking between adjacent layers.
  - **How:** `tt` on layer N means the next layer N+1 serves as N's
    matte. Needs an offscreen render pass — group affected layers into
    a layer group rendered to an intermediate texture, then composite
    with the matte's alpha. Non-trivial; park until real files demand
    it.

---

## Phase 5 — Format + performance

- [ ] **dotLottie (`.lottie`)**
  - **How:** Zip archive containing `animation.json` and assets. Add
    a `zip` dep; `from_dotlottie_bytes(&[u8])` unpacks, loads the
    JSON, and wires up any image assets.

- [ ] **dotLottie state machines** — *depends on dotLottie archive*
  - **Why:** dotLottie's spec extension beyond core Lottie JSON. Apps
    ship one archive that defines named states (e.g. `idle`, `hover`,
    `clicked`) where each state points at a segment of the master
    animation, plus event-triggered transitions between states. Common
    in Lottie-driven UI components (interactive icons, onboarding
    hero animations) — authors want to ship one `.lottie` with all
    the interaction states baked in rather than wire state handling
    in application code.
  - **How:** Archive member `state_machine.json` alongside
    `animation.json`. Decode into a struct paralleling Blinc's
    existing [`blinc_core::FsmRuntime`](../../crates/blinc_core/src/fsm.rs)
    so transition + guard logic reuses the framework FSM primitive
    instead of reinventing:
    - Named states → `StateId`s. Each carries a `segment: Range<f32>`
      on the master timeline + a loop flag + playback speed.
    - Events (`pointer.enter`, `pointer.click`, `custom("foo")`) →
      `TransitionTrigger`s. Numeric inputs (`progress >= 0.5`) go
      through guard closures.
    - `LottieStateMachine` wraps `LottiePlayer` with a current-state
      pointer + event queue. `.handle(event)` fires the FSM and
      updates the player's playback range.
  - **Scope notes:**
    - Lands AFTER `.lottie` archive parsing — there's no JSON-only
      state-machine format worth implementing in core Lottie.
    - Reference: <https://dotlottie.io/state-machines>.
    - Segment playback (in/out range on the player) is already
      parsed via layer `ip`/`op`; the missing piece is a "play a
      sub-range of the composition, not just per-layer" API on
      `LottiePlayer` that the state machine drives.

- [ ] **Keyframe lookup acceleration**
  - **Why:** `sample_*` does a linear scan per property per frame. Fine
    for short timelines, quadratic behavior on long ones with many
    keyframes.
  - **How:** Replace `windows(2)` scan with binary search + per-property
    `last_index` cursor that biases the search. Also: if `t` hasn't
    moved across a boundary, return the cached value from the previous
    frame.

- [ ] **Off-screen layer culling**
  - Skip `layer.render` work when the layer's transformed AABB doesn't
    intersect the destination `rect`. Needs an AABB-in-source-space
    estimator for each `LayerKind`.

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
