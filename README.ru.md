<div align="center">

# Skyline Speeder

**Ускорение TCP на стороне отправителя для каналов с большими потерями и задержками, на eBPF**

*Устанавливается только на сервер-отправитель. Клиенты остаются на обычном TCP: без программ, патчей и настройки.*

[![License](https://img.shields.io/badge/license-GPL--2.0-blue.svg)](LICENSE)
[![Kernel](https://img.shields.io/badge/kernel-6.12%20LTS%2B-orange.svg)](#требования)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu%20%7C%20Fedora%20%7C%20RHEL%20%7C%20Alpine-red.svg)](install.sh)
[![eBPF](https://img.shields.io/badge/eBPF-CO--RE%20struct__ops-green.svg)](bpf/)
[![Rust](https://img.shields.io/badge/rust-1.82%2B-black.svg)](rust-toolchain.toml)

[English](README.md) · [简体中文](README.zh.md) · [繁體中文](README.zh-TW.md) · [日本語](README.ja.md) · [한국어](README.ko.md) · **Русский** · [Français](README.fr.md)

---

### Эксклюзивный спонсор — **[Skyline Connect](https://www.skylineconnect.io)**

---

</div>

## Что это

Skyline Speeder — одностороннее ускорение TCP. Оно работает в виде программ eBPF на сервере, который отправляет данные, и рассчитано на каналы с большими потерями пакетов и долгим временем приёма-передачи, где обычный TCP проседает. Принимающая сторона не меняется.

Оно заменяет управление перегрузкой на сервере алгоритмом `skyline_cc`, построенным на одном допущении: **потеря пакета сама по себе ничего не говорит о перегрузке.**

- **Сначала измерить, потом отправлять:** окно следует за измеренной полосой × базовый RTT, и потеря пакета его не уменьшает.
- **Компенсация и равномерная отправка:** скорость повышается на измеренную долю потерь (до потолка) и уходит равными интервалами через `fq`.
- **Один тормоз:** замедляют его только рост задержки в очереди и метки ECN.
- **Короткие ответы в первую очередь:** рукопожатие и первые 64 КиБ каждого соединения отправляются дважды с интервалом 10 мс, поэтому одна потеря стоит миллисекунд, а не таймера повторной передачи.

> [!IMPORTANT]
> Это помогает, только если потери не связаны с очередями. Если UDP-поток с постоянной скоростью идёт без потерь, а TCP массово переотправляет, потери вызваны переполнением очереди в перегруженном узком месте, и этот инструмент не подходит.

## Требования

- **Linux 6.12 LTS или новее** с `/sys/kernel/btf/vmlinux`. Ветки 6.1 и 6.6 LTS и штатные ядра Debian 12 и Ubuntu 22.04/24.04 слишком старые.
- Физический хост или виртуальная машина KVM, не контейнер (LXC, Proxmox CT).
- Debian, Ubuntu, Fedora, семейство RHEL (Rocky Linux, AlmaLinux, CentOS Stream) или Alpine.

## Быстрый старт

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

В Alpine, от root:

```sh
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

Устанавливается последний опубликованный релиз, без компилятора на хосте; каждый объект BPF проходит проверку верификатором ядра, затем `skyline_cc` становится управлением перегрузкой для всех новых TCP-соединений и остаётся им после перезагрузки. Прокси не ставится, порты не открываются. Проверка после установки:

```bash
ssctl status    # переключён ли хост? ACCELERATING означает да
ssctl flows     # какие соединения ускоряются и что решил алгоритм
```

Удаление (хост переходит на bbr + fq):

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --uninstall
```

## Расширенное использование

**Установка из клона репозитория**, чтобы прочитать скрипты до запуска:

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh --check      # только предварительная проверка, ничего не меняет
sudo ./install.sh              # опубликованный релиз (сборка из исходников, только если он не запустится)
sudo ./install.sh --source     # собрать это дерево из исходников
sudo ./install.sh --release v0.6.0   # этот опубликованный релиз, иначе остановиться
sudo ./install.sh --uninstall --restore-pre-install   # удалить и вернуть прежние управление перегрузкой и qdisc
```

Для обновления запустите установщик ещё раз. Он завершает текущие соединения (до 60 секунд) и снова подключает `skyline_cc`.

**Повседневная работа:**

```bash
sudo ssctl drain                                # мягкое отключение: новые соединения идут на fallback_cc
sudo ssctl enable                               # подключить снова
sudo ssctl enable --all-off                     # skyline_cc со всеми модулями выключенными: база для сравнения
sudo ssctl set-redundancy --disable             # не отправлять начало соединений дважды (если трафик тарифицируется)
sudo ssctl set-module-config --max-pacing-mbps 500   # полная перезапись: пропущенные флаги возвращаются к умолчаниям
```

Значения по умолчанию подобраны, и в большинстве случаев менять ничего не нужно. Изменение через `ssctl set-*` действует до перезапуска демона; чтобы сохранить его, запишите его в `/etc/skyline-speeder/speeder.toml`.

**Документация** (на английском):

| Документ | Содержание |
|---|---|
| [docs/01-features.md](docs/01-features.md) | Что делает установщик, как это работает, каждая функция с её значением по умолчанию, ценой и переключателем, каждая строка `ssctl status` и `ssctl flows`, сборка из исходников |
| [docs/usage.md](docs/usage.md) | Все переключатели и параметры с диапазонами, четыре пресета, поиск неисправностей |
| [DEPLOY.md](DEPLOY.md) | Руководство по развёртыванию и эксплуатации: установка, проверка, обновление, удаление |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | Команды `ssctl`, протокол обмена, поля конфигурации, коды событий |
| [docs/03-design.md](docs/03-design.md) | Архитектура и устройство каждого модуля |
| [docs/04-performance-report.md](docs/04-performance-report.md) | Контролируемый стенд из двух ВМ, сравнение с bbr |

## Измеренная производительность

bbr, [tcp-brutal](https://github.com/HyNetworks/tcp-brutal) и skyline_cc, каждый только на сервере, на одном реальном международном маршруте: KVM VPS в Сингапуре отправляет на Alibaba Cloud ECS в Южном Китае (Гуандун, 200 Мбит/с) с обычным TCP. RTT 70-73 мс, потери 10-30%, вечерний час пик; 8 ротаций × 3 алгоритма × 5 сценариев.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="tcp-brutal и skyline_cc относительно bbr на игровом, видео-, веб- и массовом трафике" src="docs/images/single-ended-ataglance-light.svg">
</picture>

| Сценарий | Победитель | Перевес |
|---|---|---|
| **Видео** (фрагменты по 6 МБ @ 48 Мбит/с) | **tcp-brutal** | старт 7/8 ротаций, p95 фрагмента 6/8 |
| **Игра** (ping-pong, 60 сообщ./с) | **skyline_cc** | джиттер 5/8, зависания >150 мс 4/8 |
| **Массовая передача, 4 потока** | skyline_cc и tcp-brutal, оба впереди bbr | медиана 117 / 115 против 77 Мбит/с |
| **Массовая передача, 1 поток** | bbr и skyline_cc вровень; tcp-brutal последний | медиана 59 / 53 / 46 Мбит/с |
| **Загрузка веб-страницы** | устойчивого победителя нет | разброс между ротациями больше разницы |
| **Повторные передачи сервера**, доля всех отправленных сегментов | у bbr меньше всего | bbr 9,1%, skyline_cc 10,4%, tcp-brutal 10,9% |

Это полевое измерение на одном маршруте, а не обещание для вашего канала: skyline_cc работал с коэффициентами по умолчанию версии 0.1.0, теперь это пресет "high random loss". Методика, условия и полные результаты: [docs/01-features.md](docs/01-features.md#measured-performance-in-detail).

## Лицензия

GPL-2.0, см. [LICENSE](LICENSE) и [NOTICE](NOTICE); Copyright (c) 2026 **CYBERVERSE LLC**. "Skyline Speeder" и "CYBERVERSE" — товарные знаки CYBERVERSE LLC. Перед тем как вносить вклад, прочитайте [CONTRIBUTING.md](CONTRIBUTING.md).

---

<div align="center">

Эксклюзивный спонсор — **[Skyline Connect](https://www.skylineconnect.io)**

</div>
