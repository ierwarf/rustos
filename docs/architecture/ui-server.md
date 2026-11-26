# UI Server & Wayland

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

`uiserver` is RustOS's display server. It owns the framebuffer, draws the
desktop chrome, hosts a Wayland compositor, and renders console-hosted
sessions for Linux and Windows PE programs.

### Boot Path

```text
runtimed bootstrap_ui_server
  -> spawn uiserver with manifest args/env
uiserver run()
  -> AppState::initialize (display_get_info + display_create_surface)
  -> render_boot_frame (single solid fill — no chrome yet)
  -> first present (boot frame stays on screen while runtime/Wayland init runs)
  -> open RuntimeClient (notify_ui_ready socket + sync socket)
  -> WaylandCompositor::initialize (creates /run/user/1000/wayland-0)
  -> notify_ui_ready -> runtimed unblocks autostart launches
  -> main loop:
       drain input, runtime poll, console poll, cursor blink,
       coalesce partial dirty rects, render & present
```

The synchronous critical path is intentionally short:

1. The first present uses a solid colour, not the full chrome. Chrome
   composition is deferred to the first idle frame so the user always sees
   pixels within a few frames of `display_create_surface`.
2. Console window surfaces are no longer "primed" during runtime polling;
   `draw_console_window` rebuilds the cached surface on the first draw,
   matching the same total work to the render budget instead of the runtime
   refresh budget.
3. The terminal cell loop in `terminal.rs` skips redundant per-cell
   background fills when the bulk render has already painted
   `layout.bounds`. Per-cell paint stays for single-cell repaints
   (cursor blink) only.

### Translucent Aero Palette

The chrome palette is a translucent monochrome blue. Glass and inner
panels stack via `fill_rect_alpha` so the desktop band shows through the
rails. Highlights and shadows are tuned to suggest frosted glass:

- Rails draw an upper sheen half + a lower settled half + an inner
  elevated bloom, with a 1-pixel specular line at the top edge and a faint
  baseline shadow underneath.
- Window title bars use the same two-layer technique with an extra accent
  overlay when focused.
- Mouse cursor stays at the existing soft blue/white tone; do not change
  `cursor_sprites.rs` or the cursor colours in `canvas.rs` for theme
  recolours.

### Wayland Surfaces

The compositor exposes the standard Wayland sockets via
`/run/user/1000/wayland-0`. Surfaces, buffers, frame callbacks, and damage
all use the canonical Wayland protocol — the in-tree Wayland clients
(`apps/wayclick`, console hosts) and the planned external clients should
not need special wrappers.

WayClick uses the normal client event queue and blocking dispatch loop. A
`wl_surface.frame` callback is one-shot and becomes active with the matching
surface commit; a buffer is reused only after `wl_buffer.release`. Profiling
may request another representative redraw after each callback, but it does not
replace the wire protocol or add a GPU-specific application API. This follows
the upstream Wayland protocol and client event-loop contracts:
<https://wayland.freedesktop.org/docs/html/apa.html> and
<https://wayland.freedesktop.org/docs/html/apb.html>.

The bounded KVM proof keeps the contract honest without a client-specific
compositor path. Uiserver performs one coalesced present after input, Wayland,
and runtime damage collection; the retired cursor-only early-present lane must
not return. A successful real presentation grants one non-accumulating
callback permit that is consumed before the next present, giving a standard
client time to draw for the next refresh. Damage-free callback-only commits use
the same 15 ms monotonic cadence as DVM GPU presentation, and a pending
callback deadline participates in the main wait. This removes the 15/16 ms
beat and prevents the compositor from sleeping past client work while still
forbidding accumulated timer credit and unbounded callback loops.

The final bounded capture passed the exact sustained 55 FPS gate. Its first
accepted three contiguous WayClick windows delivered 57, 65, and 68 matched
commit/callback/release cycles over 3.060 seconds (62.092 FPS aggregate), with
per-window callback rates of 56.394--67.342 FPS and a 45 ms maximum callback
gap. The corresponding uiserver and authenticated DVM relay windows also
passed their FPS, input-loss, cursor, fence-count, and backlog gates. Initial
window-topology repacking remains separately bounded to 100 ms before the first
profile window; any later loop above 50 ms still fails acceptance.

