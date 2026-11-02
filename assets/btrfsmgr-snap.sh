#!/usr/bin/env bash
# btrfsmgr-snap.sh — créer un snapshot BTRFS + prune (conserver les N derniers).
# Usage: btrfsmgr-snap.sh <root> <snapdir> <name> <keep> [once_per_day]
# once_per_day=1 → n'effectue (au plus) qu'un instantané par jour: si un
# instantané daté d'aujourd'hui existe déjà, on ne crée rien.
set -euo pipefail

ROOT="${1:-}"
SNAPDIR="${2:-snapshots}"
NAME="${3:-}"
KEEP="${4:-7}"
ONCE_PER_DAY="${5:-0}"

if [[ -z "$ROOT" || -z "$NAME" ]]; then
    echo "Usage: $0 <root> <snapdir> <name> <keep> [once_per_day]" >&2
    exit 1
fi

# Le nom peut contenir un format strftime (ex. savejour-%Y%m%d-%H%M%S,
# fourni tel quel par l'unité systemd): on l'expand en timestamp réel.
NAME="$(date +"$NAME")"

dev_of() {
    findmnt -n -o SOURCE "$1" 2>/dev/null | head -1 | cut -d'[' -f1
}
is_subvolume() { btrfs subvolume show "$1" >/dev/null 2>&1; }
is_ro()        { btrfs property get "$1" ro 2>/dev/null | grep -q 'ro=true'; }

subvol_id() {
    local base="$1" sname="$2" line
    line="$(btrfs subvolume list "$base" 2>/dev/null \
        | awk -v s="$sname" '{p=$0; sub(/^.* path /,"",p); if (p==s) {print $0; exit}}')"
    [[ -n "$line" ]] && echo "$line" | awk '{print $2}'
}

