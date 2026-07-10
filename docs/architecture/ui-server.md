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

### Console Hosting

Console-hosted programs (the shell, the PE demos) get a console session
from `runtimed` and a window from `uiserver`. The terminal state lives in
`uiserver`'s `TerminalState`, parsing input bytes from the console output
ring. Layout uses the terminal monospace atlas; chrome uses the UI atlas.

### Profile Trace

Set `RUSTOS_UI_PROFILE=1` when profiling is needed to enable
`profile::record_*` paths. The summary lines tagged
`uiserver profile: ...` get flushed to debugcon and are the primary tool
for diagnosing slow refresh / slow present regressions. Use them before
adding new diag prints.
Per-frame cursor/render pipeline samples are also profile-gated; normal Xen
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
per-frame cursor/render pipeline sample도 profile-gated입니다. 일반 Xen
실행은 coarse `uiserver: update tick` liveness line만 유지합니다.
