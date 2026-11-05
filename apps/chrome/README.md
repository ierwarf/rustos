# Google Chrome — local import

[English](#english) | [한국어](#korean)

## English

Chrome is a proprietary external Linux application, not a RustOS build product.
The binary is not committed or downloaded by normal builds. Import the already
downloaded official amd64 `.deb` explicitly:

```sh
python3 tools/agent/import-chrome.py
cargo xtask build
```

The importer checks the pinned SHA-256 for Google Chrome 157.0.8103.38-1,
extracts only `opt/google/chrome` into the ignored `apps/chrome/bundle/`, and
publishes the ignored `RUSTOS.package.toml` from the checked-in example last.
The `prebuilt-app` builder installs the bundle at `/apps/chrome`. A fresh
checkout without the import has no Chrome package and no download requirement.
A changed package version requires an explicitly reviewed `--sha256` value.
Existing imports are never overwritten implicitly.

`Google Chrome` appears on uiserver's desktop with a browser glyph. Its startup
mode is **none**: no Chrome autostart file or boot/session launch is installed.
The desktop action names the real `/apps/chrome/chrome` ELF, not the host shell
wrapper, and selects Wayland. Import, build, and ordinary boot do not run Chrome.

This is installation/discovery, not proof that Chrome runs. Shared-library,
Linux syscall, sandbox, writable-profile and Wayland compatibility still need
separate tests. No `--no-sandbox` fallback is added. No package maintenance
scripts, host installation, cron integration, or host setuid/setgid permissions
are applied; privilege bits are stripped during extraction. The recorded digest
is a local pin, not an independent verification of Google's package signature.
Locally imported payloads are not checked-in formal source evidence.

## 한국어

Chrome은 RustOS가 빌드하는 프로그램이 아니라 외부 Linux 앱입니다. 바이너리는
Git에 넣지 않으며 일반 빌드에서 자동 다운로드하지 않습니다. 이미 받은 공식
amd64 `.deb`를 다음과 같이 명시적으로 가져옵니다.

```sh
python3 tools/agent/import-chrome.py
cargo xtask build
```

가져오기는 고정 SHA-256을 확인한 뒤 Chrome 본체와 리소스만 로컬 `bundle/`에
풀고 패키지 등록 파일을 마지막에 만듭니다. 이후 이미지의 `/apps/chrome`에
포함됩니다. 가져오지 않은 checkout에는 Chrome 패키지가 없으며 기존 가져오기
결과를 자동으로 덮어쓰지 않습니다.

uiserver 바탕화면에 `Google Chrome`이 표시되지만 **자동 실행하지 않습니다**.
실행 아이콘은 실제 Chrome ELF와 Wayland 옵션에 연결됩니다. Chrome 실행 호환성은
아직 보장하지 않으며 라이브러리·시스템 호출·샌드박스·프로필·Wayland 시험은
별도입니다. 호스트 설치, 패키지 스크립트 실행, cron 등록, 샌드박스 우회는 하지
않고 setuid/setgid 비트도 제거합니다.
