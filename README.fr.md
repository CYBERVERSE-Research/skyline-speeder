<div align="center">

# Skyline Speeder

**Accélération TCP côté émetteur pour les liens à fortes pertes et forte latence, en eBPF**

*À installer uniquement sur le serveur qui envoie. Les clients restent en TCP standard, sans logiciel, sans correctif, sans configuration.*

[![License](https://img.shields.io/badge/license-GPL--2.0-blue.svg)](LICENSE)
[![Kernel](https://img.shields.io/badge/kernel-6.12%20LTS%2B-orange.svg)](#prérequis)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu%20%7C%20Fedora%20%7C%20RHEL%20%7C%20Alpine-red.svg)](install.sh)
[![eBPF](https://img.shields.io/badge/eBPF-CO--RE%20struct__ops-green.svg)](bpf/)
[![Rust](https://img.shields.io/badge/rust-1.82%2B-black.svg)](rust-toolchain.toml)

[English](README.md) · [简体中文](README.zh.md) · [繁體中文](README.zh-TW.md) · [日本語](README.ja.md) · [한국어](README.ko.md) · [Русский](README.ru.md) · **Français**

---

### Sponsor exclusif : **[Skyline Connect](https://www.skylineconnect.io)**

---

</div>

## Présentation

Skyline Speeder est un logiciel d'accélération TCP unilatéral. Il fonctionne sous forme de programmes eBPF sur le serveur qui envoie les données, et vise les liens à fortes pertes de paquets et à long temps d'aller-retour, là où le TCP standard s'effondre. Le côté récepteur n'est pas modifié.

Il remplace le contrôle de congestion du serveur par `skyline_cc`, fondé sur une seule hypothèse : **une perte, à elle seule, ne dit rien de la congestion.**

- **Mesurer, puis envoyer :** la fenêtre suit la bande passante mesurée × le RTT de base, et une perte ne la réduit jamais.
- **Compenser et lisser :** le débit est relevé selon le taux de pertes mesuré, jusqu'à un plafond, et part à intervalles réguliers par `fq`.
- **Un seul frein :** seuls la hausse du délai de file d'attente et les marques ECN le ralentissent.
- **Les réponses courtes d'abord :** la poignée de main et les 64 Kio initiaux de chaque connexion partent deux fois, à 10 ms d'écart, si bien qu'une perte coûte quelques millisecondes et non un temporisateur de retransmission.

> [!IMPORTANT]
> Cela n'aide que si les pertes sont indépendantes des files d'attente. Si un flux UDP à débit fixe ne perd rien alors que TCP retransmet massivement, vos pertes viennent du débordement d'une file sur un goulot congestionné, et ce n'est pas le bon outil.

## Prérequis

- **Linux 6.12 LTS ou plus récent**, avec `/sys/kernel/btf/vmlinux`. Les branches LTS 6.1 et 6.6 et les noyaux d'origine de Debian 12 et d'Ubuntu 22.04/24.04 sont trop anciens.
- Une machine physique ou une machine virtuelle KVM, pas un conteneur (LXC, CT Proxmox).
- Debian, Ubuntu, Fedora, la famille RHEL (Rocky Linux, AlmaLinux, CentOS Stream) ou Alpine.

## Démarrage rapide

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

Sur Alpine, en root :

```sh
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

Cela installe la dernière version publiée, sans compilateur sur l'hôte, fait vérifier chaque objet BPF par le vérificateur du noyau, puis fait de `skyline_cc` le contrôle de congestion de toute nouvelle connexion TCP, y compris après un redémarrage. Aucun proxy n'est installé, aucun port n'est ouvert. Ensuite, vérifiez :

```bash
ssctl status    # l'hôte est-il pris en charge ? ACCELERATING veut dire oui
ssctl flows     # quelles connexions sont accélérées, et ce que l'algorithme a décidé
```

Pour le désinstaller, ce qui remet l'hôte sur bbr + fq :

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --uninstall
```

## Utilisation avancée

**Installer depuis un clone**, pour lire les scripts avant de les exécuter :

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh --check      # vérifications préalables seulement, ne change rien
sudo ./install.sh              # la version publiée (compile seulement là où elle ne peut pas tourner)
sudo ./install.sh --source     # compiler cette arborescence
sudo ./install.sh --release v0.6.0   # cette version publiée, sinon arrêt
sudo ./install.sh --uninstall --restore-pre-install   # désinstaller et remettre le contrôle de congestion et la qdisc d'avant
```

Pour mettre à jour, relancez l'installateur. Il vide les connexions en cours (jusqu'à 60 secondes) et rattache `skyline_cc`.

**Au quotidien :**

```bash
sudo ssctl drain                                # détachement en douceur : les nouvelles connexions passent à fallback_cc
sudo ssctl enable                               # rattacher
sudo ssctl enable --all-off                     # skyline_cc avec tous ses modules coupés : une référence de comparaison
sudo ssctl set-redundancy --disable             # ne plus envoyer deux fois le début des connexions (trafic facturé au volume)
sudo ssctl set-module-config --max-pacing-mbps 500   # écrasement complet : tout drapeau omis revient à sa valeur par défaut
```

Les valeurs livrées sont réglées, et la plupart des déploiements n'ont rien à changer. Une modification par `ssctl set-*` dure jusqu'au redémarrage du démon ; pour la garder, inscrivez-la dans `/etc/skyline-speeder/speeder.toml`.

**Documentation** (en anglais) :

| Document | Contenu |
|---|---|
| [docs/01-features.md](docs/01-features.md) | Ce que fait l'installateur, le fonctionnement, chaque fonction avec sa valeur par défaut, son coût et son réglage, chaque ligne de `ssctl status` et `ssctl flows`, la compilation depuis les sources |
| [docs/usage.md](docs/usage.md) | Chaque option et paramètre avec sa plage, quatre préréglages, dépannage |
| [DEPLOY.md](DEPLOY.md) | Le manuel de déploiement et d'exploitation : installation, vérification, mise à jour, retrait |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | Commandes `ssctl`, protocole, champs de configuration, codes d'événement |
| [docs/03-design.md](docs/03-design.md) | L'architecture et le fonctionnement de chaque module |
| [docs/04-performance-report.md](docs/04-performance-report.md) | Le banc de test contrôlé à deux VM, face à bbr |

## Performances mesurées

bbr, [tcp-brutal](https://github.com/HyNetworks/tcp-brutal) et skyline_cc, chacun côté serveur seulement, sur un même lien international réel : un VPS KVM à Singapour qui envoie vers un ECS Alibaba Cloud en Chine du Sud (Guangdong, 200 Mbit/s) resté en TCP standard. RTT 70-73 ms, 10-30 % de pertes, à l'heure de pointe du soir ; 8 rotations × 3 algorithmes × 5 scénarios.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="tcp-brutal et skyline_cc mesurés par rapport à bbr sur du trafic de jeu, vidéo, web et en masse" src="docs/images/single-ended-ataglance-light.svg">
</picture>

| Scénario | Gagnant | Écart |
|---|---|---|
| **Vidéo** (segments de 6 Mo @ 48 Mbit/s) | **tcp-brutal** | démarrage 7/8 rotations, p95 des segments 6/8 |
| **Jeu** (ping-pong, 60 msg/s) | **skyline_cc** | gigue 5/8, blocages >150 ms 4/8 |
| **Transfert en masse, 4 flux** | skyline_cc et tcp-brutal, tous deux devant bbr | médiane 117 / 115 contre 77 Mbit/s |
| **Transfert en masse, 1 flux** | bbr et skyline_cc à égalité ; tcp-brutal dernier | médiane 59 / 53 / 46 Mbit/s |
| **Chargement de page web** | pas de gagnant stable | la variation d'une rotation à l'autre dépasse l'écart |
| **Retransmissions du serveur**, part de tous les segments envoyés | bbr le plus bas | bbr 9,1 %, skyline_cc 10,4 %, tcp-brutal 10,9 % |

Une mesure de terrain sur un seul lien, pas une promesse pour le vôtre : skyline_cc tournait avec les coefficients par défaut de la 0.1.0, aujourd'hui le préréglage "high random loss". Méthode, conditions et résultats complets : [docs/01-features.md](docs/01-features.md#measured-performance-in-detail).

## Licence

GPL-2.0, voir [LICENSE](LICENSE) et [NOTICE](NOTICE) ; Copyright (c) 2026 **CYBERVERSE LLC**. « Skyline Speeder » et « CYBERVERSE » sont des marques de CYBERVERSE LLC. Avant de contribuer, lisez [CONTRIBUTING.md](CONTRIBUTING.md).

---

<div align="center">

Sponsor exclusif : **[Skyline Connect](https://www.skylineconnect.io)**

</div>
