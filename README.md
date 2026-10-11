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

### Installation via paquet eopkg (Solus)

```sh
# Téléchargement et installation direct du paquet depuis GitHub :
sudo eopkg install https://github.com/Julien684/btrfsmgr/raw/main/btrfsmgr-1.0.12-1-1-x86_64.eopkg
```

> **Pourquoi le *release* dans le nom de fichier** : eopkg met en cache les
> `.eopkg` **par nom de fichier**, pas par checksum. Après un rebuild avec le
> même nom (ex. `btrfsmgr-1.0.5-1-*`), `eopkg install` refuse de re-développer
> la nouvelle version. Chaque republish **doit** incrémenter le *release*
> (dans `build_eopkg.py` → `RELEASE`) pour produire un nouveau nom de
> fichier, ce qui force eopkg à le traiter comme un paquet neuf — pas besoin
> de `sudo eopkg delete-cache` sur la cible.
### Désinstallation via paquet eopkg (Solus)
```sh
# Désinstallation
sudo eopkg remove btrfsmgr
```

> **Testé et fonctionnel** : l'installation et la désinstallation via eopkg
> ont été vérifiées sur Solus 4.9 (eopkg 5.0.0).

### Installation via le script (toute distribution Linux)

```sh
# 1. Cloner le dépôt
git clone https://github.com/Julien684/btrfsmgr.git
cd btrfsmgr

# 2. Installer
sudo ./install.sh
```
### Désinstallation via le script (toute distribution Linux)

```sh
sudo btrfsmgr uninstall
```
# Utilisation
```sh
sudo btrfsmgr                 # gère le sous-volume /
sudo btrfsmgr /mnt/backup     # gère un autre sous-volume BTRFS
```

Dépendances : `btrfs-progs`, `python3 ≥ 3.8`, `systemd` (timers).

La restauration ne nécessite **ni rsync ni copie** : c'est un swap CoW
de sous-volumes (deux `mv`).

## Version et mise à jour

`btrfsmgr --version` (ou `-V`) affiche la version installée.

Le projet est versionné par *tags git* (`v1.0.0`, `v1.1.0`, …) sur GitHub, et
la version locale est constante `VERSION` dans `btrfsmgr.py`.

`btrfsmgr update` (menu/CLI, sans argument) vérifie automatiquement la
dernière version sur GitHub :

```sh
sudo btrfsmgr update
```

* interroge les tags du dépôt GitHub (API GitHub, sinon `git ls-remote`) ;
* compare la version distante à la version locale ;
* si une version **supérieure** est disponible, il la propose et, sur
  confirmation, clône cette version (clone temporaire) puis relance
  `install.sh` — le dépôt de travail local n'est pas modifié ;
* sinon il affiche « Vous êtes déjà à jour » (réinstallation locale
  possible) ;
* hors ligne ou dépôt sans tag, il relance l'`install.sh` local
  (comportement historique) : `btrfsmgr update /chemin/vers/le/depot`.

## Fonctionnalités

| Menu | Action |
|------|--------|
| 1 | Créer un instantané (read-only) dans `@snapshots/` |
| 2 | Listez les instantanés + autres sous-volumes |
| 3 | Restaurer : swap CoW (deux `mv`) — le snapshot devient `@`, l'ancien `@` est conservé dans `snapshots/@_old-<date-heure>` (tout est CoW, rien n'est copié) |
| 4 | Supprimer un instantané (un seul, plusieurs `1,3,5`, ou `tous`) |
| 5 | Programmer des snapshots automatiques (timer systemd) avec planning `OnCalendar` (ex. `daily`, `weekly`, `Mon *-*-* 03:00:00`) et rétention (conserver les N plus récents, supprimer les plus anciens) |
| 6 | Supprimer une automatisation (timer systemd) |
| 7 | Rétention manuelle : supprimer tous les snapshots sauf les N plus récents |
| 8 | Infos : chargeur de boot détecté, timers actifs, espace disque |
| 9 | Menu systemd-boot : afficher/masquer le menu au démarrage (timeout) |
| 10 | Quitter |

## État des tests

Fonctionnalités **testées et fonctionnelles** :

* Création d'un instantané
* Lister les instantanés
* Supprimer un instantané
* Supprimer une automatisation (timer systemd)
* Infos système (chargeur de boot, timers…)
* Menu systemd-boot : afficher/masquer au démarrage

Fonctionnalité **en cours de test** :

* Programmer des instantanés automatiques (systemd) avec toutes les
  possibilités et la suppression automatique (rétention).

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

