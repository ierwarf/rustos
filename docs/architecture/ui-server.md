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
the Wayland server's aggregate epoll fd together with its input notification;
compat honors finite/infinite application deadlines and performs atomic
check-register-recheck-arm-presence-check against service-owned generations.
Open-description lifetime, provider epoch revoke, per-interest `ERR|HUP`, and
bounded close/dup/fork/exec cleanup are source/model covered. State mutations
now use exact operation IDs with replay/reconciliation, vfsd restores its
rootd-retained authenticated epoll checkpoint before endpoint publication, and
service objects use boot-entropy capabilities plus sender/dependency checks.
Runtime crash injection and the live KVM workload remain unclaimed; host
admission rejects the available NVIDIA render node. Fuchsia's
peered IOBuffer remains a useful commercial-OS reference for the object shape,
not an ABI to copy verbatim:
<https://fuchsia.dev/fuchsia-src/reference/kernel_objects/io_buffer>.

### Console Hosting

Console-hosted programs (the shell, the PE demos) get a console session
from `runtimed` and a window from `uiserver`. The terminal state lives in
`uiserver`'s `TerminalState`, parsing input bytes from the console output
ring. Layout uses the terminal monospace atlas; chrome uses the UI atlas.

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
aggregate epoll fd와 input notification을 함께 기다리고, compat는 유한/무한
application deadline과 service-owned generation에 대한
check-register-recheck-arm-presence-check를 수행합니다. open-description
lifetime, provider epoch revoke, interest별 `ERR|HUP`, bounded
close/dup/fork/exec cleanup은 source/model 검증 범위입니다. 원격 상태변경은
동일 operation ID replay/reconciliation을 사용하고, vfsd는 endpoint 공개 전
rootd의 인증된 epoll checkpoint를 복구하며, service object는 boot entropy
capability와 sender/dependency 검사를 함께 사용합니다. 남은 release gate는
runtime crash injection과 NVIDIA host admission 때문에 실행하지 못한 live
KVM workload이며, 이를 source/model 성공으로 대체해 기록하지 않습니다.

### Console hosting

console-hosted program (shell, PE demo)은 `runtimed`에서 console session
을, `uiserver`에서 window를 받습니다. terminal state는 `uiserver`의
`TerminalState` 안에 있고 console output ring의 input byte를 parsing
합니다. layout은 terminal monospace atlas를, chrome은 UI atlas를
사용합니다.

### Profile Trace

profiling이 필요할 때 `RUSTOS_UI_PROFILE=1`을 설정하면 `profile::record_*`
경로가 활성화됩니다. `uiserver profile: ...` summary line이 debugcon으로
flush 되며 slow refresh / slow present regression을 진단할 때 주된
도구입니다. 새 diag print를 추가하기 전에 이 경로를 먼저 사용하세요.
per-frame cursor/render pipeline sample도 profile-gated입니다. 일반 KVM
실행은 coarse `uiserver: update tick` liveness line만 유지합니다.
