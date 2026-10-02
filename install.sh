#!/usr/bin/env bash
# Installation / mise à jour de btrfsmgr
set -euo pipefail

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST=/bin/btrfsmgr

if [[ $EUID -ne 0 ]]; then
    echo "ERREUR: lancer en root (sudo)" >&2
    exit 1
fi

command -v btrfs >/dev/null || {
    echo "ERREUR: btrfs-progs introuvable." >&2
    echo "  Solus:  eopkg install btrfsprogs" >&2
    echo "  Debian: apt install btrfs-progs" >&2
    exit 1
}

command -v python3 >/dev/null || {
    echo "ERREUR: python3 introuvable." >&2
    echo "  Solus:  eopkg install python3" >&2
    exit 1
}

install -m 0755 "$SRC_DIR/btrfsmgr.py" "$DEST"
echo "✓ btrfsmgr installé dans $DEST"

# --- Dossiers des snapshots ------------------------------------------------
# Les snapshots vivent dans un sous-volume `snapshots` au NIVEAU RACINE du
# FS BTRFS (même niveau que @ et @home), monté sur /snapshots.
#
# Quand / est monté sur @, on ne peut pas créer de sous-volume à la racine
# du FS sans un montage temporaire — c'est `btrfsmgr ensure` qui s'en
# charge (montage temporaire de la racine, création du sous-volume,
# migration des anciens @snapshots ou snapshots-in-@, montage sur
# /snapshots, écriture de fstab).  C'est idempotent.
# --------------------------------------------------------------------------
echo
echo "Vérification du dossier des snapshots (snapshots au niveau racine)…"
"$DEST" ensure / --snapdir snapshots

# rsync est utilisé par la restauration (mode 1)
if ! command -v rsync >/dev/null; then
    echo
    echo "⚠ rsync introuvable — la restauration en sera limitée."
    echo "  Solus:  eopkg install rsync"
fi

# clr-boot-manager est utilisé pour monter /boot (Solus, partition EFI)
if ! command -v clr-boot-manager >/dev/null; then
    echo "⚠ clr-boot-manager introuvable — le montage automatique de /boot"
    echo "  (boot sur un instantané) est limité au fallback 'mount /boot'."
fi

echo
echo "Lancer     :  btrfsmgr              (interface TUI, sous-volume /)"
echo "ou         :  btrfsmgr /chemin      (autre sous-volume BTRFS)"
echo "MàJ        :  btrfsmgr update /chemin/vers/le/depot"
echo "Désinstall :  btrfsmgr uninstall    (root; conserve les snapshots)"
