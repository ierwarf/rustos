# Rust를 통한 UEFI OS 개발

## 패키지 설치

```bash
sudo apt update

sudo apt install -y rustup

rustup default nightly
rustup component add rust-src llvm-tools-preview
rustup target add x86_64-unknown-uefi

sudo apt install -y make qemu-system-x86 ovmf
```

## 빌드

프로젝트 루트에서 아래 명령을 실행하세요.

```bash
make build
```

빌드가 완료되면 UEFI 실행 파일이 `build/EFI/BOOT/BOOTX64.EFI`에 생성됩니다.
빌드 산물을 삭제하려면 아래 명령을 실행하세요.

```bash
make clean
```

## 실행

아래 명령으로 QEMU에서 실행할 수 있습니다.

```bash
./run.sh
```

run.sh 는 기본적으로 빌드를 포함하고 있기 때문에 별다른 명령 없이 바로 실행하셔도 됩니다.