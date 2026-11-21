<div align="center">

# Skyline Speeder

**Accélération TCP côté émetteur, reposant sur eBPF, pour les liens à fortes pertes et à forte latence**

*À déployer sur le seul serveur émetteur. Les clients conservent leur TCP standard : aucun logiciel, aucun correctif, aucune configuration.*

[![License](https://img.shields.io/badge/license-GPL--2.0-blue.svg)](LICENSE)
[![Kernel](https://img.shields.io/badge/kernel-6.12%20LTS%2B-orange.svg)](#prérequis)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu%20%7C%20Fedora%20%7C%20RHEL%20%7C%20Alpine-red.svg)](install.sh)
[![eBPF](https://img.shields.io/badge/eBPF-CO--RE%20struct__ops-green.svg)](bpf/)
[![Rust](https://img.shields.io/badge/rust-1.82%2B-black.svg)](rust-toolchain.toml)

[English](README.md) · [简体中文](README.zh.md) · [繁體中文](README.zh-TW.md) · [日本語](README.ja.md) · [한국어](README.ko.md) · [Русский](README.ru.md) · **Français**

---

### Sponsor exclusif : **[Skyline Connect](https://www.skylineconnect.io)**

---

</div>

## Présentation

Skyline Speeder est un accélérateur TCP qui s'installe d'un seul côté de la connexion. Il tourne sous forme de programmes eBPF sur le serveur qui émet les données et s'adresse aux liens où les pertes de paquets sont fortes et le temps d'aller-retour élevé, là où TCP standard s'effondre. Le côté récepteur reste intact.

Il remplace le contrôle de congestion du serveur par `skyline_cc`, qui repose sur un seul postulat : **à elle seule, une perte ne renseigne en rien sur la congestion.**

- **Mesurer, puis envoyer :** la fenêtre suit le produit bande passante mesurée × RTT de base ; un paquet perdu ne la réduit jamais.
- **Compenser et lisser :** le débit est majoré en fonction du taux de pertes mesuré, dans la limite d'un plafond, et `fq` espace régulièrement les paquets émis.
- **Un seul frein :** l'émission ne ralentit qu'en cas de hausse du délai de file d'attente ou de marquage ECN.
- **Priorité aux réponses courtes :** la poignée de main et les 64 premiers Kio de chaque connexion sont envoyés deux fois, à 10 ms d'intervalle ; un paquet perdu ne coûte alors que quelques millisecondes, au lieu d'une attente du temporisateur de retransmission.

> [!IMPORTANT]
> Skyline Speeder n'est utile que si les pertes ne dépendent pas des files d'attente. Si un flux UDP à débit fixe ne subit aucune perte alors que TCP retransmet abondamment, vos pertes viennent du débordement de la file d'un goulot d'étranglement saturé : ce n'est pas l'outil qu'il vous faut.

## Prérequis

- **Linux 6.12 LTS ou ultérieur**, avec `/sys/kernel/btf/vmlinux`. Les branches LTS 6.1 et 6.6, ainsi que les noyaux fournis d'origine avec Debian 12 et Ubuntu 22.04/24.04, sont trop anciens.
- Une machine physique ou une VM KVM, pas un conteneur (LXC, CT Proxmox).
- Debian, Ubuntu, Fedora, la famille RHEL (Rocky Linux, AlmaLinux, CentOS Stream) ou Alpine.

## Démarrage rapide

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

Sur Alpine, en root :

```sh
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

Le script installe la dernière version publiée sans déposer de compilateur sur l'hôte, soumet chaque objet BPF au vérificateur du noyau, puis fait de `skyline_cc` le contrôle de congestion de toute nouvelle connexion TCP, de façon persistante après redémarrage. Il n'installe aucun proxy et n'ouvre aucun port. Vérifiez ensuite :

```bash
ssctl status    # l'hôte est-il passé sous skyline_cc ? ACCELERATING signifie oui
ssctl flows     # connexions accélérées et décisions de l'algorithme
```

Pour désinstaller (l'hôte repasse alors sur bbr + fq) :

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --uninstall
```

## Utilisation avancée

**Installer depuis un clone du dépôt**, pour relire les scripts avant de les exécuter :

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh --check      # vérifications préalables uniquement, aucune modification
sudo ./install.sh              # la version publiée (compilée sur place seulement si elle ne peut pas tourner)
sudo ./install.sh --source     # compiler cette arborescence depuis les sources
sudo ./install.sh --release v0.6.0   # exactement cette version publiée, sinon abandon
sudo ./install.sh --uninstall --restore-pre-install   # désinstaller et restaurer le contrôle de congestion et la qdisc d'avant l'installation
```

Pour mettre à jour, relancez l'installateur : il draine les flux en cours pendant 60 secondes au maximum, puis rattache `skyline_cc`.

**Au quotidien :**

```bash
sudo ssctl drain                                # détachement en douceur : les nouvelles connexions passent sur fallback_cc
sudo ssctl enable                               # rattacher
sudo ssctl enable --all-off                     # skyline_cc, tous modules désactivés : base de référence pour les comparaisons
sudo ssctl set-redundancy --disable             # ne plus doubler les premières salves, si le trafic sortant est facturé au volume
sudo ssctl set-module-config --max-pacing-mbps 500   # réglage absolu : toute option omise reprend sa valeur par défaut
```

Les valeurs livrées par défaut sont déjà ajustées : dans la plupart des déploiements, il n'y a rien à modifier. Un changement fait avec `ssctl set-*` vaut jusqu'au prochain redémarrage du démon ; pour le rendre permanent, reportez-le dans `/etc/skyline-speeder/speeder.toml`.

**Documentation** (en anglais) :

| Document | Contenu |
|---|---|
| [docs/01-features.md](docs/01-features.md) | Ce que fait l'installateur, le principe de fonctionnement, chaque fonctionnalité avec sa valeur par défaut, son coût et l'option qui la pilote, chaque ligne de `ssctl status` et de `ssctl flows`, la compilation depuis les sources |
| [docs/usage.md](docs/usage.md) | Toutes les options et tous les paramètres avec leurs plages de valeurs, quatre préréglages, le dépannage |
| [DEPLOY.md](DEPLOY.md) | Le manuel de déploiement et d'exploitation : installation, vérification, mise à jour, désinstallation |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | Les commandes `ssctl`, le protocole de communication, les champs de configuration, les codes d'événement |
| [docs/03-design.md](docs/03-design.md) | L'architecture et le fonctionnement de chaque module |
| [docs/04-performance-report.md](docs/04-performance-report.md) | Le banc d'essai contrôlé à deux VM, face à bbr |

## Performances mesurées

bbr, [tcp-brutal](https://github.com/HyNetworks/tcp-brutal) et skyline_cc, chacun déployé côté serveur uniquement, sur un même chemin transfrontalier réel : un VPS KVM à Singapour émettant vers une instance ECS d'Alibaba Cloud en Chine du Sud (200 Mbit/s), restée en TCP standard. RTT de 70 à 73 ms, de 10 à 30 % de pertes, à l'heure de pointe du soir ; 8 rotations × 3 algorithmes × 5 scénarios.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="tcp-brutal et skyline_cc comparés à une référence bbr, pour du trafic de jeu, de vidéo, web et de transfert de masse" src="docs/images/single-ended-ataglance-light.svg">
</picture>

| Scénario | Gagnant | Écart |
|---|---|---|
| **Vidéo** (segments de 6 Mo à 48 Mbit/s) | **tcp-brutal** | démarrage : 7/8 rotations ; p95 par segment : 6/8 |
| **Jeu** (ping-pong, 60 msg/s) | **skyline_cc** | gigue : 5/8 ; blocages de plus de 150 ms : 4/8 |
| **Transfert de masse, 4 flux** | skyline_cc et tcp-brutal, tous deux devant bbr | médianes : 117 / 115 contre 77 Mbit/s |
| **Transfert de masse, 1 flux** | bbr et skyline_cc à égalité ; tcp-brutal en dernier | médianes : 59 / 53 / 46 Mbit/s |
| **Chargement de page web** | aucun gagnant constant | la dispersion d'une rotation à l'autre dépasse la différence |
| **Retransmissions côté serveur**, en proportion des segments envoyés | bbr, le plus faible | bbr 9,1 %, skyline_cc 10,4 %, tcp-brutal 10,9 % |

Il s'agit d'une mesure de terrain sur un seul chemin, qui ne préjuge pas de ce que vous obtiendrez sur votre lien : skyline_cc utilisait les valeurs par défaut de la 0.1.0, qui correspondent aujourd'hui au préréglage « high random loss ». Méthode, conditions et résultats complets : [docs/01-features.md](docs/01-features.md#measured-performance-in-detail).

## Licence

Sous licence GPL-2.0 : voir [LICENSE](LICENSE) et [NOTICE](NOTICE) ; copyright (c) 2026 **CYBERVERSE LLC**. « Skyline Speeder » et « CYBERVERSE » sont des marques de CYBERVERSE LLC. Avant toute contribution, lisez [CONTRIBUTING.md](CONTRIBUTING.md).

---

<div align="center">

Sponsor exclusif : **[Skyline Connect](https://www.skylineconnect.io)**

</div>