Window movement is not a topology change. The retained scene binds each visible
window identity and atlas source rectangle to one exact GPU layer index, then a
drag changes only that command's destination rectangle. Position is excluded
from the structural signature; dimensions, focus, visibility, title, ordering,
or any binding mismatch still force a fail-closed rebuild. This prevents
pointer-rate dragging from allocating, rasterizing, comparing, and copying a
complete 2048x2048 atlas on every motion.

The OS userspace event/readiness boundary is now implemented as a general
cross-provider wait set rather than a WayClick-specific route. uiserver waits
the Wayland server's aggregate Ring0 `EpollHandle` fd (with Ring0 `UnixSocketHandle`
Wayland client sockets) together with its input notification;
compat honors finite/infinite application deadlines and performs atomic
check-register-recheck-arm-presence-check against local readiness tokens and
service-owned generations.
Open-description lifetime, provider epoch revoke, per-interest `ERR|HUP`, and
bounded close/dup/fork/exec cleanup are source/model covered. Remote state
mutations use exact operation IDs with replay/reconciliation, vfsd restores its
rootd-retained authenticated open-description checkpoint before endpoint
publication, and service objects use boot-entropy capabilities plus
sender/dependency checks.
This does not replace focused runtime crash-injection evidence. The
[batch validation](../validation/ring3-reliability-batch.md) records the
separate KVM boot and input evidence and its timing limits. Fuchsia's
peered IOBuffer remains a useful commercial-OS reference for the object shape,
not an ABI to copy verbatim:
<https://fuchsia.dev/fuchsia-src/reference/kernel_objects/io_buffer>.

### Wayland Hardening Contract

- **Surface identity.** Window snapshots, focus, drag, move, minimize and close
  are keyed by a compositor-unique `surface_id` allocated in
  `WaylandState::create_surface_data`. A `wl_surface` protocol object id is
  per-client, so two clients routinely own the same one; keying on it made the
  second client's window unfocusable and let a retire clear the wrong client's
  focus. Internal focus comparisons use `ObjectId` equality. `0` means "no
  surface".
- **Admission limits.** 64 surfaces and 64 clients compositor-wide, 16 surfaces
  and 8 tracked `wl_output`/`wl_pointer`/`wl_keyboard` objects per client, and
  4x`MAX_WAYLAND_BUFFER_PIXELS` retained surface pixels in total. Exceeding a
  limit is a protocol error on the offending client only; one client cannot
  starve the others of surfaces, seat devices or heap.