## Mode restauration « swap mv »

Le menu 3 :

1. `mv  @ → snapshots/@_old-<date-heure>`   — l'ancien sous-volume courant
   est conservé comme snapshot daté.
2. `mv  snapshots/<snapshot> → @`   — le snapshot choisi prend la place
   de `@` et devient la racine bootable (flag `ro` levé).
3. `btrfs subvolume set-default @ /`   — le default subvolume est repointé
   sur `@` (le sous-volume restauré).

Tout est CoW : deux renommages de sous-volumes, aucune copie de données.
Le montage vivant de `/` reste valable (un montage suit le sous-volume,
pas son nom). Redémarrez ensuite pour booter sur la version restaurée.

## Utilitaire pour les scripts

```sh
btrfsmgr auto / --snapdir @snapshots --name test --keep 3
```

Crée `//@snapshots/test` puis ne garde que les 3 plus récents.

---

# btrfsmgr — BTRFS Snapshot Manager (TUI)

> ## ⚠️ Warning / Disclaimer
>
> **Ce projet n'est PAS officiel. / This project is UNOFFICIAL.**
> It is neither provided nor maintained by the **Solus** distribution
> nor by BTRFS. It is an independent third-party utility.
>
> **Code généré à 100 % par IA / Code written 100% by AI (LLM).**
> The entire codebase was generated by an artificial intelligence.
>
> **Non entièrement testé / Not fully tested.**
> Not all features have **yet** been tested. Some operations (notably
> restoration and boot manipulation) can be destructive. **Use at your
> own risk**; back up your data before any action.

BTRFS snapshot manager for Linux: dependency-free TUI (standard Python
only), systemd timers with retention, restoration, and GRUB /
systemd-boot boot entries.

> **Unofficial / Not affiliated with Solus or BTRFS. Code written 100% by
> AI. Not all features have been tested — use at your own risk.**

## Installation

### Installation via eopkg package (Solus)

```sh
# Direct download and installation of the package from GitHub:
sudo eopkg install https://github.com/Julien684/btrfsmgr/raw/main/btrfsmgr-1.0.12-1-1-x86_64.eopkg
```

> **Why the *release* in the filename**: eopkg caches `.eopkg` files
> **by filename**, not by checksum. After a rebuild with the same name
> (e.g. `btrfsmgr-1.0.5-1-*`), `eopkg install` refuses to re-deploy the
> new version. Every republish **must** bump the *release* (in
> `build_eopkg.py` → `RELEASE`) to produce a new filename, which makes
> eopkg treat it as a fresh package — no need for
> `sudo eopkg delete-cache` on the target.
### Uninstallation via eopkg package (Solus)
```sh
# Uninstall
sudo eopkg remove btrfsmgr
```

> **Tested and working**: installation and uninstallation via eopkg
> have been verified on Solus 4.9 (eopkg 5.0.0).

### Installation via script (any Linux distribution)

```sh
# 1. Clone the repository
git clone https://github.com/Julien684/btrfsmgr.git
cd btrfsmgr

# 2. Install
sudo ./install.sh
```
### Uninstallation via script (any Linux distribution)

```sh
sudo btrfsmgr uninstall
```
# Usage
```sh
sudo btrfsmgr                 # manages the / subvolume
sudo btrfsmgr /mnt/backup     # manages another BTRFS subvolume
```

Dependencies: `btrfs-progs`, `python3 ≥ 3.8`, `systemd` (timers).

Restoration requires **no rsync and no copy**: it is a CoW subvolume
swap (two `mv` commands).

## Version and update

`btrfsmgr --version` (or `-V`) shows the installed version.

The project is versioned with *git tags* (`v1.0.0`, `v1.1.0`, …), and
the local version is the constant `VERSION` in `btrfsmgr.py`.

`btrfsmgr update` (menu/CLI, no argument) automatically checks the
latest version:

```sh
sudo btrfsmgr update
```

* queries the repository tags (API, otherwise `git ls-remote`);
* compares the remote version to the local version;
* if a **higher** version is available, it offers it and, upon
  confirmation, clones that version (temporary clone) then re-runs
  `install.sh` — your local working repository is not modified;
* otherwise it displays "You are already up to date" (local
  reinstallation is still possible);
* offline, or if the repository has no tags, it falls back to the
  local `install.sh` (historical behavior):
  `btrfsmgr update /path/to/the/repository`.

## Features