ensure_snapdir() {
    local root="$1" snapdir="$2"
    local vis="$root/$snapdir"
    local dev
    dev="$(dev_of "$root")"
    [[ -z "$dev" ]] && { echo "ERREUR: device BTRFS introuvable" >&2; exit 1; }

    if [[ -n "$(findmnt -n "$vis" 2>/dev/null)" ]]; then
        local cur
        cur="$(findmnt -n -o SOURCE "$vis" | head -1)"
        [[ "$cur" == *"/$snapdir]"* || "$cur" == *"/$snapdir" ]] && return 0
        umount "$vis" 2>/dev/null || true
    fi

    local M
    M="$(mktemp -d /tmp/btrfsmgr-snap.XXXXXX)"
    mount -o "subvol=/,rw" "$dev" "$M"
    local p="$M/$snapdir" entry

    for entry in "$M/.btrfsmgr-mig."*; do
        [[ -e "$entry" ]] && btrfs subvolume delete "$entry" 2>/dev/null || true
    done

    if is_subvolume "$p"; then
        if is_ro "$p"; then
            local tmp="$M/.btrfsmgr-mig.$$"
            btrfs subvolume create "$tmp"
            local rel
            while IFS= read -r rel; do
                [[ -z "$rel" ]] && continue
                is_ro "$p/$rel" && btrfs property set "$p/$rel" ro false
                mv "$p/$rel" "$tmp/$rel" 2>/dev/null || true
            done < <(btrfs subvolume list "$p" 2>/dev/null \
                     | awk '{x=$0; sub(/^.* path /,"",x); print x}' \
                     | grep -v "^$snapdir$")
            btrfs subvolume delete "$p"
            btrfs subvolume create "$p"
            local child
            for child in "$tmp"/*; do
                [[ -e "$child" ]] && mv "$child" "$p/" 2>/dev/null || true
            done
            btrfs subvolume delete "$tmp" 2>/dev/null || true
        fi
    else
        rm -rf "$p" 2>/dev/null || true
        btrfs subvolume create "$p"
    fi

    umount "$M"
    rmdir "$M"

    if [[ -z "$(findmnt -n "$vis" 2>/dev/null)" ]]; then
        mkdir -p "$vis"
        local f
        for f in "$vis"/* "$vis"/.*; do
            [[ -e "$f" && "$f" != "$vis/." && "$f" != "$vis/.." ]] \
                && rm -rf "$f" 2>/dev/null || true
        done
        mount -o "subvol=/$snapdir,rw,noatime" "$dev" "$vis"
    fi
}

prune_snapshots() {
    local root="$1" snapdir="$2" keep="$3"
    local vis="$root/$snapdir"
    [[ "$keep" -le 0 ]] && return 0
    local snapdir_id
    snapdir_id="$(subvol_id "$root" "$snapdir")"
    [[ -z "$snapdir_id" ]] && return 0

    local lines
    lines="$(btrfs subvolume list "$root" 2>/dev/null \
        | awk -v id="$snapdir_id" '
            {
              gen=""; top=""; path=""
              for (i=1; i<=NF; i++) {
                if ($i == "gen")   gen  = $(i+1)
                if ($i == "level" && $(i-1)=="top") top = $(i+1)
                if ($i == "path")  path = $(i+1)
              }
              if (top == id && path != "") print gen, path
            }')"
    local total
    total="$(echo "$lines" | grep -c . || true)"
    [[ "$total" -le "$keep" ]] && return 0
    local to_delete=$(( total - keep ))
    echo "$lines" | sort -n | head -n "$to_delete" | while read -r gen pth; do
        [[ -z "$pth" ]] && continue
        echo "  Suppression: $root/$pth (gen $gen)"
        btrfs subvolume delete "$root/$pth" 2>/dev/null \
            || echo "  ! échec: $root/$pth"
    done
}

ensure_snapdir "$ROOT" "$SNAPDIR"
vis="$ROOT/$SNAPDIR"

# Un seul instantané par jour: si un instantané daté d'aujourd'hui est déjà
# présent, on s'arrête — le déclenchement au démarrage est alors capé à un
# instantané par jour (un démarrage supplémentaire le même jour est ignoré).
if [[ "$ONCE_PER_DAY" == "1" ]]; then
    _day="$(date +%Y%m%d)"
    if compgen -G "$vis/*$_day*" >/dev/null 2>&1; then
        echo "[btrfsmgr-snap] instantané du jour ($_day) déjà présent — ignoré"
        exit 0
    fi
fi

target="$vis/$NAME"
if [[ -e "$target" ]]; then
    echo "ERREUR: $target existe déjà" >&2
    exit 1
fi
echo "[btrfsmgr-snap] créant: $ROOT → $target"
btrfs subvolume snapshot "$ROOT" "$target"
echo "[btrfsmgr-snap] OK: $target"
# Notification graphique aux utilisateurs connectés.
# Le service tourne en ROOT: `notify-send` seul s'exécute dans la session
# root (sans bus DBus) et n'atteint PAS le bureau.  On envoie donc la
# notification via le bus de session de chaque utilisateur graphique.
if command -v notify-send >/dev/null 2>&1 \
   && command -v loginctl >/dev/null 2>&1; then
    for _u in $(loginctl list-users --no-legend 2>/dev/null | awk '{print $2}'); do
        _uid=$(id -u "$_u" 2>/dev/null) || continue
        _bus="/run/user/$_uid/bus"
        [ -S "$_bus" ] || continue
        DBUS_SESSION_BUS_ADDRESS="unix:path=$_bus" DISPLAY=:0 \
            su -s /bin/sh "$_u" -c \
            "notify-send -a 'BTRFS Manager' -n chronometer -i chronometer \
             -h string:desktop-entry:__BM_DESKTOP_ENTRY__ \
             'BTRFS Manager' 'Instantané créé: $SNAPDIR/$NAME'" 2>/dev/null || true
    done
fi
prune_snapshots "$ROOT" "$SNAPDIR" "$KEEP"

# Resynchroniser les entrées systemd-boot (comme en création manuelle):
# ajouter le nouveau snapshot, retirer les .conf des snapshots prunés.
# No-op silencieux si le chargeur n'est pas systemd-boot.
for _bm in $(command -v btrfsmgr) /bin/btrfsmgr /usr/local/bin/btrfsmgr; do
    [[ -x "$_bm" ]] || continue
    "$_bm" sync "$ROOT" --snapdir "$SNAPDIR" || true
    break
done
