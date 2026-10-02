# btrfsmgr — Gestionnaire TUI BTRFS

> ## ⚠️ Avertissement / Disclaimer
>
> **Ce projet n'est PAS officiel. / This project is UNOFFICIAL.**
> Il n'est fourni ni maintenu par la distribution **Solus** ni par
> BTRFS. Il est un utilitaire tiers indépendant.
>
> **Code généré à 100 % par IA / Code written 100% by AI (LLM).**
> L'intégralité du code a été générée par une intelligence artificielle.
>
> **Non entièrement testé / Not fully tested.**
> Toutes les fonctionnalités n'ont **pas encore** été testées.
> Certaines opérations (notamment restauration et manipulation du boot)
> peuvent être destructives. **Utilisez sous votre responsabilité et à
> vos risques** ; faites des sauvegardes avant toute action.

Gestionnaire d'instantanés BTRFS pour Linux : TUI sans dépendance
(standard Python uniquement), timers systemd avec rétention,
restauration, et entrées de boot GRUB / systemd-boot.

> **Unofficial / Not affiliated with Solus or BTRFS. Code written 100% by
> AI. Not all features have been tested — use at your own risk.**

## Installation

```sh
# 1. Cloner le dépôt
git clone https://github.com/Julien684/btrfsmgr.git
cd btrfsmgr

# 2. Installer
sudo ./install.sh

# 3. Utiliser
btrfsmgr                 # gère le sous-volume /
btrfsmgr /mnt/backup     # gère un autre sous-volume BTRFS
```

Dépendances : `btrfs-progs`, `python3 ≥ 3.8`, `rsync` (restauration),
`systemd` (timers).

## Fonctionnalités

| Menu | Action |
|------|--------|
| 1 | Créer un instantané (read-only) dans `@snapshots/` |
| 2 | Listez les instantanés + autres sous-volumes |
| 3 | Restaurer : `rsync` vers un sous-volume, ou contenu + `btrfs subvolume set-default` pour booter dessus ensuite |
| 4 | Détruire un instantané (un seul, plusieurs `1,3,5`, ou `tous`) |
| 5 | Programmer des snapshots automatiques (timer systemd) avec planning `OnCalendar` (ex. `daily`, `weekly`, `Mon *-*-* 03:00:00`) et rétention (conserver les N plus récents, supprimer les plus anciens) |
| 6 | Supprimer une automatisation (timer systemd) |
| 7 | Rétention manuelle : supprimer tous les snapshots sauf les N plus récents |
| 8 | Infos : chargeur de boot détecté, timers actifs, espace disque |
| 9 | Menu systemd-boot : afficher/masquer le menu au démarrage (timeout) |
| 10 | Quitter |

## Timers systemd

`btrfsmgr` crée (menu 5) :

* `/etc/systemd/system/btrfsmgr-snap-<root>-<snapdir>-<tag>.service`
* `/etc/systemd/system/btrfsmgr-snap-<root>-<snapdir>-<tag>.timer`

Le service exécute `btrfsmgr auto <root> --snapdir @snapshots
--name <tag>-%Y%m%d-%H%M%S --keep N` : création du snapshot puis
suppression automatique des plus anciens au-delà de N (rétention).

Suivi : `systemctl list-timers | grep btrfsmgr`,
`journalctl -u btrfsmgr-snap-*`.

Suppression d'un plan : option 6 du TUI — liste les timers `btrfsmgr`,
choix, puis `systemctl disable --now` + suppression des fichiers dans
`/etc/systemd/system`.

## Boot sur un snapshot

* **GRUB** : une entrée `menuentry` est générée dans
  `/etc/grub.d/40_btrfsmgr_snapshots` (puis `grub-mkconfig` régénère
  `grub.cfg` si disponible).
* **systemd-boot** : une entrée UEFI est écrite dans
  `/boot/loader/entries/btrfsmgr-<nom>.conf`.

* **Solus** : la partition EFI (`/boot`) n'étant pas montée par défaut,
  `btrfsmgr` la monte automatiquement via `clr-boot-manager mount-boot`
  (fallback `mount /boot`) avant de créer l'entrée de boot.

L'entrée boot le même kernel que le système actuel. Ses options sont
**clonées du système en cours** (`/proc/cmdline`) — `root=PARTUUID=…`,
`rootflags=subvol=…`, `rd.vconsole*`, `quiet splash rw`, `plymouth`… — et
seul le sous-volume est remplacé, donc elle démarre exactement comme le
système. Exemple généré sous Solus :

```
options: root=PARTUUID=b11754f0-… rootflags=subvol=@snapshots/<nom> \
         rd.vconsole.keymap=fr rd.vconsole.font=ter-v32b quiet splash rw \
         plymouth.use-simpledrm
```

Redémarrez et choisissez l'entrée dans le chargeur.

## Synchronisation au lancement

À chaque démarrage du TUI, `btrfsmgr` synchronise les entrées
**systemd-boot** qu'il a générées avec les snapshots réellement présents
dans `<root>/@snapshots` :

* **ajoute** une entrée (fichier `.conf`) pour chaque snapshot orphelin ;
* **supprime** le fichier `.conf` dont le snapshot n'existe plus.

Seuls les fichiers `btrfsmgr-*.conf` du dossier `/boot/loader/entries`
sont concernés — les entrées d'autres outils (ex. `Solus-current-*.conf`)
restent intactes. Si le chargeur n'est pas systemd-boot (GRUB), cette
étape est un no-op silencieux.

## Mode restauration « set-default »

Le mode 2 du menu 3 :

1. `rsync -aHAX --delete <snapshot>/ → <sous-volume courant>/`
2. `btrfs subvolume set-default <gen> /`

Le prochain boot chargera alors le sous-volume restauré.

## Utilitaire pour les scripts

```sh
btrfsmgr auto / --snapdir @snapshots --name test --keep 3
```

Crée `//@snapshots/test` puis ne garde que les 3 plus récents.
