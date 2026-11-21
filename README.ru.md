<div align="center">

# Skyline Speeder

**Ускорение TCP на стороне отправителя для каналов с большими потерями и высокой задержкой, на базе eBPF**

*Ставится только на отправляющий сервер. Клиенты остаются на штатном TCP — никаких программ, патчей и настроек.*

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

## Что это такое

Skyline Speeder — односторонний ускоритель TCP. Он работает в виде eBPF-программ на сервере, который отдаёт данные, и рассчитан на каналы с сильными потерями пакетов и большим RTT, где стандартный TCP практически перестаёт работать. На принимающей стороне ничего менять не нужно.

Вместо алгоритма управления перегрузкой, который стоит на сервере, начинает работать `skyline_cc`. В его основе лежит одно допущение: **потеря пакета сама по себе ничего не говорит о перегрузке.**

- **Сначала измерить, потом отправлять:** окно подстраивается под произведение измеренной пропускной способности на базовый RTT, а потеря пакета его никогда не уменьшает.
- **Компенсация и пейсинг:** скорость поднимается с поправкой на измеренную долю потерь (но не выше потолка), а `fq` выдаёт пакеты в сеть равномерно.
- **Единственный тормоз:** замедлить отправку могут только растущая задержка в очередях и метки ECN.
- **Короткие ответы — в первую очередь:** рукопожатие и первые 64 КиБ каждого соединения отправляются дважды с разницей в 10 мс, поэтому один потерянный пакет обходится в миллисекунды, а не в ожидание таймера повторной передачи.

> [!IMPORTANT]
> Skyline Speeder помогает только там, где потери не зависят от очередей. Если UDP-поток с фиксированной скоростью проходит без потерь, а у TCP при этом масса ретрансмитов, значит, потери вызваны переполнением очереди в перегруженном узком месте — и вам нужен другой инструмент.

## Требования

- **Linux 6.12 LTS или новее** с `/sys/kernel/btf/vmlinux`. Ветки 6.1 и 6.6 LTS, а также штатные ядра Debian 12 и Ubuntu 22.04/24.04 слишком старые.
- Физический сервер или виртуальная машина KVM, но не контейнер (LXC, Proxmox CT).
- Debian, Ubuntu, Fedora, семейство RHEL (Rocky Linux, AlmaLinux, CentOS Stream) или Alpine.

## Быстрый старт

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

В Alpine — от имени root:

```sh
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

Скрипт ставит последний опубликованный релиз, не устанавливая на хост компилятор, прогоняет каждый BPF-объект через верификатор ядра, назначает `skyline_cc` алгоритмом управления перегрузкой для всех новых TCP-соединений и сохраняет эту настройку после перезагрузки. Никаких прокси он не ставит и портов не открывает. Затем проверьте:

```bash
ssctl status    # перешёл ли хост под управление skyline_cc? ACCELERATING — значит, да
ssctl flows     # какие соединения ускоряются и что по ним решил алгоритм
```

Удаление (после него хост переходит на bbr + fq):

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --uninstall
```

## Продвинутое использование

