# AnyDrop

Windows ↔ macOS(Intel / Apple Silicon) ↔ Linux 사이에서 **AirDrop처럼 파일을 주고받는** 오픈소스 도구입니다.
광고 없음, 서버 없음. 같은 LAN 또는 **Tailscale** 네트워크면 동작합니다. Rust로 작성되었습니다.

```
anydrop receive                 # 받는 쪽: 대기 (LAN에 자동 광고)
anydrop list                    # 주변 기기 찾기 (LAN + Tailscale)
anydrop send MacBook a.pdf dir/ # 보내는 쪽: 이름/IP/Tailscale 호스트명으로 전송
```

## 구조

```
anydrop/
├─ crates/anydrop-core/     # 라이브러리 (GUI에서도 그대로 재사용)
│  ├─ src/protocol.rs       # 프레임 포맷: u32 길이 + JSON 제어메시지, 파일은 raw 스트림
│  ├─ src/transfer.rs       # 송신/수신 엔진, SHA-256 검증, .anydrop-part 임시파일
│  ├─ src/discovery.rs      # mDNS/DNS-SD (_anydrop._tcp) 광고/탐색 — 순수 Rust, Bonjour 불필요
│  ├─ src/tailscale.rs      # `tailscale status --json` → 피어 목록 → 포트 probe
│  ├─ src/paths.rs          # 경로 보안 + 크로스플랫폼 파일명(NFC 한글, Windows 금지문자)
│  └─ tests/transfer.rs     # E2E 시뮬레이션 테스트
└─ crates/anydrop-cli/      # `anydrop` 명령행 프로그램
```

### 프로토콜 (v1, TCP 47820)

```
Sender                                   Receiver
  | -- Hello{version, device} ----------->  |
  | <------------ HelloAck{version, device} |
  | -- Offer{files[], total_size} -------->  |   (ping은 여기서 종료)
  | <-------------- Accept | Reject{reason}  |   (수신자가 y/N 확인)
  | == 파일마다: raw bytes + FileEnd{sha256} |   (수신측 해시 검증 후 rename)
  | <---------------- Done | Error{message}  |
```

안전장치: `..`/절대경로/드라이브 문자 차단, 같은 이름은 `name (1).ext`로 저장, 중단·해시 불일치 시 임시파일 삭제,
macOS NFD 한글 파일명 → NFC 변환(Windows에서 자모 분리 방지), `<>:"|?*`·`CON`·`NUL` 등 Windows 금지 이름 치환.

## 빌드

공통: Rust 1.75+ (`rustup` 권장). 결과물은 의존성 없는 단일 실행파일입니다.

### Windows 10/11 (x64, ARM64)