- **Shared-memory ownership.** The compositor does not map client SHM. It
  uses positioned reads into owned pixels and rejects incomplete copies; see
  [the SHM ownership contract](#mixed-window-and-hostile-shm-ownership-contract).
- **Toplevel recreation.** Destroying a toplevel clears focus and abandons
  outstanding commits. Recreating it on the same `wl_surface` preserves
  identity and placement, but resets title, decoration and maximize state.
- **GPU atlas capacity.** Occlusion fragment packing (`render/fragments.rs`)
  and dynamic atlas geometry fit maximized windows alongside the desktop and
  dock (verified up to 2560x1440 in a 2688x2560 atlas and 2880x1800 in a
  3008x2944 atlas). Windows are packed in reverse draw order (focused
  first, then topmost) and appended in draw order, so a full atlas costs the
  window furthest down the stack its texture. Dropped windows are logged
  (bounded, once per change of the dropped set) as `gpu atlas ... full`.
- **Stall budget & focus.** Newly mapped Wayland toplevel windows receive
  automatic keyboard/window focus (`app/mod.rs`). Wayland socket calls on the
  UI thread are nonblocking (`UnixSocketHandle`), and `WAYLAND_STEP != none`
  retains a 7 s watchdog budget while every other UI phase (with granular
  `PRESENT_STEP` diagnostics in `main.rs`/`gpu_runtime.rs`) keeps 3 s.
- **Module layout.** `wayland.rs` owns constants, shared types,
  `WaylandCompositor`, listener-socket binding, buffer-argument validation,
  row conversion and `pointer_button`: `formal/zero-trust-subsystems.tsv` and
  `formal/check-performance-contracts.sh` pin those tokens to this file.
  `wayland/` holds `state` (surface table, snapshots, configure), `input` (hit
  test, pointer focus, keyboard), `windows` (z-order, focus, retire,
  move/minimize/maximize/close), `buffer`, and one dispatch file per protocol
  group (`compositor`, `output`, `seat`, `shm`, `surface`, `xdg`).
  `sys/input.rs` holds input reads and event translation. Unit tests live in
  `*/tests.rs` beside their module. Further splits of `render.rs`,
  `gpu_runtime.rs`, `sys.rs` and `app/runtime.rs` are blocked on moving the
  formal anchors that name those files (`implementation-mutations.tsv`,
  `check-performance-contracts.sh`, `run-source-conformance.sh`,
  `trust-boundaries.tsv`).

### Console Hosting

Console-hosted programs (the shell, the PE demos) get a console session
from `runtimed` and a window from `uiserver`. The terminal state lives in
`uiserver`'s `TerminalState`, parsing input bytes from the console output
ring. Layout uses the terminal monospace atlas; chrome uses the UI atlas.

### Fonts and UI Config

`uiserver/build.rs` pre-rasterizes the licensed Inter and Maple Mono faces from
`assets/ui/bundle/ui.conf` into immutable glyph metrics and alpha masks in
`OUT_DIR`. The binary embeds these atlases for all four roles (`ui_large`,
`ui_medium`, `clock`, `terminal`) and that configuration's UTC offset (currently
+540 minutes). The first desktop frame therefore has usable title, clock, date
and terminal typography without filesystem access or target-side font parsing.
The same `glyph_atlas.rs` rasterizer, strict config parser and bounded reads are
used by the build and runtime loader; there is no separate fallback layout.
Build invalidation tracks the config, named font files and shared sources.
Font licenses remain with the packaged fonts (`fonts/Inter-OFL.txt` and
`fonts/MapleMono-OFL.txt`).

A background loader still reads `/assets/ui/ui.conf` from the `ui-assets` data
package (`RUSTOS_UI_CONFIG` overrides its path for development), parsing each
named face once with `fontdue`. Reads are bounded (64 KiB config, 4 MiB per
font). Invalid config reports the offending line when available; terminal
roles must declare `monospace = true` so no zero-width grid can be published.
Unavailable storage is retried on the loader thread. Missing, invalid or
unreadable overrides retain bundled text, never a blank desktop.

Loader completion transfers an owned asset set to the UI loop rather than
publishing mid-frame. Exactly one explicit promotion switches configured fonts
and clock offset together between frames. Missing roles in a partial override
use the bundled atlas. `font::asset_revision()` is nonzero from startup (1),
advances on successful promotion (2), and is a required identity in every
retained font-dependent cache, including console rasters and the GPU scene
signature (Wayland titles and dock text included). Promotion invalidates desktop
chrome and every console surface, marks terminal layout/output dirty, clears
the clock poll timestamp and requests a full scene update. Changed terminal
metrics replay the cached output at the new grid size; unchanged metrics keep
the incremental parser state.

The clock always reads real `SystemTime`, never elapsed boot time or a fabricated
time string. It displays UTC plus the active `clock.utc_offset_minutes`; RustOS
has no timezone database or automatic daylight-saving transitions. Before an
override is ready, the build-time packaged offset is used; a different override
offset takes effect atomically with its asset handoff. Tests cover initial
production drawing/measurement with no loader, real clock/packaged offset,
exact generated/runtime atlas pixel and geometry parity, one-shot promotion,
custom-font preservation and terminal output reflow. Live first-desktop DVM
readability and combined retained GPU invalidation remain integration gates.

### Retained rendering and copy budget

The 2026-11-26 optimization batch was reviewed statically only, at the user's
request. The historical FPS and KVM results elsewhere in this document do not
validate this batch, and no new speedup or frame-time measurement is claimed.

- **Submission storage:** `GpuCompositor` retains layer, source, command,
  encoded-byte, and submission-damage allocations. Expired damage-history
  entries return their allocation to the next frame. `compile_into` invalidates
  the reusable batch before any fallible work; `encode_into` clears previous
  bytes and still validates every new wire batch. Cursor-only turns reuse the
  scene/dock signatures. The mixed window-order visitor has the same
  stack/focus/popup/drag-icon ordering as the collected rendering plan.