**Установка из клонированного репозитория** — если хотите прочитать скрипты перед запуском:

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh --check      # только предварительная проверка, ничего не меняет
sudo ./install.sh              # опубликованный релиз (сборка только там, где он не запускается)
sudo ./install.sh --source     # собрать из исходников этого клона
sudo ./install.sh --release v0.6.0   # только этот опубликованный релиз; если не выйдет — остановиться
sudo ./install.sh --uninstall --restore-pre-install   # удалить и вернуть алгоритм управления перегрузкой и qdisc, стоявшие до установки
```

Для обновления запустите установщик ещё раз. Он до 60 секунд даёт активным соединениям завершиться (drain) и снова подключает `skyline_cc`.

**Повседневные команды:**

```bash
sudo ssctl drain                                # мягкое отключение: новые соединения получают fallback_cc
sudo ssctl enable                               # подключить снова
sudo ssctl enable --all-off                     # skyline_cc со всеми выключенными модулями — точка отсчёта для сравнений
sudo ssctl set-redundancy --disable             # не отправлять первую порцию данных дважды — там, где исходящий трафик оплачивается по объёму
sudo ssctl set-module-config --max-pacing-mbps 500   # перезаписывает всё целиком: неуказанные флаги возвращаются к значениям по умолчанию
```

Значения по умолчанию уже подобраны, и в большинстве случаев менять ничего не нужно. Изменения через `ssctl set-*` действуют до перезапуска демона; чтобы они сохранились, пропишите их в `/etc/skyline-speeder/speeder.toml`.

**Документация** (на английском):

| Документ | Содержание |
|---|---|
| [docs/01-features.md](docs/01-features.md) | Что делает установщик, принцип работы, все функции с их значениями по умолчанию, издержками и переключателями, построчный разбор `ssctl status` и `ssctl flows`, сборка из исходников |
| [docs/usage.md](docs/usage.md) | Все переключатели и параметры с допустимыми диапазонами, четыре пресета, устранение неполадок |
| [DEPLOY.md](DEPLOY.md) | Руководство по развёртыванию и эксплуатации: установка, проверка, обновление, удаление |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | Команды `ssctl`, протокол обмена, поля конфигурации, коды событий |
| [docs/03-design.md](docs/03-design.md) | Архитектура и устройство каждого модуля |
| [docs/04-performance-report.md](docs/04-performance-report.md) | Сравнение с bbr на контролируемом стенде из двух ВМ |

## Замеры производительности

Сравнивались bbr, [tcp-brutal](https://github.com/HyNetworks/tcp-brutal) и skyline_cc — каждый установлен только на сервере — на одном реальном международном маршруте: KVM VPS в Сингапуре отдаёт данные на Alibaba Cloud ECS в Южном Китае (200 Мбит/с) со штатным TCP. RTT 70–73 мс, потери 10–30%, вечерний час пик; 8 циклов × 3 алгоритма × 5 сценариев.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="tcp-brutal и skyline_cc в сравнении с bbr на игровом трафике, видео, веб-страницах и передаче больших объёмов" src="docs/images/single-ended-ataglance-light.svg">
</picture>

| Сценарий | Победитель | Перевес |
|---|---|---|
| **Видео** (фрагменты по 6 МБ при 48 Мбит/с) | **tcp-brutal** | время старта: 7/8 циклов, p95 фрагмента: 6/8 |
| **Игра** (пинг-понг, 60 сообщ./с) | **skyline_cc** | джиттер: 5/8, подвисания >150 мс: 4/8 |
| **Передача больших объёмов, 4 потока** | skyline_cc и tcp-brutal, оба опережают bbr | медиана 117 / 115 против 77 Мбит/с |
| **Передача больших объёмов, 1 поток** | bbr и skyline_cc наравне; tcp-brutal — последний | медиана 59 / 53 / 46 Мбит/с |
| **Загрузка веб-страницы** | стабильного победителя нет | разброс между циклами больше самой разницы |
| **Ретрансмиты сервера**, доля от всех отправленных сегментов | меньше всего у bbr | bbr 9,1%, skyline_cc 10,4%, tcp-brutal 10,9% |

Это полевой замер на одном маршруте, а не обещание для вашего канала: skyline_cc работал с настройками по умолчанию из версии 0.1.0, которые теперь стали пресетом "high random loss". Методика, условия и полные результаты — в [docs/01-features.md](docs/01-features.md#measured-performance-in-detail).

## Лицензия

GPL-2.0, см. [LICENSE](LICENSE) и [NOTICE](NOTICE); Copyright (c) 2026 **CYBERVERSE LLC**. «Skyline Speeder» и «CYBERVERSE» — товарные знаки CYBERVERSE LLC. Прежде чем участвовать в разработке, прочитайте [CONTRIBUTING.md](CONTRIBUTING.md).

---

<div align="center">

Эксклюзивный спонсор — **[Skyline Connect](https://www.skylineconnect.io)**

</div>
