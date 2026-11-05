# btrfsmgr — version Rust

Portage du cœur Python (`btrfsmgr.py`, 3220 lignes) vers un binaire Rust
unique. Cette version est désormais sur `main`.

## Construction

```bash
cargo build --release      # target/release/btrfsmgr
cargo test --release       # tests unitaires + non-régression
BTRFSMGR_ALLOW_NONROOT=1 ./target/release/btrfsmgr /   # rendu/menus sans root
```

## Tests de non-régression

`tests/regression.rs` vérifie des **valeurs de référence figées** (plus aucune
dépendance à Python, supprimé du dépôt) :

* fonctions pures : `snaprel`, `snap_subvol`, `fver_match`, versions,
  sélections multiples, tailles, noms d'unités systemd ;
* contenu **exact** des unités `.service` / `.timer` générées ;
* intégrité du script d'instantané embarqué (`assets/btrfsmgr-snap.sh`) ;
* environnement BTRFS simulé (stubs `btrfs` / `findmnt` / `systemctl`) pour
  `list_snapshots`, `prune_snapshots` et `list_timers`.

## Écarts par rapport à l'ancienne version Python

* `detect_bootloader()` sonde aussi `/etc/grub.d`, `/etc/default/grub`,
  `/boot/grub{,2}/grub.cfg`, les `grub.cfg` EFI et
  `/sys/firmware/efi/efi/systemd`. L'ancienne version ne regardait que
  `/boot/loader` puis `/etc/grub` et rendait « Inconnu » sur un GRUB sain.
* Rétention : `keep <= 0` conserve **tout** (cohérent avec le libellé UI et
  avec le script shell). Le Python supprimait tous les instantanés dans
  `prune_snapshots(0)` tout en affichant « 0 = tous ».
* Le TUI utilise ANSI/termios (pas de dépendance curses) et conserve le
  repli numéroté hors terminal.

## État

* L'`.eopkg` Rust est construit : `btrfsmgr-1.0.15-10-1-x86_64.eopkg`
  (binaire unique, aucune dépendance Python).
* Le script `install.sh` a été retiré : l'installation depuis les sources se
  fait par `cargo build --release` puis copie du binaire.
* Validé sur la VM Solus : création/suppression d'instantané, timer systemd,
  synchronisation des entrées de boot, clics souris du TUI.
* Les fichiers Python de l'ancienne implémentation ont été retirés ; seul
  `build_eopkg.py` reste (outil de packaging).