1. [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) 설치 → "C++를 사용한 데스크톱 개발" 워크로드 선택
2. [rustup-init.exe](https://rustup.rs) 실행 (기본값 `x86_64-pc-windows-msvc`)
3. PowerShell:
   ```powershell
   git clone <repo-url> anydrop; cd anydrop
   cargo test
   cargo build --release
   # 결과: target\release\anydrop.exe
   ```
   ARM64 PC용: `rustup target add aarch64-pc-windows-msvc; cargo build --release --target aarch64-pc-windows-msvc`

### macOS (Apple Silicon / Intel / Universal)

```bash
xcode-select --install                      # 링커
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo test && cargo build --release         # 현재 머신 아키텍처용: target/release/anydrop
```
Universal 바이너리(Intel + Apple Silicon 하나로):
```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
lipo -create -output anydrop target/aarch64-apple-darwin/release/anydrop target/x86_64-apple-darwin/release/anydrop
lipo -info anydrop      # Architectures: x86_64 arm64
```

### Linux (참고)

```bash
cargo build --release
# Linux에서 Windows exe 크로스빌드:
sudo apt install mingw-w64 && rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

### CI

`.github/workflows/build.yml`이 push마다 Windows x64/ARM64, macOS arm64/x64/universal, Linux 바이너리를 만들어 Artifacts로 올립니다.
(macOS 바이너리는 리눅스에서 링크할 수 없으므로 CI 또는 맥에서 빌드하세요.)

## 첫 실행 시 OS 설정

| OS | 해야 할 일 |
|---|---|
| Windows | 첫 `receive` 때 방화벽 팝업 → **개인 네트워크 허용**. 팝업이 없었다면 관리자 PowerShell: `New-NetFirewallRule -DisplayName AnyDrop -Direction Inbound -Program "C:\path\anydrop.exe" -Action Allow -Profile Private`. 네트워크 프로필이 "공용"이면 mDNS가 막히므로 "개인"으로 변경 |
| macOS 15+ | 시스템 설정 → 개인정보 보호 및 보안 → **로컬 네트워크**에서 터미널(또는 iTerm) 허용. 없으면 mDNS 탐색과 LAN 연결이 실패함 |
| macOS (다운로드한 바이너리) | 서명되지 않은 실행파일이므로 `xattr -d com.apple.quarantine ./anydrop` |
| macOS 방화벽 켜짐 | "들어오는 연결 허용" 팝업에서 허용 |
| Tailscale | 양쪽 기기에 Tailscale 로그인. ACL을 쓰면 TCP 47820 허용 |

## 사용법

```bash
# 받는 쪽 (기본 저장 위치: ~/Downloads/AnyDrop, 매번 수락 여부 질문)
anydrop receive
anydrop receive -d D:\Inbox -y          # 자동 수락
anydrop -n "회의실PC" receive            # 표시 이름 지정

# 보내는 쪽
anydrop list                            # LAN(mDNS) + Tailscale 피어 목록
anydrop send MacBook ./report.pdf ./photos   # 이름(대소문자 무시, 앞부분만 써도 됨)
anydrop send 192.168.0.23 big.iso
anydrop send macbook-pro:47820 a.zip     # host:port, Tailscale MagicDNS 이름도 가능
anydrop ping MacBook                     # 연결 확인
```

TARGET 해석 순서: `IP[:port]` → `host:port` → LAN mDNS 이름 → Tailscale 피어 HostName → DNS/MagicDNS.

## 테스트 방법

### 1) 자동 테스트 (어느 OS에서나)
```bash
cargo test
```
단위 7개(경로 보안, NFD→NFC, 프레임, Tailscale JSON 파싱) + E2E 7개(20MB+폴더+한글 전송, 이름 충돌,
수신 거절, ping, 경로 탐색 공격, 해시 위조, 전송 중단 후 임시파일 정리).

### 2) 한 대의 PC에서 두 기기 흉내 내기
터미널 두 개:
```bash
anydrop -n PC-A receive -d ./inboxA -y
anydrop -n PC-B receive -d ./inboxB -p 47821
```
세 번째 터미널:
```bash
anydrop list
anydrop send PC-A ./somefile
anydrop send PC-B ./somefile      # PC-B 터미널에서 y/N 입력
```

### 3) 실제 Windows ↔ Mac (같은 Wi-Fi)
1. Mac: `./anydrop receive` → Windows: `anydrop.exe list` 에 Mac이 보이는지 확인
2. Windows: `anydrop.exe send <Mac이름> C:\Users\me\Pictures` → Mac에서 `y`
3. 반대로 Windows에서 `receive`, Mac에서 `send`
4. 해시 비교: Mac `shasum -a 256 파일`, Windows `Get-FileHash 파일 -Algorithm SHA256`
5. 한글/공백/특수문자(`보고서?.txt`) 파일명, 1GB 이상 파일, 폴더째 전송, 전송 중 Ctrl+C 를 각각 확인

`list`에 안 보이면: `anydrop send <상대IP> 파일` 로 직접 시도 → 성공하면 mDNS(UDP 5353)만 막힌 것(방화벽/공용 네트워크/AP 격리),
실패하면 TCP 47820 방화벽 문제입니다.

### 4) Tailscale
1. 두 기기에서 `tailscale status` 로 서로 보이는지 확인
2. 한쪽 `anydrop receive`, 다른 쪽 `anydrop list` → `tailscale` 출처로 표시됨
3. `anydrop send <tailscale 호스트명> 파일` 또는 `anydrop send 100.x.y.z 파일`

## 보안 참고 (v0.1)

- 데이터는 SHA-256으로 무결성 검증하지만 **LAN 구간은 암호화하지 않습니다.** 신뢰할 수 있는 네트워크에서만 쓰거나 Tailscale(WireGuard 암호화)을 이용하세요.
- 수신 측은 기본적으로 매 전송마다 수락을 묻습니다. `-y`는 신뢰 네트워크에서만 사용하세요.

## 로드맵

- [ ] TLS(자체 서명 + 지문 확인/페어링 PIN) 암호화
- [ ] GUI (egui 또는 Tauri), 트레이 상주, Windows 탐색기/Finder "보내기" 메뉴
- [ ] 이어받기(resume), 클립보드/텍스트 전송
- [ ] 코드 서명(Windows Authenticode, macOS notarization), 설치 패키지(MSI/DMG)

## 라이선스

MIT OR Apache-2.0