| Menu | Action |
|------|--------|
| 1 | Create a snapshot (read-only) in `@snapshots/` |
| 2 | List snapshots + other subvolumes |
| 3 | Restore: CoW swap (two `mv`) — the snapshot becomes `@`, the old `@` is kept in `snapshots/@_old-<date-time>` (everything is CoW, nothing is copied) |
| 4 | Delete a snapshot (single one, several `1,3,5`, or `all`) |
| 5 | Schedule automatic snapshots (systemd timer) with `OnCalendar` planning (e.g. `daily`, `weekly`, `Mon *-*-* 03:00:00`) and retention (keep the N most recent, delete the oldest) |
| 6 | Remove an automation (systemd timer) |
| 7 | Manual retention: delete all snapshots except the N most recent |
| 8 | Info: detected boot loader, active timers, disk space |
| 9 | systemd-boot menu: show/hide the boot menu (timeout) |
| 10 | Quit |

## Testing status

Features **tested and working**:

* Snapshot creation
* Listing snapshots
* Deleting a snapshot
* Removing an automation (systemd timer)
* System info (boot loader, timers…)
* systemd-boot menu: show/hide at boot time

Feature **currently being tested**:

* Scheduling automatic snapshots (systemd) with all scheduling options
  and automatic deletion (retention).

## systemd timers

`btrfsmgr` creates (menu 5):

* `/etc/systemd/system/btrfsmgr-snap-<root>-<snapdir>-<tag>.service`
* `/etc/systemd/system/btrfsmgr-snap-<root>-<snapdir>-<tag>.timer`

The service runs `btrfsmgr auto <root> --snapdir @snapshots
--name <tag>-%Y%m%d-%H%M%S --keep N`: it creates the snapshot then
automatically deletes the oldest ones beyond N (retention).

Monitoring: `systemctl list-timers | grep btrfsmgr`,
`journalctl -u btrfsmgr-snap-*`.

Removing a schedule: TUI option 6 — lists the `btrfsmgr` timers,
you pick one, then `systemctl disable --now` + deletion of the files in
`/etc/systemd/system`.

## Booting from a snapshot

* **GRUB**: a `menuentry` entry is generated in
  `/etc/grub.d/40_btrfsmgr_snapshots` (then `grub-mkconfig` regenerates
  `grub.cfg` if available).
* **systemd-boot**: a UEFI entry is written in
  `/boot/loader/entries/btrfsmgr-<name>.conf`.

* **Solus**: since the EFI partition (`/boot`) is not mounted by
  default, `btrfsmgr` mounts it automatically via
  `clr-boot-manager mount-boot` (fallback `mount /boot`) before
  creating the boot entry.

The boot entry uses the same kernel as the current system. Its options
are **cloned from the running system** (`/proc/cmdline`) —
`root=PARTUUID=…`, `rootflags=subvol=…`, `rd.vconsole*`, `quiet splash
rw`, `plymouth`… — and only the subvolume is replaced, so it boots
exactly like the current system. Example generated on Solus:

```
options: root=PARTUUID=b11754f0-… rootflags=subvol=@snapshots/<name> \
         rd.vconsole.keymap=fr rd.vconsole.font=ter-v32b quiet splash rw \
         plymouth.use-simpledrm
```

Reboot and choose the entry in the boot loader.

## Startup synchronization

On every TUI startup, `btrfsmgr` synchronizes the **systemd-boot**
entries it generated with the snapshots actually present in
`<root>/@snapshots`:

* **adds** an entry (`.conf` file) for each orphaned snapshot;
* **deletes** the `.conf` file whose snapshot no longer exists.

Only the `btrfsmgr-*.conf` files in `/boot/loader/entries` are
affected — entries from other tools (e.g. `Solus-current-*.conf`)
remain untouched. If the boot loader is not systemd-boot (GRUB), this
step is a silent no-op.

## "mv swap" restoration mode

Menu 3:

1. `mv  @ → snapshots/@_old-<date-time>`   — the old current subvolume
   is kept as a dated snapshot.
2. `mv  snapshots/<snapshot> → @`   — the chosen snapshot takes over
   `@` and becomes the bootable root (the `ro` flag is lifted).
3. `btrfs subvolume set-default @ /`   — the default subvolume is
   repointed to `@` (the restored subvolume).

Everything is CoW: two subvolume renames, no data copied. The live
mount of `/` remains valid (a mount follows the subvolume, not its
name). Reboot afterwards to boot into the restored version.

## Scripting utility

```sh
btrfsmgr auto / --snapdir @snapshots --name test --keep 3
```

Creates `//@snapshots/test` then keeps only the 3 most recent.