- **Damage and recovery:** differential atlas damage keeps separate scanline
  bands within the wire descriptor limit. Excess fragmentation collapses to a
  conservative bounding rectangle. Both exact-predecessor and older-slot
  submissions normalize overlaps and obey the 64-rectangle limit. Every
  scratch reservation includes its transparent padding in `used_height`;
  rebuilds clear the prior scratch extent and compare the maximum of the old
  and new extents, including cleared storage. A failed build marks the scratch
  extent full before it can be reused.
- **Failed publication:** an incremental patch that falls back to rebuilding
  may already have changed the local atlas. A rejected GPU submit may likewise
  have changed pixels without creating an accepted epoch. Either condition
  forces a complete snapshot into both the next transfer and its accepted
  damage-history entry. Slot epochs, pending fences, and history advance only
  after successful submit; compiler admission rolls back on transport failure.
- **Atlas copies:** tightly packed complete rows form one contiguous transfer;
  partial rows retain bounded scanline copies. The x86-64 streaming copy has a
  64-byte loop and one publication `sfence` after all writes, including small
  scalar writes into the write-combining mapping. In-flight slots remain
  immutable until their exact completion is consumed.
- **Window rasterization:** undecorated Wayland fragments copy directly from
  the immutable owned client snapshot. Console fragments copy only visible
  source rectangles and apply window alpha using original window coordinates.
  Uncropped decorated windows rasterize into reserved atlas storage instead of
  an intermediate full-window image. Partial decorated updates recompose the
  affected chrome/client region and reapply rounded-corner alpha. A window
  used as an opaque occluder becoming translucent invalidates the fragment
  layout before it can hide newly visible content. Cropped decorated scenes
  still rasterize an independent full window before selecting fragments;
  translucent updates in that path retain that temporary raster while copying
  and alpha-converting only changed visible fragments.
- **Cursor and glass:** cursor raster storage is reused, and the strided API
  writes only the visible cursor texels in its reserved region. Glass blur's
  vertical passes use row-major sliding column sums, preserving the original
  integer rounding and replicated-edge behavior while avoiding column-stride
  image traversal.
- **Terminal:** VTE consumes complete output suffixes per call. Scrollback is
  a bounded FIFO with row-allocation reuse; multi-row scrolls move the live
  grid once and retain at most the final 256 outgoing rows. Terminal painting
  clips the cell traversal and pins one font atlas for the whole paint.
  Asset promotion and the font revision in retained caches remain unchanged.
- **Pixels:** alpha-mask and premultiplied source-over scanlines have scalar
  and runtime-gated AVX2 implementations. Transparent/opaque endpoints,
  `div255_round`, malformed-channel saturation, and the unused BGR high byte
  retain scalar semantics. The existing CPU/OS AVX2 admission and XSAVE
  context preservation are prerequisites; no stronger ISA is assumed.

Wayland snapshots continue to use the bounded worker pool and positioned reads
into compositor-owned storage; client SHM is never mapped. Full-buffer damage
publishes the fresh complete snapshot instead of cloning an old `Arc` frame and
then overwriting it. Contiguous damage reads directly into its final allocation;
padded rows initialize spare capacity with fused XRGB conversion. The vector
length is published only after every byte/element is initialized. Integer
viewport crops use row copies; transformed resampling precomputes column
offsets and reuses repeated sampled rows. Admission limits, release/callback
ordering, deadlines, and per-client worker fairness remain binding contracts.

Snapshot readiness caches the oldest noncancelled submission time. Submission,
retirement, and cancellation maintain it under the queue lock, including equal
timestamps. Completion/callback/release vectors retain capacity only when the
destination queue is still empty, so recycling cannot overwrite newly arrived
work. Completing one snapshot wakes one worker for the newly available client
slot. Worker staging storage clears its live length before enforcing its
existing retention cap.

References for these mechanisms and their contracts:

- [Wayland damage, commit, buffer release and frame callbacks](https://wayland.freedesktop.org/docs/html/apa.html)
- [Rust vector capacity and initialized/spare storage](https://doc.rust-lang.org/std/vec/struct.Vec.html)
- [Rust FIFO operations](https://doc.rust-lang.org/std/collections/vec_deque/struct.VecDeque.html)
- [VTE 0.15 batch parser API](https://docs.rs/vte/0.15.0/vte/struct.Parser.html)
- [Non-temporal store synchronization](https://doc.rust-lang.org/core/arch/x86_64/fn._mm_sfence.html)

### Profile Trace

Set `RUSTOS_UI_PROFILE=1` when profiling is needed to enable
`profile::record_*` paths. The summary lines tagged
`uiserver profile: ...` pass through a dedicated bounded asynchronous
observability channel to debugcon. A slow observability path can make the
KVM gate fail conservatively, but cannot block the render loop. Use these
samples before adding new diag prints.
Per-frame cursor/render pipeline samples are also profile-gated; normal KVM
runs keep only coarse `uiserver: update tick` liveness lines.

<a id="korean"></a>

## 한국어

`uiserver`는 RustOS의 display server입니다. framebuffer 소유, desktop chrome
draw, Wayland compositor 운영, 그리고 Linux/Windows PE program의
console-hosted session rendering을 담당합니다.

### Boot 경로

```text
runtimed bootstrap_ui_server
  -> manifest args/env로 uiserver spawn
uiserver run()
  -> AppState::initialize (display_get_info + display_create_surface)
  -> render_boot_frame (chrome 없이 단색 fill)
  -> first present (runtime/Wayland 초기화 동안 boot frame 유지)
  -> RuntimeClient open (notify_ui_ready socket + sync socket)
  -> WaylandCompositor::initialize (/run/user/1000/wayland-0 생성)
  -> notify_ui_ready -> runtimed가 autostart launch 해제
  -> main loop:
       input drain, runtime poll, console poll, cursor blink,
       partial dirty rect coalesce, render & present
```

동기 임계 경로는 의도적으로 짧습니다.

1. 첫 present는 full chrome이 아니라 단색을 사용합니다. chrome 합성은 첫
   idle frame으로 미뤄지므로, 사용자에게는 `display_create_surface` 직후
   몇 frame 안에 항상 pixel이 보입니다.
2. console window surface는 runtime polling 중에 "prime" 하지 않습니다.
   `draw_console_window`가 첫 draw에서 cached surface를 rebuild하며,
   동일한 총 작업량을 runtime refresh budget이 아닌 render budget으로
   옮깁니다.
3. `terminal.rs`의 cell loop는 bulk render가 이미 `layout.bounds`를
   채웠다면 cell별 background fill을 생략합니다. cell별 paint는
   cursor blink 같은 single-cell repaint에서만 유지됩니다.

### Translucent Aero Palette

chrome palette는 translucent monochrome blue입니다. glass / inner panel은
`fill_rect_alpha`로 쌓아서 desktop band가 rail을 통해 비치도록 합니다.
hilight / shadow는 frosted glass 느낌을 내도록 tune 했습니다.

- rail은 상단 sheen 반쪽 + 하단 settled 반쪽 + inner elevated bloom를
  그리고, top edge에 1px specular line, base line에 옅은 그림자를
  추가합니다.
- window title bar는 같은 two-layer 기법을 쓰며 focus 상태에서 accent
  overlay 한 겹이 더 들어갑니다.
- 마우스 커서는 기존의 soft blue/white tone을 그대로 둡니다. theme
  recolor 시에 `cursor_sprites.rs`나 `canvas.rs`의 cursor color는
  바꾸지 마세요.

### Wayland Surface

compositor는 `/run/user/1000/wayland-0`로 standard Wayland socket을
노출합니다. surface, buffer, frame callback, damage는 canonical Wayland
protocol을 사용합니다. tree 내의 Wayland client (`apps/wayclick`, console
host)와 예정된 외부 client에 별도 wrapper가 필요하지 않습니다.

WayClick도 일반 client event queue와 blocking dispatch loop를 사용합니다.
`wl_surface.frame` callback은 commit에 연결되는 one-shot이며,
`wl_buffer.release`를 받은 뒤에만 buffer를 재사용합니다. profile 모드는
callback마다 대표 redraw를 더 요청할 뿐 protocol이나 GPU 전용 app API를
바꾸지 않습니다.
uiserver는 input, Wayland, runtime damage를 모은 뒤 한 번만 present하며,
폐기한 cursor-only early-present 경로를 다시 두지 않습니다. 실제 present
성공은 누적되지 않는 callback permit 하나를 만들고, 다음 present 전에
소비해 client가 다음 refresh용 frame을 미리 그릴 시간을 줍니다.
damage가 없는 callback-only commit은 DVM GPU present와 같은 15 ms
monotonic cadence를 사용하고, pending callback deadline도 main wait에
포함됩니다. 따라서 15/16 ms beat와 callback deadline을 넘긴 sleep을
없애면서 timer credit 누적이나 무제한 callback loop는 허용하지 않습니다.

최종 bounded capture는 sustained 55 FPS gate를 통과했습니다. 최초로
인정된 연속 3개 WayClick window는 3.060초 동안 commit/callback/release를
각각 57, 65, 68회 정확히 일치시켜 aggregate 62.092 FPS를 기록했고,
window별 callback rate는 56.394--67.342 FPS, 최대 callback gap은
45 ms였습니다. 같은 구간의 uiserver와 인증된 DVM relay도 FPS,
input-loss, cursor, fence count, backlog gate를 통과했습니다. 첫 profile
이전의 최초 window-topology repack은 별도 100 ms bound를 적용하고,
그 이후 50 ms를 넘는 loop는 계속 acceptance 실패입니다.

창 이동은 topology 변경이 아닙니다. retained scene은 각 visible window의
identity와 atlas source rectangle을 정확한 GPU layer index에 묶고, drag 중에는
그 command의 destination rectangle만 바꿉니다. 위치는 structural signature에서
제외하지만 dimensions, focus, visibility, title, ordering이나 binding 불일치는
fail-closed 전체 rebuild를 강제합니다. 따라서 pointer motion마다 2048x2048
atlas 전체를 할당, rasterize, 비교, 복사하던 경로는 허용되지 않습니다.

OS userspace event/readiness 경계는 이제 WayClick 전용 경로가 아니라 범용
cross-provider wait set으로 구현되어 있습니다. uiserver는 Wayland server의
Ring0 `EpollHandle` aggregate epoll fd(및 Ring0 `UnixSocketHandle` 소켓)와
input notification을 함께 기다리고, compat는 유한/무한
application deadline과 로컬/service-owned generation에 대한
check-register-recheck-arm-presence-check를 수행합니다. open-description
lifetime, provider epoch revoke, interest별 `ERR|HUP`, bounded
close/dup/fork/exec cleanup은 source/model 검증 범위입니다. 원격 상태변경은
동일 operation ID replay/reconciliation을 사용하고, vfsd는 endpoint 공개 전
rootd의 인증된 open-description checkpoint를 복구하며, service object는 boot entropy
capability와 sender/dependency 검사를 함께 사용합니다. 이 검사는 별도의 runtime crash injection을 대체하지 않습니다.
[KVM 부팅과 입력 검증 기록](../validation/ring3-reliability-batch.md#korean)은
확인한 범위와 시간 제한을 따로 설명합니다.

### Wayland 보강 계약

- `surface_id`는 compositor가 발급하는 고유 값이며 per-client인 `wl_surface`
  protocol id를 쓰지 않습니다. focus 비교는 `ObjectId`로 합니다.
- 한도: 전체 surface 64, client 64, client당 surface 16과 output/pointer/keyboard
  8, 보존 surface pixel 총량 상한. 초과하면 해당 client만 protocol error입니다.
- 클라이언트 SHM을 직접 매핑하지 않고 compositor 소유 메모리로 읽습니다.
  완전히 읽지 못한 프레임은 게시하지 않습니다.
- 같은 `wl_surface`에서 창을 다시 만들면 배치와 식별자는 유지하지만 제목,
  장식 설정, 최대화 이력은 초기화합니다.
- occlusion fragment packing과 동적 GPU atlas로 최대화 window도 desktop/dock과
  함께 배치됩니다(2560x1440 / 2880x1800 검증). atlas가 가득 차면 focused
  window를 먼저 packing하고 draw order는 유지하며, 빠진 window는 bounded log로
  남깁니다.
- 새로 매핑된 Wayland toplevel 창은 자동으로 포커스를 받으며, Wayland socket
  호출 중 UI phase는 7 s, 그 외 phase(`PRESENT_STEP` 진단 포함)는 3 s로
  watchdog이 판정합니다.

### Console hosting

console-hosted program (shell, PE demo)은 `runtimed`에서 console session
을, `uiserver`에서 window를 받습니다. terminal state는 `uiserver`의
`TerminalState` 안에 있고 console output ring의 input byte를 parsing
합니다. layout은 terminal monospace atlas를, chrome은 UI atlas를
사용합니다.

### Font와 UI 설정

`uiserver/build.rs`가 `assets/ui/bundle/ui.conf`의 Inter / Maple Mono font를
미리 rasterize하고 네 role의 atlas와 UTC offset(현재 +540분)을 binary에
포함합니다. 첫 desktop frame의 title, clock, date, terminal text는 filesystem
접근이나 target-side font parsing 없이 표시됩니다. build와 runtime loader는
같은 `glyph_atlas.rs` rasterizer, config parser, bounded read를 사용합니다.

background loader는 `/assets/ui/ui.conf`(개발용 `RUSTOS_UI_CONFIG` override
가능)와 font를 읽습니다. 실패하면 bundled text를 유지합니다. UI loop가
frame 사이에서 한 번만 font와 clock offset을 함께 promote하며 desktop chrome,
console surface와 terminal layout을 invalidate합니다. `font::asset_revision()`
(bootstrap 1, promotion 2)은 console raster와 retained GPU scene의 cache identity에
포함되어야 합니다. terminal metric이 달라지면 cached output을 새 grid에 replay합니다.

clock은 처음부터 실제 `SystemTime`에 active `clock.utc_offset_minutes`를
더합니다. override가 준비되기 전에는 packaged offset을 사용하며 임의의 시각을
만들지 않습니다. timezone database나 자동 DST 전환은 없습니다.

### 2026-11-26 성능 변경과 검토 범위

이번 변경은 사용자 요청에 따라 **정적 검토만 수행**했습니다. 문서의 과거 FPS,
KVM 결과는 이번 변경의 검증 결과가 아니며 새로운 성능 향상률은 측정하지 않았습니다.

GPU 명령·인코딩·damage 저장소를 프레임 간 재사용하고, 커서만 움직이는 프레임은
창 제목과 장면 서명을 다시 계산하지 않습니다. 아틀라스의 변경은 분리된 행 구간으로
유지하고, wire 한도를 넘을 때만 보수적인 경계 사각형으로 합칩니다. 실제 사용 높이는
투명 padding을 포함하며, 이전 영역을 지우는 작업도 damage에 포함됩니다.

부분 갱신 도중 전체 재구성으로 바뀌거나 GPU 제출이 거절된 경우, 다음 전체 스냅샷을
전송뿐 아니라 성공한 damage 이력에도 기록합니다. 이전 슬롯을 재사용할 때 미반영
픽셀이 되살아나지 않게 하는 계약입니다. 겹친 damage는 정확한 직전 슬롯에서도
정규화하며, 슬롯의 epoch와 fence는 제출 성공 후에만 진행합니다.

창 조각은 필요한 픽셀만 직접 복사하고 장식·모서리 alpha는 원래 창 좌표로 적용합니다.
커서는 재사용 캐시에서 strided atlas 영역으로 복사하며, blur는 같은 정수 연산을
행 순서로 처리합니다. 터미널은 VTE 배치 입력, 스크롤백 행 재사용, 한 번의 화면 이동,
보이는 셀 순회와 paint당 한 번의 폰트 조회를 사용합니다. AVX2 경로는 기존 CPU/OS
지원 검사를 유지하고 scalar와 같은 반올림·투명도·포화 규칙을 적용합니다.

Wayland 전체 damage는 새 owned snapshot을 그대로 게시하여 이전 프레임의 복제와
덮어쓰기를 없앱니다. 연속된 부분 읽기와 viewport 행 재사용도 적용하지만, 클라이언트
SHM 직접 매핑 금지, 완전 초기화 후 게시, bounded worker와 release/callback 순서는
유지합니다. 상세 소유권·복구 규칙과 근거 자료는 위 영문 절에 함께 기록했습니다.

### Profile Trace

profiling이 필요할 때 `RUSTOS_UI_PROFILE=1`을 설정하면 `profile::record_*`
경로가 활성화됩니다. `uiserver profile: ...` summary line이 debugcon으로
flush 되며 slow refresh / slow present regression을 진단할 때 주된
도구입니다. 새 diag print를 추가하기 전에 이 경로를 먼저 사용하세요.
per-frame cursor/render pipeline sample도 profile-gated입니다. 일반 KVM
실행은 coarse `uiserver: update tick` liveness line만 유지합니다.

## Mixed window and hostile SHM ownership contract

`uiserver::app::stacking` owns a single back-to-front `WindowId` order for
console sessions and compositor-unique Wayland surface identities. Raising a
window preserves every other window's relative order, including windows of the
other protocol. Minimize retains an identity's place but removes it from paint
and hit testing; destruction prunes it. Focus fallback selects the highest
remaining visible mixed-stack identity, not a protocol-preferred list. CPU and
GPU composition consume the same draw plan. The retained GPU scene signature
includes the complete mixed order.

Pointer routing resolves the top frame in that order first. Console frames and
upper window chrome occlude lower Wayland client areas. Only a top Wayland
client area becomes the protocol pointer target; the protocol owner must not
independently rescan lower surfaces. A pressed Wayland button retains its
original surface grab through release, including motion outside that surface.
Keyboard routing follows the explicit UI focus, including focus changes caused
by console refresh. Input targets must be refreshed after snapshot, placement,
minimize, and geometry changes even if the physical pointer is stationary.

Initial placement also crosses protocol boundaries. Console creation checks
existing console and Wayland origins. Newly mapped Wayland snapshots recheck
against the mixed desktop, because allocation can precede console publication.
Placement tries distinct legal cascade origins and, if the old cascade wraps,
other legal origins. Identical origins are allowed only when all legal origins
are occupied (for example a tiny output admitting just one console position).
Requested Wayland moves reserve their destination against later console
creation. The UI retains acknowledged geometry until a matching owner snapshot
retires the move; enqueue is not completion. Explicit user drag/maximize cancels
a pending initial placement. A disappearing surface retires its pending move.

Wayland SHM is **never mapped or borrowed as a slice** by uiserver. The pool owns
a readable fd and a bounded logical extent (64 MiB); buffers share its ownership
so pool destruction and client-fd close cannot invalidate a live attachment.
Creation and growth probe the requested extent with positioned reads; invalid
sizes, shrink, unreadable/undersized backing, and failed growth leave the old
extent unchanged. These probes are admission checks, not a TOCTOU safety claim.
Every actual copy uses exact `pread` into compositor-owned bytes. EOF and I/O
failure reject a copy; an unreadable commit disconnects that client rather than
pretending the old pixels were a successful new commit. Damage is staged before
mutating retained pixels, so a short read cannot partially publish an update.
Copied pixels remain valid after truncate, fd close, pool/buffer destruction,
and disconnect. Same-layout commits declaring no damage can retain the existing
owned pixels without touching client storage.

This strategy uses RustOS's existing `SYS_PREAD64`: memfds use locked `read_at`
and bounded bounce copies (`kernel/compat/src/user/syscall/linux/service_ops/local_memfd_io.rs`), while VFS uses
bounded positioned read replies. No kernel change or file seals are required.
Concurrent client writes/truncate/regrow may yield mixed-frame contents (clients
must synchronize their buffers), but cannot invalidate compositor memory or
cause compositor SIGBUS. Snapshot I/O belongs to the Wayland protocol worker,
never to the UI rendering thread when that owner is isolated.

Regression evidence is in `app::stacking` (mixed history, retirement and both
placement orders), `render::tests` (identical paint/hit order and chrome
occlusion), and `wayland::tests` (exact pointer target/grab, undersized/truncated
backing, transactional damage, resize rollback/growth, shared-fd lifecycle and
concurrent truncation). Guest GUI capture and end-to-end worker/atlas validation
remain integration checks, not claims established by host unit tests.
