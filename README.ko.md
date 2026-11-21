<div align="center">

# Skyline Speeder

**손실이 많고 지연이 긴 링크를 위한 eBPF 기반 송신 측 TCP 가속**

*데이터를 보내는 서버에만 설치합니다. 클라이언트는 기본 TCP 그대로이며 소프트웨어도, 패치도, 설정도 필요 없습니다.*

[![License](https://img.shields.io/badge/license-GPL--2.0-blue.svg)](LICENSE)
[![Kernel](https://img.shields.io/badge/kernel-6.12%20LTS%2B-orange.svg)](#요구-사항)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu%20%7C%20Fedora%20%7C%20RHEL%20%7C%20Alpine-red.svg)](install.sh)
[![eBPF](https://img.shields.io/badge/eBPF-CO--RE%20struct__ops-green.svg)](bpf/)
[![Rust](https://img.shields.io/badge/rust-1.82%2B-black.svg)](rust-toolchain.toml)

[English](README.md) · [简体中文](README.zh.md) · [繁體中文](README.zh-TW.md) · [日本語](README.ja.md) · **한국어** · [Русский](README.ru.md) · [Français](README.fr.md)

---

### **[Skyline Connect](https://www.skylineconnect.io)** 단독 후원

---

</div>

## 소개

Skyline Speeder는 한쪽에만 설치하는 TCP 가속 소프트웨어입니다. 데이터를 보내는 서버에서 eBPF 프로그램으로 동작하며, 패킷 손실이 심하고 왕복 시간이 길어 표준 TCP가 무너지는 링크를 위해 만들어졌습니다. 수신 측은 전혀 건드리지 않습니다.

서버의 혼잡 제어를 `skyline_cc`로 바꾸며, 전제는 하나뿐입니다: **패킷 손실 자체는 혼잡에 대한 정보를 담고 있지 않다.**

- **먼저 측정하고 보낸다:** 윈도는 측정한 대역폭 × 기준 RTT를 따르며, 손실 때문에 줄어들지 않습니다.
- **보정하고 페이싱한다:** 측정한 손실률만큼 전송 속도를 (상한까지) 높이고, `fq`를 거쳐 고른 간격으로 내보냅니다.
- **브레이크는 하나:** 큐잉 지연이 늘거나 ECN 표시가 있을 때만 속도를 줄입니다.
- **짧은 응답 우선:** 각 연결의 핸드셰이크와 처음 64 KiB를 10 ms 간격으로 두 번 보내므로, 패킷 하나를 잃어도 재전송 타이머가 아니라 몇 밀리초만 늦어집니다.

> [!IMPORTANT]
> 손실이 큐와 무관할 때만 효과가 있습니다. 고정 속도 UDP 흐름에는 손실이 없는데 TCP만 재전송이 많다면, 손실의 원인은 혼잡한 병목 지점의 큐 넘침이며 이 도구가 맞지 않습니다.

## 요구 사항

- `/sys/kernel/btf/vmlinux`가 있는 **Linux 6.12 LTS 이상**. 6.1·6.6 LTS 계열과 Debian 12, Ubuntu 22.04/24.04의 기본 커널은 너무 오래되었습니다.
- 물리 호스트 또는 KVM 가상 머신. 컨테이너(LXC, Proxmox CT)에서는 동작하지 않습니다.
- Debian, Ubuntu, Fedora, RHEL 계열(Rocky Linux, AlmaLinux, CentOS Stream) 또는 Alpine.

## 빠른 시작

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

Alpine에서는 root로:

```sh
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

최신 공개 릴리스를 설치하고(호스트에 컴파일러를 설치하지 않습니다), 모든 BPF 오브젝트를 커널 검증기로 확인한 뒤, 새로 맺어지는 모든 TCP 연결이 `skyline_cc`를 쓰도록 하고 재부팅 후에도 유지합니다. 프록시를 설치하지 않으며 포트도 열지 않습니다. 설치 후 확인:

```bash
ssctl status    # 이 호스트가 전환되었는가? ACCELERATING이면 성공
ssctl flows     # 어떤 연결이 가속되고 있고, 알고리즘이 무엇을 결정했는가
```

제거(호스트는 bbr + fq로 돌아갑니다):

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --uninstall
```

## 고급 사용법

**복제한 저장소에서 설치**하면 실행 전에 스크립트를 읽어 볼 수 있습니다:

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh --check      # 사전 점검만, 아무것도 바꾸지 않음
sudo ./install.sh              # 공개 릴리스 (실행할 수 없을 때만 소스에서 빌드)
sudo ./install.sh --source     # 이 소스 트리에서 빌드
sudo ./install.sh --release v0.6.0   # 지정한 공개 릴리스, 안 되면 중단
sudo ./install.sh --uninstall --restore-pre-install   # 제거하고, 설치 전의 혼잡 제어와 qdisc로 복원
```

업그레이드는 설치 스크립트를 다시 실행하면 됩니다. 기존 연결을 최대 60초 동안 정리한 뒤 `skyline_cc`를 다시 연결합니다.

**일상 운영:**

```bash
sudo ssctl drain                                # 부드러운 분리: 새 연결은 fallback_cc로
sudo ssctl enable                               # 다시 연결
sudo ssctl enable --all-off                     # 모든 모듈을 끈 skyline_cc: 비교용 기준선
sudo ssctl set-redundancy --disable             # 첫 데이터 이중 전송 중지 (트래픽 종량제 회선용)
sudo ssctl set-module-config --max-pacing-mbps 500   # 전체 덮어쓰기: 생략한 플래그는 기본값으로 돌아감
```

기본값은 조정을 마친 값이라 대부분의 환경에서는 아무것도 바꿀 필요가 없습니다. `ssctl set-*`로 바꾼 값은 데몬이 재시작될 때까지 유지되며, 계속 쓰려면 `/etc/skyline-speeder/speeder.toml`에 적어 둡니다.

**문서**(영어):

| 문서 | 내용 |
|---|---|
| [docs/01-features.md](docs/01-features.md) | 설치 스크립트가 하는 일, 동작 원리, 기능별 기본값·비용·설정 방법, `ssctl status`와 `ssctl flows`의 줄별 설명, 소스에서 빌드 |
| [docs/usage.md](docs/usage.md) | 모든 스위치와 파라미터 및 범위, 프리셋 4종, 문제 해결 |
| [DEPLOY.md](DEPLOY.md) | 배포·운영 매뉴얼: 설치, 검증, 업그레이드, 제거 |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` 명령, 통신 프로토콜, 설정 항목, 이벤트 코드 |
| [docs/03-design.md](docs/03-design.md) | 아키텍처와 각 모듈의 동작 방식 |
| [docs/04-performance-report.md](docs/04-performance-report.md) | 통제된 2대 VM 테스트베드에서 bbr과의 비교 |

## 실측 성능

bbr, [tcp-brutal](https://github.com/HyNetworks/tcp-brutal), skyline_cc를 모두 서버 측에만 적용해 같은 실제 국제 구간에서 비교했습니다: 싱가포르의 KVM VPS에서 화남(광둥)의 Alibaba Cloud ECS(200 Mbps, 기본 TCP)로 전송. RTT 70-73 ms, 손실률 10-30%, 저녁 피크 시간대 측정. 8 로테이션 × 3 알고리즘 × 5 시나리오.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="게임, 동영상, 웹, 대용량 전송에서 bbr 기준 대비 tcp-brutal과 skyline_cc의 실측 결과" src="docs/images/single-ended-ataglance-light.svg">
</picture>

| 시나리오 | 승자 | 차이 |
|---|---|---|
| **동영상** (6 MB 청크 @ 48 Mbps) | **tcp-brutal** | 재생 시작 7/8 로테이션, 청크 p95 6/8 |
| **게임** (ping-pong, 60 msg/s) | **skyline_cc** | 지터 5/8, 150 ms 초과 끊김 4/8 |
| **대용량, 스트림 4개** | skyline_cc와 tcp-brutal, 모두 bbr보다 앞섬 | 중앙값 117 / 115 vs 77 Mbps |
| **대용량, 스트림 1개** | bbr과 skyline_cc 동률, tcp-brutal 최하위 | 중앙값 59 / 53 / 46 Mbps |
| **웹 페이지 로딩** | 안정적인 승자 없음 | 로테이션 간 편차가 차이보다 큼 |
| **서버 재전송**, 전체 송신 세그먼트 대비 비율 | bbr이 가장 낮음 | bbr 9.1%, skyline_cc 10.4%, tcp-brutal 10.9% |

한 구간에서의 현장 측정이며, 여러분의 링크에서도 같은 결과를 보장하지 않습니다. skyline_cc는 0.1.0의 기본 계수, 즉 지금의 "high random loss" 프리셋으로 실행했습니다. 방법, 조건, 전체 결과: [docs/01-features.md](docs/01-features.md#measured-performance-in-detail).

## 라이선스

GPL-2.0입니다. [LICENSE](LICENSE)와 [NOTICE](NOTICE)를 참고하세요. Copyright (c) 2026 **CYBERVERSE LLC**. "Skyline Speeder"와 "CYBERVERSE"는 CYBERVERSE LLC의 상표입니다. 기여하기 전에 [CONTRIBUTING.md](CONTRIBUTING.md)를 읽어 주세요.

---

<div align="center">

**[Skyline Connect](https://www.skylineconnect.io)** 단독 후원

</div>
