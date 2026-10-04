#!/usr/bin/env python3
"""
btrfsmgr — Gestionnaire TUI pour BTRFS sur Linux.

Fonctionnalités:
  * Créer / supprimer des instantanés (snapshots)
  * Lister les instantanés d'un sous-volume
  * Programmer des instantanés automatiques (timers systemd) avec
    rétention (conserver les N plus récents, supprimer les plus anciens)
  * Restaurer un snapshot (swap mv: le snapshot devient @, l'ancien @
    est conservé dans snapshots/@_old-<ts>)
  * Ajouter un snapshot au boot: entrées GRUB + entrées systemd-boot
    (BtrfsSubvol=), redémarrer ou basculer sur un snapshot précis

Usage:
    sudo btrfsmgr [SUBVOLUME]

Raccourcis:
  1      Menu principal
  s      Créer un snapshot
  l      Lister les snapshots
  r      Restaurer un snapshot
  d      Détruire un snapshot
  p      Programmer des snapshots automatiques
  x      Supprimer un timer systemd
  i      Infos système
  m      Menu systemd-boot: afficher/masquer au démarrage
  q      Quitter

Dépendances: python3 (>=3.8), btrfs-progs, systemd (pour les timers et boot)
"""

from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import os
import pwd
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
import time

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------

SYSTEMD_SNAPDIR = "/etc/systemd/system"
SNAPSHOTS_DIR = "/var/lib/btrfsmgr"
SNAPSHOTS_STATE = "/var/lib/btrfsmgr/snapshots.state"
GRUB_SNAPDIR_FILE = "/etc/grub.d/40_btrfsmgr_snapshots"
BOOTXMD_SNAPDIR = "/boot/loader/entries/btrfsmgr-snapshots.conf"
SNAP_SCRIPT = "/usr/local/bin/btrfsmgr-snap.sh"
DESKTOP_FILE = "/usr/share/applications/btrfsmgr.desktop"

APP = "btrfsmgr"

# Dossier des snapshots, créé au même niveau que @ et @home
DEFAULT_SNAPDIR = "snapshots"

# Script shell auto-suffisant (pas de dépendance python) exécuté par
# l'unité systemd pour créer + pruner les snapshots.
_SNAP_SCRIPT_BODY = r"""#!/usr/bin/env bash
# btrfsmgr-snap.sh — créer un snapshot BTRFS + prune (conserver les N derniers).
# Usage: btrfsmgr-snap.sh <root> <snapdir> <name> <keep>
set -euo pipefail

ROOT="${1:-}"
SNAPDIR="${2:-snapshots}"
NAME="${3:-}"
KEEP="${4:-7}"

if [[ -z "$ROOT" || -z "$NAME" ]]; then
    echo "Usage: $0 <root> <snapdir> <name> <keep>" >&2
    exit 1
fi

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
target="$vis/$NAME"
if [[ -e "$target" ]]; then
    echo "ERREUR: $target existe déjà" >&2
    exit 1
fi
echo "[btrfsmgr-snap] créant: $ROOT → $target"
btrfs subvolume snapshot "$ROOT" "$target"
echo "[btrfsmgr-snap] OK: $target"
# Notification graphique (si notify-send est dispo — GNOME/KDE)
if command -v notify-send >/dev/null 2>&1; then
    notify-send "BTRFS Manager" \
        "Instantané créé: $SNAPDIR/$NAME" \
        -i system-software-update 2>/dev/null || true
fi
prune_snapshots "$ROOT" "$SNAPDIR" "$KEEP"
"""


# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------


def is_root() -> bool:
    return os.geteuid() == 0


def run(cmd, check: bool = True):
    """Run a command and return (rc, stdout+stderr)."""
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    except FileNotFoundError:
        print(f"ERREUR: commande introuvable: {cmd[0]}")
        return 127, ""
    except subprocess.TimeoutExpired:
        print(f"ERREUR: timeout: {' '.join(cmd)}")
        return 124, ""
    out = (p.stdout or "") + (p.stderr or "")
    if check and p.returncode != 0:
        print(f"ERREUR (rc={p.returncode}): {' '.join(cmd)}")
        print(out.strip())
    return p.returncode, out


def btrfs_available() -> bool:
    return shutil.which("btrfs") is not None


def systemd_available() -> bool:
    # systemd-nspawn containers: `systemctl is-system-running` may fail
    rc, _ = run(["systemctl", "is-system-running"], check=False)
    return rc == 0


def read_state() -> dict:
    if not os.path.exists(SNAPSHOTS_STATE):
        return {}
    try:
        with open(SNAPSHOTS_STATE) as f:
            import json
            return json.load(f)
    except Exception:
        return {}


def write_state(data: dict):
    import json
    os.makedirs(SNAPSHOTS_DIR, exist_ok=True)
    with open(SNAPSHOTS_STATE, "w") as f:
        json.dump(data, f, indent=2)
    os.chmod(SNAPSHOTS_STATE, 0o644)


def now_stamp() -> str:
    return dt.datetime.now().strftime("%Y%m%d-%H%M%S")


def human_size(n: int) -> str:
    for unit in ("o", "K", "M", "G", "T", "P"):
        if abs(n) < 1024:
            return f"{n:.1f}{unit}"
        n /= 1024
    return f"{n:.1f}E"


# ---------------------------------------------------------------------------
# BTRFS queries
# ---------------------------------------------------------------------------


def btrfs_subvols(root: str) -> list[dict]:
    """Return list of snapshots of `root` (btrfs subvolume list -s)."""
    rc, out = run(["btrfs", "subvolume", "list", "-s", root], check=False)
    if rc != 0:
        return []
    subs = []
    for line in out.splitlines():
        # "generation 6161 genid 6161 top level 5 path @snapshots/20240101-120000"
        if " path " not in line:
            continue
        try:
            gen = int(line.split()[1])
            path = line.split(" path ", 1)[1].strip()
        except (ValueError, IndexError):
            continue
        subs.append({"gen": gen, "path": path, "full": os.path.join(root, path)})
    subs.sort(key=lambda s: s["gen"])
    return subs


def all_subvols(root: str) -> list[dict]:
    """All subvolumes under root (including non-snapshots), oldest first."""
    rc, out = run(["btrfs", "subvolume", "list", root], check=False)
    subs = []
    if rc == 0:
        for line in out.splitlines():
            if " path " not in line:
                continue
            try:
                gen = int(line.split()[1])
                path = line.split(" path ", 1)[1].strip()
            except (ValueError, IndexError):
                continue
            subs.append({"gen": gen, "path": path, "full": os.path.join(root, path)})
    subs.sort(key=lambda s: s["gen"])
    return subs


def move_subvol(src: str, dst: str) -> tuple[int, str]:
    """Déplacer un sous-volume `src` vers `dst` via snapshot + delete.

    Compatibilité avec les vieilles versions de btrfs-progs qui n'ont pas
    `btrfs subvolume move`.  Les deux sont équivalentes (CoW).
    """
    rc, out = run(["btrfs", "subvolume", "snapshot", src, dst], check=False)
    if rc != 0:
        return rc, out
    rc, out = run(["btrfs", "subvolume", "delete", src], check=False)
    return rc, out


def is_ro_subvolume(path: str) -> bool:
    """True si `path` est un snapshot BTRFS read-only.

    Méthode principale: `btrfs property get <path> ro` → "ro=true"/
    "ro=false" (fiable sur toutes les versions ≥ 4.20).  Fallback ancien:
    parsing de `btrfs subvolume show` (dispositif + "Snapshot: yes" ou
    Flags: ro) — utile uniquement si `property` n'est pas disponible.
    """
    if not is_btrfs_subvolume(path):
        return False
    # 1) property get ro (robuste, y compris sur btrfs-progs v7.x où
    #    `subvolume show -s` n'existe pas).
    rc, out = run(["btrfs", "property", "get", path, "ro"], check=False)
    if rc == 0 and "ro=" in out:
        return "ro=true" in out
    # 2) fallback historique (ancien btrfs-progs sans `property`)
    rc, out = run(["btrfs", "subvolume", "show", path], check=False)
    if rc == 0:
        for line in out.splitlines():
            s = line.strip()
            if s.startswith("Snapshot:"):
                return "yes" in s
            if s.startswith("Flags:") and "ro" in s.split():
                return True
    return False


def _subvols_by_id(root: str) -> dict:
    """{subvolume_id: nom} du FS via `btrfs subvolume list` (chemins sans
    slash initial).  Fonctionne même quand le sous-volume demandé n'est
    pas visible depuis le montage courant."""
    rc, out = run(["btrfs", "subvolume", "list", root], check=False)
    m = {}
    if rc == 0:
        for line in out.splitlines():
            if " path " not in line:
                continue
            parts = line.split()
            if len(parts) < 2:
                continue
            p = line.split(" path ", 1)[1].strip().lstrip("/")
            if not p:
                continue
            try:
                m[parts[1]] = p
            except ValueError:
                continue
    return m


def _subvol_of_mount(mount: str, root: str) -> str | None:
    """Nom (sans slash) du sous-volume BTRFS monté sur `mount`.

    Indépendant de la version de util-linux / btrfs-progs:
      * SOURCE `dev[/name]` (modern findmnt) → `name`
      * OPTIONS `subvol=name` → `name`
      * OPTIONS `subvolid=N` (anciens findmnt) → lookup de N dans
        `btrfs subvolume list` (montage temporaire de la racine du FS si
        le sous-volume n'y figure pas)
    None si `mount` n'est pas monté sur un sous-volume."""
    rc, out = run(["findmnt", "-n", "-o", "SOURCE,OPTIONS", mount],
                  check=False)
    if rc != 0:
        return None
    line = out.strip().splitlines()[0] if out.strip() else ""
    m = re.search(r"\[(.+)\]", line)
    if m and m.group(1):
        return m.group(1).strip("/")
    subvolid = None
    for tok in line.split():
        if tok.startswith("subvol="):
            return tok[len("subvol="):].strip("/")
        if tok.startswith("subvolid="):
            subvolid = tok[len("subvolid="):]
    if subvolid:
        table = _subvols_by_id(root)
        if subvolid in table:
            return table[subvolid]
        with fs_root_mount(root) as r:
            table = _subvols_by_id(r)
        return table.get(subvolid)
    return None


def is_subvolume_rw(path: str) -> bool:
    """True si `path` est un sous-volume BTRFS **read-write**.

    La référence est le montage: un snapshot read-only y est toujours
    monté en `ro` (option de montage, indépendante de la version de
    btrfs-progs).  `btrfs subvolume show -s` (Snapshot: yes/no) sert de
    renfort quand il est parsable."""
    if not os.path.exists(path):
        return False
    rc, out = run(["findmnt", "-n", "-o", "OPTIONS", path], check=False)
    if rc == 0 and out.strip():
        first = out.strip().splitlines()[0]
        if re.search(r"(^|,)ro(,|$)", first):
            return False
    rc, out = run(["btrfs", "subvolume", "show", "-s", path], check=False)
    if rc == 0:
        for line in out.splitlines():
            s = line.strip()
            if s.startswith("Snapshot:"):
                return "yes" not in s
            if s.startswith("Flags:") and "ro" in s.split():
                return False
    return True


def subvol_name(path: str) -> str | None:
    """Nom du sous-volume BTRFS auquel `path` appartient (champ `Name:`
    de `btrfs subvolume show` — sans slash, ex. "snapshots").  None si
    `path` n'est pas (dans) un sous-volume."""
    rc, out = run(["btrfs", "subvolume", "show", path], check=False)
    if rc != 0:
        return None
    for line in out.splitlines():
        s = line.strip()
        if s.startswith("Name:"):
            return s[len("Name:"):].strip()
    return None


def list_subvols(mount: str) -> list[str]:
    """Chemin des subvolumes de `mount` (relatifs à la racine du FS, sans
    slash initial, ex. "snapshots/20260101-120000").  Fonctionne aussi pour
    des snapshots RO (os.listdir échoue dessus, btrfs si)."""
    rc, out = run(["btrfs", "subvolume", "list", mount], check=False)
    paths = []
    for line in out.splitlines():
        if " path " in line:
            paths.append(line.split(" path ", 1)[1].strip().lstrip("/"))
    return paths


def is_btrfs_subvolume(path: str) -> bool:
    """True if `path` exists and is a BTRFS subvolume (not a read-only
    snapshot — those cannot contain nested subvolumes)."""
    if not os.path.exists(path):
        return False
    rc, out = run(["btrfs", "subvolume", "show", path], check=False)
    return rc == 0


def btrfs_dev_of(path: str) -> str:
    """Return the bare block device (no `[/subvol]` suffix) for `path`."""
    rc, out = run(["findmnt", "-n", "-o", "SOURCE", path], check=False)
    dev = out.strip().splitlines()[0].strip() if out.strip() else ""
    return dev.split("[", 1)[0]


@contextlib.contextmanager
def fs_root_mount(root: str):
    """Montage temporaire de la RACINE du fichier de système BTRFS.

    Quand / est monté sur un sous-volume (ex. @), on ne peut pas créer de
    sous-volume au niveau racine du FS (top level 5, même niveau que @ et
    @home): `btrfs subvolume create /X` créerait X DEDANS @.  Ce contexte
    monte le device avec subvol=/ dans un répertoire temporaire — on y
    opère au niveau racine du FS — puis démonte.

    Si `root` est déjà la racine du FS montée (subvol=/), on le réutilise
    tel quel (pas de remontage).
    """
    rc, out = run(["findmnt", "-n", "-o", "SOURCE", root], check=False)
    src = out.strip().splitlines()[0].strip() if out.strip() else ""
    if "[" not in src:
        # déjà monté sur la racine du FS
        yield root
        return
    dev = src.split("[", 1)[0]
    mnt = tempfile.mkdtemp(prefix="btrfsmgr-root.")
    rc, out = run(["mount", "-o", "subvol=/,rw", dev, mnt], check=False)
    if rc != 0:
        shutil.rmtree(mnt, ignore_errors=True)
        print(f"ERREUR: montage temporaire de la racine du FS impossible:\n{out.strip()}")
        sys.exit(1)
    try:
        yield mnt
    finally:
        run(["umount", mnt], check=False)
        shutil.rmtree(mnt, ignore_errors=True)


def default_subvol(root: str) -> dict:
    """Return the *default* subvolume of the filesystem at root."""
    rc, out = run(["btrfs", "subvolume", "get-default", root], check=False)
    if rc != 0:
        return {"gen": 0, "path": "", "full": root}
    # "ID 5 (path @) gen 12348 top level 5"  |  "ID 256 (path @)"  |
    # "ID 5 (FS_TREE)" (pas de subvolume par défaut explicite)
    m = re.search(r"ID\s+(\d+)\s*(?:\((path\s+([^\s)]+)|FS_TREE)\))?\s*"
                  r"(?:gen\s+(\d+))?", out)
    if m:
        svol = (m.group(3) or "").strip()
        return {"gen": int(m.group(4) or 0), "path": svol,
                "full": os.path.join(root, svol) if svol else root}
    return {"gen": 0, "path": "", "full": root}


def make_snapshot(source: str, target: str) -> tuple[int, str]:
    """Créer un snapshot **read-write** de `source` vers `target`.

    RW (pas `-r`): un snapshot btrfs read-only ne peut PAS être monté en
    écriture — le flag est intrinsèque au sous-volume.  Pour pouvoir
    booter dessus (systemd-boot/GRUB) il doit être RW.

    `ensure_snapdir()` garantit que le parent de `target` est déjà un
    sous-volume BTRFS RW (BTRFS refuse de créer des sous-volumes dans un
    snapshot read-only).  On ne cherche PAS à le recréer ici: un
    sous-volume monté ne peut pas être supprimé (ETXTBSY/Invalid
    argument), et s'il n'existe pas, la création échouerait quand même —
    la cause est alors un état incohérent à réparer via ensure_snapdir.
    """
    parent = os.path.dirname(target)
    if parent and not is_subvolume_rw(parent):
        return 1, (f"le dossier des snapshots {parent} n'est pas un "
                   f"sous-volume BTRFS RW (ensure_snapdir)")
    return run(["btrfs", "subvolume", "snapshot", source, target])


def delete_subvol(path: str) -> tuple[int, str]:
    return run(["btrfs", "subvolume", "delete", path])


def _subvol_gen_by_path(root: str, path: str) -> int | None:
    """Génération (ID) d'un sous-volume dont le chemin (relatif au root
    monté) est `path`. None si introuvable."""
    rc, out = run(["btrfs", "subvolume", "list", root], check=False)
    if rc != 0:
        return None
    for line in out.splitlines():
        if " path " not in line:
            continue
        p = line.split(" path ", 1)[1].strip()
        if p == path:
            try:
                return int(line.split()[1])
            except (ValueError, IndexError):
                return None
    return None


def _snapdir_fstab_line(root: str, snapdir: str) -> str:
    """Ligne fstab du sous-volume <racine du FS>/<snapdir> monté sur
    <root>/<snapdir> (ex. /snapshots).  Format standard: device + subvol=.
    """
    dev = btrfs_dev_of(root) or "<device>"
    mnt = os.path.join(root, snapdir)
    return f"{dev}\t{mnt}\tbtrfs\tnoatime,subvol=/{snapdir} 0 0\n"


def ensure_snapdir(root: str, snapdir: str) -> str:
    """Assurer que le dossier des snapshots existe et est opérationnel.

    Architecture:
      * le sous-volume `snapshots` vit au **niveau racine du FS**
        (top level 5, même niveau que @ et @home).  Il est créé via un
        montage temporaire de la racine du FS (fs_root_mount), car quand
        / est monté sur @, `btrfs subvolume create /snapshots` créerait
        le dossier DEDANS @.
      * il est ensuite **monté** sur <root>/<snapdir> (entrée fstab).
        Quand le système est monté sur @, le sous-volume racine-niveau
        n'est pas visible sinon: c'est ce montage qui le rend accessible
        au quotidien (créer/supprimer/lister les snapshots).

    Migrations gérées automatiquement (toutes dans le temp-mount racine,
    où l'ancien sous-volume in-@ et le nouveau sont visibles ensemble):
      * <snapdir> racine-niveau qui est un snapshot RO (remplacé par un
        sous-volume RW, enfants préservés)
      * @snapshots (frère de @, hors de @)
      * <snapdir> créé par erreur DEDANS @ (chemin "@/<snapdir>")

    Retourne le chemin du dossier monté.
    """
    vis = os.path.join(root, snapdir)
    dev = btrfs_dev_of(root)
    if not dev:
        print(f"ERREUR: device BTRFS introuvable pour {root}")
        sys.exit(1)

    rc, _ = run(["findmnt", "-n", vis], check=False)
    if rc == 0 and _subvol_of_mount(vis, root) == snapdir:
        # 0) voie rapide: /snapshots déjà monté sur le bon sous-volume
        pass
    else:
        # 1) dé-monter un montage antérieur (mauvais sous-volume ou non)
        if rc == 0:
            run(["umount", vis], check=False)

        # 2) tout se passe dans le montage temporaire de la racine du FS
        with fs_root_mount(root) as r:
            p = os.path.join(r, snapdir)

            # nettoyage des résidus de migrations interrompues
            for entry in os.listdir(r):
                if entry.startswith(".btrfsmgr-mig."):
                    run(["btrfs", "subvolume", "delete",
                         os.path.join(r, entry)], check=False)

            if is_ro_subvolume(p):
                # snapshot RO à la racine: il ne peut pas accueillir de
                # snapshots.  On déplace les enfants, on le remplace par
                # un vrai sous-volume RW, on remet les enfants.
                children = [c[len(snapdir) + 1:] for c in list_subvols(r)
                            if c.startswith(f"{snapdir}/")]
                tmp = os.path.join(r, f".btrfsmgr-mig.{os.getpid()}")
                rc, out = run(["btrfs", "subvolume", "create", tmp],
                              check=False)
                if rc != 0:
                    print(f"ERREUR: sous-volume temporaire:\n{out.strip()}")
                    sys.exit(1)
                for c in children:
                    rc, out = move_subvol(os.path.join(r, snapdir, c),
                                          os.path.join(tmp, c))
                    if rc != 0:
                        print(f"  ! migration {c}: {out.strip()}")
                run(["btrfs", "subvolume", "delete", p], check=False)
                rc, out = run(["btrfs", "subvolume", "create", p],
                              check=False)
                if rc != 0:
                    print(f"ERREUR: recréation de {p}:\n{out.strip()}")
                    sys.exit(1)
                for c in children:
                    move_subvol(os.path.join(tmp, c),
                                os.path.join(p, c))
                run(["btrfs", "subvolume", "delete", tmp], check=False)
                print(f"  + {snapdir} (racine du FS) remplacé: snapshot RO "
                      f"→ sous-volume RW ({len(children)} snapshots "
                      f"préservés)")
            elif is_btrfs_subvolume(p):
                pass  # sous-volume RW déjà au bon endroit: rien à faire
            else:
                shutil.rmtree(p, ignore_errors=True)
                rc, out = run(["btrfs", "subvolume", "create", p],
                              check=False)
                if rc != 0:
                    print(f"ERREUR: création de {p}:\n{out.strip()}")
                    sys.exit(1)
                print(f"  + {snapdir} créé (racine du FS)")

            # 2a) migration @snapshots (frère de @)
            old = os.path.join(r, "@snapshots")
            if is_btrfs_subvolume(old):
                children = [c[len("@snapshots/"):]
                            for c in list_subvols(r)
                            if c.startswith("@snapshots/")]
                for c in children:
                    rc, out = move_subvol(os.path.join(r, "@snapshots", c),
                                          os.path.join(p, c))
                    if rc == 0:
                        print(f"  + déplacé @snapshots/{c} → "
                              f"snapshots/{c}")
                    else:
                        print(f"  ! @snapshots/{c}: {out.strip()}")
                run(["btrfs", "subvolume", "delete", old], check=False)

            # 2b) migration du résidu in-@ (ancien bug: snapshots DEDANS @)
            in_at = os.path.join(r, "@", snapdir)
            if is_btrfs_subvolume(in_at):
                children = [c[len(f"@/{snapdir}/"):]
                            for c in list_subvols(r)
                            if c.startswith(f"@/{snapdir}/")]
                for c in children:
                    rc, out = move_subvol(os.path.join(r, "@", snapdir, c),
                                          os.path.join(p, c))
                    if rc == 0:
                        print(f"  + déplacé @{snapdir}/{c} (dans @) → "
                              f"snapshots/{c}")
                    else:
                        print(f"  ! @{snapdir}/{c}: {out.strip()}")
                run(["btrfs", "subvolume", "delete", in_at], check=False)

    # 3) point de montage propre + montage (uniquement si non monté)
    rc, _ = run(["findmnt", "-n", vis], check=False)
    if rc != 0:
        if os.path.isdir(vis):
            for entry in os.listdir(vis):
                ep = os.path.join(vis, entry)
                try:
                    if os.path.isdir(ep) and not os.path.islink(ep):
                        shutil.rmtree(ep, ignore_errors=True)
                    else:
                        os.unlink(ep)
                except OSError:
                    pass
        os.makedirs(vis, exist_ok=True)
        rc, out = run(["mount", "-o", f"subvol=/{snapdir},rw,noatime",
                       dev, vis], check=False)
        if rc != 0:
            print(f"ERREUR: montage de {vis} impossible:\n{out.strip()}")
            sys.exit(1)

    # 4) migration des anciens snapshots RO → RW (idempotent).
    #    Un snapshot read-only ne peut PAS être monté en écriture (flag
    #    intrinsèque), donc il n'est pas bootable.  On bascule en RW.
    #    Les snapshots créés par les versions récentes sont déjà RW :
    #    la conversion n'est alors jamais appelée.
    try:
        conv = 0
        for s in list_snapshots(root, snapdir):
            if s["full"] == vis:
                continue
            if os.path.isdir(s["full"]) and is_ro_subvolume(s["full"]):
                rc, _ = run(["btrfs", "property", "set", s["full"],
                             "ro", "false"], check=False)
                if rc == 0:
                    conv += 1
        if conv:
            print(f"  + {conv} ancien(s) snapshot(s) RO → RW (bootable)")
    except Exception:
        pass

    # 5) fstab: on NE monte PAS /snapshots au démarrage.  Le sous-volume
    #    est monté à la volée (on-demand) uniquement quand btrfsmgr en a
    #    besoin (créer/supprimer/lister/boot).  On retire donc l'éventuelle
    #    ligne fstab laissée par une version précédente (idempotent).
    snapdir_full = vis
    _remove_fstab_line_for(snapdir_full, snapdir)
    return vis


def _remove_fstab_line_for(snapdir_full: str, snapdir: str) -> None:
    """Retirer de /etc/fstab les lignes qui monteraient le sous-volume
    `snapdir` sur le point de montage `snapdir_full` (et d'éventuels
    points de montage legacy).  Le montage devient on-demand."""
    fstab = "/etc/fstab"
    try:
        with open(fstab) as f:
            lines = f.readlines()
    except OSError:
        return
    kept = []
    removed = 0
    for ln in lines:
        s = ln.strip()
        if not s or s.startswith("#"):
            kept.append(ln)
            continue
        parts = s.split()
        # 1) le point de montage (col.2) est notre dossier snapshots
        # 2) la source mentionne subvol=/<snapdir>
        if len(parts) >= 2 and parts[1] == snapdir_full:
            removed += 1
            continue
        if any(f"subvol=/{snapdir}" in p for p in parts):
            removed += 1
            continue
        kept.append(ln)
    if removed:
        with open(fstab, "w") as f:
            f.writelines(kept)
        print(f"  - fstab: {removed} ligne(s) retirée(s) "
              f"({snapdir} monté on-demand, plus au boot)")


def set_default_subvol(root: str, subvol: str) -> tuple[int, str]:
    # Resolve numeric ID.  Le sous-volume doit être visible depuis `root`
    # (montage courant); sinon, on passe par un montage temporaire de la
    # racine du FS.
    gen = _subvol_gen_by_path(root, subvol)
    if gen is None:
        with fs_root_mount(root) as r:
            gen = _subvol_gen_by_path(r, subvol)
    if gen is None:
        return 1, f"subvolume introuvable: {subvol}"
    return run(["btrfs", "subvolume", "set-default", str(gen), root])


def btrfs_filesystem_of(path: str) -> str | None:
    """Return the mount point of the btrfs fs containing `path`, or None."""
    rc, out = run(["btrfs", "filesystem", "show", path], check=False)
    if rc != 0:
        return None
    # "Label: '...' ... Total Devices: 1\n * ID: 1 ...  /mnt/data"
    for line in out.splitlines():
        if line.strip().startswith("/"):
            return line.strip().lstrip("* ").strip()
    return None


# ---------------------------------------------------------------------------
# Snapshot operations
# ---------------------------------------------------------------------------


def list_snapshots(root: str, snapdir: str = DEFAULT_SNAPDIR) -> list[dict]:
    """Snapshots de <root>/<snapdir> (snapdir au niveau racine du FS,
    monté sur <root>/<snapdir> par ensure_snapdir).

    Robustesse btrfs-progs ≥ 7.1: `btrfs subvolume list -s <chemin>` ne
    liste plus seulement l'arbre de <chemin> — il renvoie *tous* les
    sous-volumes du FS, avec `path` relatif à la racine du FS.  On filtre
    donc par CHEMIN: un snapshot est un sous-volume dont le `path` est
    `snapdir/<nom>`.  Le chemin est la source de vérité (pas le champ
    `top level`): celui-ci reflète le parent d'ORIGINE d'un sous-volume
    et NE CHANGE PAS lors d'un `mv` (btrfs mv = rename, le `top level`
    reste celui de l'ancien parent).  Conséquence: un sous-volume obtenu
    par `mv` (ex. @ → snapshots/@_old-…) a `top level` = racine du FS,
    pas l'ID du snapdir, et serait écarté par un filtre by-id — alors
    qu'il EST physiquement dans le snapdir.  Le chemin, lui, reflète la
    position physique (`snapshots/@_old-…`) → il est conservé.

    S'il n'est pas monté (premier run), on passe par un montage
    temporaire de la racine du FS (même parsing).
    """
    snap = snapdir.strip("/")
    snap_prefix = snap + "/"
    if os.path.isdir(os.path.join(root, snapdir)):
        base = root
        rc, out = run(["btrfs", "subvolume", "list", base],
                      check=False)
    else:
        with fs_root_mount(root) as r:
            if not os.path.isdir(os.path.join(r, snapdir)):
                return []
            rc, out = run(["btrfs", "subvolume", "list", r],
                          check=False)
    subs = []
    if rc == 0:
        for line in out.splitlines():
            if " path " not in line:
                continue
            parts = line.split()
            try:
                gen = int(parts[1])
            except (IndexError, ValueError):
                continue
            p = line.split(" path ", 1)[1].strip().lstrip("/")
            # Conserver UNIQUEMENT les sous-volumes physiquement dans le
            # snapdir (path = "snapdir/<nom>").  `@_save`, `@home`, `@`
            # ont un path sans préfixe → écartés.  `@_old-…` (après mv)
            # a path `snapshots/@_old-…` → conservé.
            if not p.startswith(snap_prefix):
                continue
            rel = p[len(snap_prefix):]
            if rel == "":
                continue  # le snapdir lui-même
            subs.append({
                "gen": gen,
                "path": rel,
                "full": os.path.join(root, snapdir, rel),
            })
    subs.sort(key=lambda s: s["gen"])
    return subs


def create_snapshot(root: str, snapdir: str = DEFAULT_SNAPDIR,
                     name: str | None = None) -> str:
    name = name or now_stamp()
    # Normaliser les espaces en underscores (noms de sous-volumes robustes)
    name = name.replace(" ", "_")
    ensure_snapdir(root, snapdir)
    target = os.path.join(root, snapdir, name)
    rc, out = make_snapshot(root, target)
    if rc != 0:
        print(out.strip())
        return ""
    print(f"Snapshot créé: {target}")
    return target


# ---------------------------------------------------------------------------
# Retention (suppression des plus anciens)
# ---------------------------------------------------------------------------


def prune_snapshots(root: str, snapdir: str, keep: int):
    """Keep the `keep` most recent snapshots, delete the rest (oldest first)."""
    snapdir_full = os.path.join(root, snapdir)
    snaps = [s for s in list_snapshots(root, snapdir)
             if s["full"] != snapdir_full and os.path.isdir(s["full"])]
    if keep <= 0:
        keep_all = snaps
    else:
        keep_all = snaps[:-keep] if len(snaps) > keep else []
    for s in keep_all:
        print(f"Suppression: {s['full']}")
        rc, out = delete_subvol(s["full"])
        if rc != 0:
            print(out.strip())


# ---------------------------------------------------------------------------
# systemd timers + units
# ---------------------------------------------------------------------------


def _snap_unit_name(root: str, snapdir: str, tag: str) -> str:
    """Unit names must be alphanumeric + '-' + '_' + '.' (no '/' nor '@')."""
    import re
    def safe(s: str) -> str:
        s = re.sub(r"[^A-Za-z0-9._-]", "-", s.strip("/"))
        return s.strip("-")
    return f"{APP}-snap-{safe(root) or 'root'}-{safe(snapdir) or 'snap'}-{safe(tag)}"


def install_snap_script() -> str:
    """Écrire le script shell auto-suffisant SNAP_SCRIPT (chmod +x) et
    renvoyer son chemin.  Le service systemd l'invoque directement, sans
    dépendre du binaire python btrfsmgr.  Installe aussi l'icône menu
    (fichier .desktop) si le dépôt la fournit."""
    os.makedirs(os.path.dirname(SNAP_SCRIPT), exist_ok=True)
    with open(SNAP_SCRIPT, "w") as f:
        f.write(_SNAP_SCRIPT_BODY)
    os.chmod(SNAP_SCRIPT, 0o755)
    # Icône menu de programmes (.desktop)
    try:
        src_desktop = None
        repo_dir = os.path.dirname(os.path.abspath(__file__))
        cand = os.path.join(repo_dir, "btrfsmgr.desktop")
        if os.path.isfile(cand):
            src_desktop = cand
        if src_desktop:
            os.makedirs(os.path.dirname(DESKTOP_FILE), exist_ok=True)
            with open(src_desktop) as rf, open(DESKTOP_FILE, "w") as wf:
                wf.write(rf.read())
            print(f"  - icône menu installée: {DESKTOP_FILE}")
        else:
            # Fallback: on génère un .desktop minimal
            os.makedirs(os.path.dirname(DESKTOP_FILE), exist_ok=True)
            with open(DESKTOP_FILE, "w") as f:
                f.write(
                    "[Desktop Entry]\n"
                    "Type=Application\n"
                    "Name=BTRFS Manager\n"
                    "Comment=Gérer les sous-volumes BTRFS — snapshots, "
                    "restauration, automation\n"
                    "Exec=/usr/local/bin/btrfsmgr\n"
                    "Icon=system-software-update\n"
                    "Terminal=true\n"
                    "Categories=System;Filesystem;\n"
                )
            print(f"  - icône menu installée: {DESKTOP_FILE}")
    except OSError as e:
        print(f"  ! icône menu non installée: {e}")
    return SNAP_SCRIPT


def write_timer_files(root: str, snapdir: str, tag: str, schedule: str,
                      keep: int, oneshot: bool = True) -> str:
    """Create service + timer units for automatic snapshots with retention.

    `schedule` is an OnCalendar value (e.g. "daily", "weekly", "hourly-*",
    "Mon *-*-* 03:00:00").  `keep` is the number of snapshots to retain.
    """
    unit_name = _snap_unit_name(root, snapdir, tag)
    svc = f"{unit_name}.service"
    tmr = f"{unit_name}.timer"
    os.makedirs(SYSTEMD_SNAPDIR, exist_ok=True)

    # Script shell (pas de dépendance python) exécuté par l'unité systemd.
    # `%Y%m%d-%H%M%S` est remplacé par systemd avec le timestamp (OnCalendar).
    script_path = install_snap_script()
    svc_content = f"""[Unit]
Description={APP} — snapshot btrfs de {root} ({tag})
DefaultDependencies=no

[Service]
Type=oneshot
ExecStart={script_path} {root} {snapdir} {tag}-%Y%m%d-%H%M%S {keep}
RemainAfterExit=no
User=root
"""
    with open(os.path.join(SYSTEMD_SNAPDIR, svc), "w") as f:
        f.write(svc_content)

    tmr_content = f"""[Unit]
Description={APP} — timer snapshots de {root} ({tag})

[Timer]
OnCalendar={schedule}
Persistent=true
Unit={svc}

[Install]
WantedBy=timers.target
"""
    with open(os.path.join(SYSTEMD_SNAPDIR, tmr), "w") as f:
        f.write(tmr_content)

    rc, out = run(["systemctl", "daemon-reload"], check=False)
    if rc != 0:
        print("WARN: daemon-reload en échec:", out.strip())
    rc, out = run(["systemctl", "enable", "--now", tmr], check=False)
    if rc == 0:
        print(f"Timer activé: {tmr}")
        print(f"  Planning:  {schedule}")
        print(f"  Rétention: {keep} snapshots")
    else:
        print(f"Échec de l'activation du timer: {out.strip()}")
    return tmr


def delete_timer_files(root: str, snapdir: str, tag: str) -> None:
    unit_name = _snap_unit_name(root, snapdir, tag)
    tmr = f"{unit_name}.timer"
    svc = f"{unit_name}.service"
    rc, out = run(["systemctl", "disable", "--now", tmr], check=False)
    rc, _ = run(["rm", "-f", os.path.join(SYSTEMD_SNAPDIR, svc),
                 os.path.join(SYSTEMD_SNAPDIR, tmr)], check=False)
    rc, _ = run(["systemctl", "daemon-reload"], check=False)
    print(f"Timer supprimé: {tmr}")


def list_timers() -> list[dict]:
    rc, out = run(["systemctl", "list-timers", "--all", "--no-pager",
                   "--plain"], check=False)
    if rc != 0:
        return []
    rows = []
    for line in out.splitlines():
        if f"{APP}-" not in line:
            continue
        parts = line.split()
        if len(parts) < 4:
            continue
        rows.append({"next": parts[0], "last": parts[1],
                     "activated": parts[2], "timer": parts[-1]})
    return rows


def auto_snapshot(root: str, snapdir: str, name: str | None, keep: int) -> int:
    """Créer + pruner via le script shell auto-suffisant (même code que
    l'unité systemd).  `name` vide → timestamp de la seconde."""
    name = name or now_stamp()
    script = install_snap_script()
    print(f"[{APP}] auto snapshot: {root} → {os.path.join(root, snapdir, name)}")
    rc, out = run([script, root, snapdir, name, str(keep)])
    if rc != 0:
        print(out.strip())
        return rc
    print(f"[{APP}] création OK: {os.path.join(root, snapdir, name)}")
    return 0


# ---------------------------------------------------------------------------
# Restore
# ---------------------------------------------------------------------------


def restore_snapshot(root: str, snapdir: str, snap_name: str) -> int:
    """Restaurer un snapshot au premier plan, par **deux mv** (CoW).

    Rien n'est copié: ce sont deux renommages de sous-volumes.

      1. mv  <at>        →  <snapdir>/<at>_old-<date-heure>   (sauvegarde)
      2. mv  <snapdir>/<snap>  →  <at>                        (restauration)

    `<at>` est le sous-volume MONTÉ SUR LA RACINE SYSTÈME (le root de
    boot, ex. `@`, `@rootfs`), pas le « default subvolume » (qui peut être
    la racine du FS, sans nom, et n'est donc pas renommable).  Le sous-volume
    courant est conservé comme snapshot daté (`snapshots/<at>_old-<ts>`), et
    le snapshot choisi prend sa place et devient la racine bootable.

    Le montage vivant de / sur @ reste valable: un montage suit le
    sous-volume (sa superblock), pas son nom/dossier.  Les opérations se
    font au niveau racine du FS (montage temporaire), où @ et <snapdir>
    sont frères.  Le snapshot devient rw (ro levé) pour pouvoir être monté
    en écriture et booté.  Le default subvolume est ensuite repointé sur
    @ (le sous-volume restauré, rw) — donc les boots par subvol=@ *et*
    subvolid=<N> atterrissent bien sur la version restaurée.
    """
    # Le sous-volume à échanger est celui MONTÉ SUR LA RACINE SYSTÈME (le
    # root de boot: @, @rootfs, @home, …), pas le « default subvolume » qui
    # peut être la racine du FS (subvolid 5, sans nom → pas renommable).
    at_name = _subvol_of_mount(root, root)
    if not at_name:
        # fallback: sous-volume par défaut s'il a un nom propre
        dv = default_subvol(root)
        at_name = (dv.get("path") or "").strip("/")
    if not at_name:
        print("ERREUR: aucun sous-volume nommé monté sur la racine "
              "système (montage direct sur la racine du FS).  La "
              "restauration par mv nécessite un sous-volume nommé "
              "(ex. @, @rootfs).")
        return 1

    # Le nom de la racine boot ne contient JAMAIS de slash (@, @rootfs…).
    # Si findmnt a renvoyé une forme imbriquée (ex. "snapshots/avant_btop"
    # juste après une restauration partiellement échouée), on ne garde que
    # le dernier segment — sinon le second mv double le chemin
    # (snapshots/snapshots/…) et la restauration échoue.
    at_name = at_name.rstrip("/")
    if "/" in at_name:
        at_name = at_name.split("/")[-1]

    rel = _snaprel(snap_name)
    src = os.path.join(root, snapdir, rel)
    if not os.path.isdir(src):
        print(f"Snapshot introuvable: {src}")
        return 1

    with fs_root_mount(root) as r:
        at = os.path.join(r, at_name)
        snap = os.path.join(r, snapdir, rel)
        stamp = now_stamp()
        old = os.path.join(r, snapdir, f"{at_name}_old-{stamp}")

        if not (is_btrfs_subvolume(at) or is_ro_subvolume(at)):
            print(f"ERREUR: sous-volume par défaut introuvable: {at}")
            return 1
        if not os.path.isdir(snap):
            print(f"ERREUR: snapshot introuvable sous la racine: {snap}")
            return 1
        if os.path.exists(old):
            print(f"ERREUR: {old} existe déjà")
            return 1

        print(f"Restauration: snapshots/{rel} → {at_name} "
              f"(l'ancien {at_name} → {snapdir}/{os.path.basename(old)})")
        # 1) écarte le sous-volume courant dans snapshots/<at>_old-<ts>
        rc, out = run(["mv", at, old], check=False)
        if rc != 0:
            print(f"Échec du déplacement de {at}:\n{out.strip()}")
            return rc
        # Durcit le premier renommage sur disque avant le second: évite un
        # état intermédiaire (le montage vivant suit le sous-volume par ID,
        # mais un synchro rend l'écarte durable avant de remplacer).
        run(["sync"], check=False)

        # le snapshot devenu @ doit pouvoir se monter en rw (boot)
        run(["btrfs", "property", "set", snap, "ro", "false"], check=False)

        # 2) le snapshot prend la place de l'ancien @
        rc, out = run(["mv", snap, at], check=False)
        if rc != 0:
            # rollback: remettre @ à sa place, annuler le renommage
            print("Échec du placement du snapshot — rollback…")
            run(["mv", old, at], check=False)
            print(out.strip())
            return rc

    rc, out = set_default_subvol(root, at_name)
    if rc != 0:
        print(f"(avert.) défaut non repointé: {out.strip()}")
    print(f"Restauration terminée: {rel} est désormais {at_name}.")
    print(f"L'ancien sous-volume est conservé dans "
          f"{snapdir}/{os.path.basename(old)}.")
    print("Redémarrez pour booter sur la version restaurée.")
    return 0


# ---------------------------------------------------------------------------
# Boot entries: GRUB + systemd-boot
# ---------------------------------------------------------------------------


def detect_bootloader() -> str:
    if os.path.isdir("/boot/loader"):          # systemd-boot / efibootmgr
        return "systemd-boot"
    if os.path.isdir("/etc/grub"):
        return "grub"
    return "unknown"


def fver_match(fname: str, kver: str) -> bool:
    """True if `fname` plausibly carries the running kernel version `kver`.

    Robust to distro naming differences (vmlinuz-X.Y, kernel-*.X.Y-BUILD,
    X.Y.current vs X.Y-BUILD.current, …): match on major.minor and, if the
    kver carries one, the trailing build number.
    """
    import re
    m_mm = re.search(r"(\d+\.\d+)", kver)
    if not m_mm:
        return False
    major_minor = m_mm.group(1)
    if major_minor not in fname:
        return False
    m_build = re.search(r"-(\d+)", kver)
    if m_build and m_build.group(1) not in fname:
        return False
    return True


def _e2l(path: str) -> str:
    """Chemin EFI (backslashes, éventuellement en \\EFI\\…) -> path Linux dans le FS."""
    p = path.replace("\\", "/").lstrip("/")
    if p.startswith("boot/"):
        return "/" + p
    if p.startswith("EFI/"):
        return "/boot/" + p
    return "/boot/" + p


def ensure_boot_mounted() -> bool:
    """S'assurer que /boot est monté (Solus: partition EFI non montée par défaut).

    Si /boot n'est pas monté, essaie `clr-boot-manager mount-boot` (Solus),
    puis un `mount /boot` générique. Retourne True si /boot est accessible.
    """
    mounted = False
    rc, _out = run(["findmnt", "-n", "-o", "SOURCE", "/boot"], check=False)
    if rc == 0:
        mounted = True
    else:
        print("( /boot n'est pas monté — tentative de montage… )")
        # 1) Solus
        rc, out = run(["clr-boot-manager", "mount-boot"], check=False)
        if rc == 0:
            print("  ✓ /boot monté via clr-boot-manager.")
        else:
            # 2) fallback générique (fstab)
            rc, out = run(["mount", "/boot"], check=False)
            if rc == 0:
                print("  ✓ /boot monté.")
            else:
                print("  ✗ impossible de monter /boot.")
                if out.strip():
                    print("    " + out.strip())
                rc2, _ = run(["findmnt", "-n", "-o", "SOURCE", "/boot"], check=False)
                mounted = (rc2 == 0)
    # vérification finale
    if not mounted:
        rc3, _ = run(["findmnt", "-n", "-o", "SOURCE", "/boot"], check=False)
        mounted = (rc3 == 0)
    return mounted


def get_boot_info() -> tuple[str, str, str]:
    """Return (root_device, kernel_image, initrd_image) for the current boot.

    The kernel/initrd paths are read from /proc/cmdline (the source of truth
    for what we just booted), then searched on disk in /boot (recursively).
    Fallback: a vmlinuz/initrd in /boot matching the running kernel version.
    """
    rc, out = run(["findmnt", "-n", "-o", "SOURCE", "/"], check=False)
    src = out.strip()
    if not src:
        rc, out = run(["df", "--output=source", "/"], check=False)
        src = out.strip().splitlines()[-1].strip() if out.strip() else "/dev/sda1"
    # findmnt renvoie '/dev/vda2[/@rootfs]' pour btrfs — garder le device seul
    if "[" in src:
        src = src.split("[", 1)[0]

    kver = run(["uname", "-r"])[1].strip()
    kernel = initrd = ""

    # 1) /proc/cmdline (initrd=\\EFI\\…)  -> chemin EFI
    try:
        with open("/proc/cmdline") as f:
            for tok in f.read().split():
                if tok.startswith("initrd=") and not initrd:
                    initrd = _e2l(tok.split("=", 1)[1])
    except OSError:
        pass

    # 2) Scanning récursif de /boot pour le kernel (vmlinuz*/linux-* ou
    #    kernel-* des distros EFI), en fin de chemin
    for base in ("/boot", "/boot/efi"):
        if not os.path.isdir(base):
            continue
        for dp, _dn, fns in os.walk(base):
            for f in fns:
                if not kernel and f.startswith(("vmlinuz", "linux-", "kernel-")) and fver_match(f, kver):
                    kernel = os.path.join(dp, f)
                if not initrd and f.startswith(("initrd", "initramfs", "init")) \
                        and fver_match(f, kver):
                    initrd = os.path.join(dp, f)

    # Fallbacks classiques
    if not kernel:
        kernel = f"vmlinuz-{kver}"
    if not initrd:
        initrd = f"initrd-{kver}"

    # Normaliser en chemin relatif à /boot (les entrées de boot disent
    # 'linux /<chemin-dans-boot>')
    def relimg(p: str) -> str:
        if p.startswith("/boot/"):
            return p[len("/boot/"):]
        return p.lstrip("/")
    return src, relimg(kernel), relimg(initrd)


def _snap_subvol(snapdir: str, snap_name: str) -> str:
    """Return the subvol path of a snapshot, relative to the btrfs root,
    without a leading slash.  The snapdir prefix is never doubled:
      * snap_name = "@snapshots/foo"  ->  "@snapshots/foo"
      * snap_name = "foo"             ->  "<snapdir>/foo"
    """
    name = snap_name.strip("/")
    snapdir_n = snapdir.strip("/")
    if name == snapdir_n or name.startswith(snapdir_n + "/"):
        return name
    return f"{snapdir_n}/{name}"


def _snaprel(snap_name: str) -> str:
    """Return the snapshot name relative to the snapdir, stripping a leading
    snapdir prefix so it is never doubled (e.g. "@snapshots/foo" -> "foo").

    Used to build filesystem paths like <root>/<snapdir>/<snaprel>.
    """
    s = snap_name.strip("/")
    if s.startswith("@snapshots/"):
        return s[len("@snapshots/"):]
    if s.startswith("snapshots/"):
        return s[len("snapshots/"):]
    return s


def current_boot_options() -> str:
    """Return the kernel options of the running system, from /proc/cmdline,
    with the boot-image token removed.

    Used to clone the *working* boot options (root=PARTUUID, rootflags,
    rd.vconsole*, plymouth, rw, …) into snapshot entries so they boot
    exactly like the current system.  Empty string if /proc/cmdline is
    unreadable.
    """
    try:
        with open("/proc/cmdline") as f:
            cmdline = f.read().strip()
    except OSError:
        return ""
    opts = [t for t in cmdline.split()
            if not t.startswith("BOOT_IMAGE=") and not t.startswith("initrd")]
    return " ".join(opts)


def snap_boot_options(snap_subvol: str) -> str:
    """Clone the current system's boot options and point the subvol at the
    snapshot.

    Rewrites a `rootflags=subvol=X` token, a bare `subvol=X` token, or
    appends one if neither is present.  `snap_subvol` is a subvol path
    relative to the btrfs root (no leading slash), e.g. "@snapshots/foo".
    Falls back to a plain `root=... subvol=...` form only if there are no
    options to clone.
    """
    opts = current_boot_options()
    parts = opts.split()
    if not parts:
        return f"subvol={snap_subvol} ro"
    out = []
    replaced = False
    for tok in parts:
        if tok.startswith("rootflags="):
            val = tok[len("rootflags="):]
            if val.startswith("subvol="):
                out.append("rootflags=subvol=" + snap_subvol)
                replaced = True
            else:
                out.append(tok)
        elif tok.startswith("subvol="):
            out.append("subvol=" + snap_subvol)
            replaced = True
        else:
            out.append(tok)
    if not replaced:
        out.append("subvol=" + snap_subvol)
    return " ".join(out)


def add_bootloader_snapshot_entry(root: str, snapdir: str, snap_name: str,
                                  label: str, verbose: bool = True) -> str:
    """Create a systemd-boot loader entry for the given snapshot.

    The entry clones the running system's boot options (see
    snap_boot_options) so it boots with the same root=PARTUUID,
    rootflags, vconsole and console settings — only the subvol differs.
    """
    src, kernel, initrd = get_boot_info()
    subvol = _snap_subvol(snapdir, snap_name)
    options = snap_boot_options(subvol)
    if not options:
        options = f"root={src} subvol=/{subvol} ro"
    entry = f"""title {label}
linux /{kernel}
initrd /{initrd}
options {options}
"""
    os.makedirs("/boot/loader/entries", exist_ok=True)
    fname = f"btrfsmgr-{_snap_subvol(snapdir, snap_name).replace('/', '-')}.conf"
    fpath = os.path.join("/boot/loader/entries", fname)
    with open(fpath, "w") as f:
        f.write(entry)
    if verbose:
        print(f"Entrée systemd-boot créée: {label}")
        print(f"  Fichier: {fpath}")
        print(f"  Options: {options}")
    return fpath


def _entry_subvol(fpath: str) -> str:
    """Extract the subvol from a systemd-boot entry's `options` line.

    Returns the value of `rootflags=subvol=X` (preferred) or `subvol=X`,
    normalised without a leading slash. Empty string if not found.
    """
    try:
        text = open(fpath).read()
    except OSError:
        return ""
    for line in text.splitlines():
        if line.strip().startswith("options"):
            for tok in line.split():
                if tok.startswith("rootflags="):
                    for flag in tok[len("rootflags="):].split(","):
                        if flag.startswith("subvol="):
                            return flag[len("subvol="):].strip("/")
                elif tok.startswith("subvol="):
                    return tok[len("subvol="):].strip("/")
    return ""


def sync_bootloader_entries(root: str, snapdir: str,
                            verbose: bool = True) -> None:
    """Synchroniser les entrées systemd-boot générées par btrfsmgr avec
    les snapshots présents dans <root>/<snapdir>.

    * Ajoute une entrée pour chaque snapshot sans fichier .conf.
    * Supprime le fichier .conf dont le snapshot n'existe plus.
    Le fichier n'est pas touché si il n'est pas géré par btrfsmgr
    (préfixe btrfsmgr-).  Ne s'exécute que si /boot/loader/entries
    existe (systemd-boot); sinon no-op silencieux.
    """
    # 1) monter /boot AVANT de détecter le chargeur (Solus: non monté
    #    par défaut; sans ça /boot/loader n'existe pas et la détection
    #    retombe sur "unknown").
    if not ensure_boot_mounted():
        if verbose:
            print("Synchronisation boot: /boot inaccessible, ignoré.")
        return
    if not detect_bootloader() == "systemd-boot":
        return
    entries_dir = "/boot/loader/entries"
    if not os.path.isdir(entries_dir):
        return
    snaps = list_snapshots(root, snapdir)
    # Nom du fichier → snapshot (chemin relative au root)
    snap_by_file = {}
    for s in snaps:
        subvol = _snap_subvol(snapdir, s["path"])
        fname = f"btrfsmgr-{subvol.replace('/', '-')}.conf"
        snap_by_file[fname] = s

    to_add, to_remove = [], []
    for fname in sorted(os.listdir(entries_dir)):
        if not fname.startswith("btrfsmgr-") or not fname.endswith(".conf"):
            continue
        if fname in snap_by_file:
            continue
        # Pas de snapshot correspondant: vérifier si le fichier pointe vers
        # un snapshot qui existe réellement (cas où le nom du fichier a été
        # renommé ou le snapshot renommé).  Sinon → suppr.
        subvol = _entry_subvol(os.path.join(entries_dir, fname))
        if subvol and os.path.isdir(os.path.join(root, subvol)):
            continue  # snapshot présent, ne pas toucher
        to_remove.append(fname)

    for fname, s in snap_by_file.items():
        if not os.path.exists(os.path.join(entries_dir, fname)):
            to_add.append((fname, s))

    if verbose:
        if not to_add and not to_remove:
            print(f"Synchronisation boot: {len(snap_by_file)} entrée(s) OK, "
                  f"rien à faire.")
        for fname, s in to_add:
            print(f"  + ajout  {fname} ({s['path']})")
        for fname in to_remove:
            print(f"  - retrait {fname}")

    for fname, s in to_add:
        subvol = _snap_subvol(snapdir, s["path"])
        short = subvol[len(snapdir.strip('/')) + 1:] \
            if subvol.startswith(snapdir.strip('/') + "/") else subvol
        label = f"Snapshot {short} ({snapdir.strip('/')})"
        add_bootloader_snapshot_entry(root, snapdir, s["path"], label,
                                      verbose=False)

    for fname in to_remove:
        try:
            os.remove(os.path.join(entries_dir, fname))
        except OSError as e:
            if verbose:
                print(f"  ! échec suppression {fname}: {e}")


# ---------------------------------------------------------------------------
# TUI
# ---------------------------------------------------------------------------

class TUI:
    """A very small curses-free line TUI (no ncurses dependency)."""

    def __init__(self, root: str, snapdir: str = DEFAULT_SNAPDIR):
        self.root = self._validate_root(root)
        self.snapdir = snapdir
        self._refreshed = False

    @staticmethod
    def _validate_root(raw_root: str) -> str:
        """Corriger un root erroné (ex. /snapshots) en remontant à /.

        Le TUI doit toujours opérer sur la RACINE SYSTÈME (/), pas sur un
        sous-dossier comme /snapshots.  Sans cette correction, les chemins
        deviennent doublés (snapshots/snapshots/…) et la restauration échoue.
        """
        r = raw_root.rstrip("/")
        if r == "" or r == "/":
            return "/"
        # Si le root est un sous-dossier connu (snapdir), on remonte à /
        if r in ("/snapshots", "snapshots"):
            print("NOTE: root /snapshots détecté — restauration impossible "
                  "sur un sous-dossier.  Utilisation de / "
                  "(racine système).")
            return "/"
        # Autre cas: on tente de trouver la racine système via le montage
        # Si / est un sous-volume BTRFS, le TUI doit toujours partir de /
        return "/"

    # -- rendering ---------------------------------------------------------
    def render(self, title: str, items: list[str], footer: str = ""):
        sys.stdout.write("\033[2J\033[H")          # clear screen
        sys.stdout.write(f"\033[1;36m{title}\033[0m\n")
        sys.stdout.write("\033[2;33m" + "─" * 60 + "\033[0m\n")
        for i, it in enumerate(items, 1):
            sys.stdout.write(f"  \033[1;32m{i}\033[0m  {it}\n")
        if footer:
            sys.stdout.write("\033[2;33m" + "─" * 60 + "\033[0m\n")
            sys.stdout.write(f"\033[2m{footer}\033[0m\n")
        sys.stdout.flush()

    def ask(self, prompt: str, default: str = "") -> str:
        try:
            val = input(f"{prompt}{'[' + default + ']' if default else ''}: ").strip()
            return val or default
        except EOFError:
            return default

    def ask_yn(self, prompt: str) -> bool:
        try:
            val = input(f"{prompt} [o/n]: ").strip().lower()
        except EOFError:
            return False
        return val in ("o", "oui", "y", "yes")

    def confirm(self, prompt: str) -> None:
        if not self.ask_yn(prompt):
            print("Annulé.")
            sys.exit(0)

    def pause(self):
        try:
            input("\n[Entrée pour continuer…]")
        except EOFError:
            pass

    # -- screens ------------------------------------------------------------
    def main_menu(self):
        self.render(
            f" Unofficial Solus BTRFS Manager — {self.root}",
            [
                "Créer un instantané",
                "Lister les instantanés",
                "Restaurer un instantané",
                "Détruire un instantané",
                "Programmer des instantanés automatiques (systemd)",
                "Supprimer une automatisation (timer systemd)",
                "Rétention : conserver les N plus récents",
                "Infos système (chargeur de boot, timers…)",
                "Menu systemd-boot : afficher/masquer au démarrage",
                "Quitter",
            ],
            "0-10 ou lettre · 10/q quitter",
        )
        return self.ask("Choix", "1")

    def sync_boot(self):
        """Synchronise les entrées systemd-boot avec les snapshots présents."""
        try:
            sync_bootloader_entries(self.root, self.snapdir, verbose=True)
        except Exception as e:
            print(f"Warning: sync boot ignorée: {e}")

    def do_create(self):
        name = self.ask("Nom du snapshot", now_stamp())
        self.confirm(f"Créer un snapshot de {self.root} → {self.snapdir}/{name}")
        create_snapshot(self.root, self.snapdir, name)
        self.sync_boot()
        self.pause()

    def do_list(self):
        snaps = list_snapshots(self.root, self.snapdir)
        if not snaps:
            self.render(f" Snapshots — {self.root}/{self.snapdir}",
                        ["(aucun snapshot)"])
            self.pause()
            return
        items = []
        for s in reversed(snaps):
            items.append(f"{s['path']}   (gen {s['gen']})")
        # add non-snapshot subvolumes for completeness
        others = [s for s in all_subvols(self.root)
                  if s["path"] != self.snapdir and
                  not s["path"].startswith(self.snapdir + "/")]
        if others:
            items.append("")
            items.append("Autres sous-volumes:")
            for s in others:
                items.append(f"      {s['path']}   (gen {s['gen']})")
        self.render(f" Snapshots — {self.root}/{self.snapdir}  "
                    f"({len(snaps)} snapshot(s))", items,
                    "N = plus récent → moins récent")
        self.pause()

    def do_schedule(self):
        tag = self.ask("Nom du plan (ex. daily, weekly)", "daily")
        sched = self.ask("Planning systemd (OnCalendar)", "daily")
        keep = self.ask("Nombre de snapshots à conserver", "7")
        try:
            keep = max(0, int(keep))
        except ValueError:
            print("Nombre invalide"); self.pause(); return
        self.confirm(
            f"Créer un timer systemd '{tag}'\n"
            f"  Planning : {sched}\n"
            f"  Rétention: {keep} snapshot(s)\n"
            f"  Cible    : {self.root}/{self.snapdir}")
        write_timer_files(self.root, self.snapdir, tag, sched, keep)
        self.pause()

    def do_remove_timer(self):
        timers = list_timers()
        if not timers:
            print("Aucun timer btrfsmgr actif."); self.pause(); return
        items = [f"{t['timer']}  (next={t['next']})" for t in timers]
        self.render(f" Supprimer une automatisation — {self.root}", items,
                    "Choisir le timer à supprimer")
        choice = self.ask("Numéro du timer à supprimer", "")
        try:
            idx = int(choice) - 1
        except ValueError:
            print("Choix invalide"); self.pause(); return
        if not 0 <= idx < len(timers):
            print("Choix invalide"); self.pause(); return
        timer = timers[idx]["timer"]
        # Extract tag from unit name: prefix = btrfsmgr-snap-{root}-{snapdir}-
        import re
        def safe(s):
            s = re.sub(r"[^A-Za-z0-9._-]", "-", s.strip("/"))
            return s.strip("-")
        prefix = f"{APP}-snap-{safe(self.root) or 'root'}-{safe(self.snapdir) or 'snap'}-"
        tag = timer.removeprefix(prefix).removesuffix(".timer")
        self.confirm(f"Supprimer l'automatisation '{tag}' ?")
        delete_timer_files(self.root, self.snapdir, tag)
        self.pause()

    def do_restore(self):
        snaps = list_snapshots(self.root, self.snapdir)
        if not snaps:
            print("Aucun snapshot à restaurer."); self.pause(); return
        items = [f"{s['path']}  (gen {s['gen']})"
                 for s in reversed(snaps)]
        self.render(f" Restaurer — {self.root}/{self.snapdir}", items,
                    "Choisir un snapshot à restaurer")
        choice = self.ask("Numéro du snapshot à restaurer", "1")
        snaps_r = list(reversed(snaps))
        try:
            idx = int(choice) - 1
        except ValueError:
            print("Choix invalide"); self.pause(); return
        if not 0 <= idx < len(snaps_r):
            print("Choix invalide"); self.pause(); return
        snap = snaps_r[idx]
        at = _subvol_of_mount(self.root, self.root) \
             or (default_subvol(self.root).get("path") or "").strip("/") \
             or "(racine du FS)"
        self.confirm(
            f"Restaurer {snap['path']} en premier plan par deux mv (CoW) ?\n\n"
            f"  1. mv  {at} → {self.snapdir}/{at}_old-<date-heure>  (conserve l'actuel)\n"
            f"  2. mv  {self.snapdir}/{snap['path']} → {at}  (le snapshot devient @)\n\n"
            f"Le sous-volume courant n'est pas perdu: il reste dans "
            f"{self.snapdir}/.\n"
            f"Redémarrez ensuite pour booter sur la version restaurée.")
        restore_snapshot(self.root, self.snapdir, snap["path"])
        self.sync_boot()
        # Option de redémarrage immédiate pour booter sur la version restaurée
        if self.ask_yn("\nRedémarrer maintenant pour booter sur la version "
                       "restaurée ?"):
            run(["systemctl", "reboot"], check=False)
            # systemctl reboot prend le relais ; on termine proprement
            try:
                time.sleep(2)
            except Exception:
                pass
        self.pause()

    def do_delete(self):
        snaps = list_snapshots(self.root, self.snapdir)
        if not snaps:
            print("Aucun snapshot à supprimer."); self.pause(); return
        snaps_r = list(reversed(snaps))
        items = [f"{s['path']}  (gen {s['gen']})"
                 for s in snaps_r]
        self.render(f" Supprimer — {self.root}/{self.snapdir}", items,
                    "un numéro, plusieurs (ex. 1,3,5), ou 'tous'")
        choice = self.ask("Numéro(s) du/des snapshot(s) à supprimer "
                          "(ex. 3, ou 1,2,4, ou 'tous')", "")
        sel = self.parse_selections(choice, len(snaps_r))
        if not sel:
            print("Aucune sélection valide."); self.pause(); return
        targets = [snaps_r[i] for i in sel]
        for t in targets:
            print(f"  → {t['full']}")
        self.confirm(f"Supprimer DÉFINITIVEMENT {len(targets)} "
                     f"instantané(s) ?")
        ok = 0
        for t in targets:
            rc, out = delete_subvol(t["full"])
            if rc == 0:
                ok += 1
                print(f"  Supprimé: {t['full']}")
            else:
                print(f"  Échec: {t['full']} — {out.strip()}")
        print(f"{ok}/{len(targets)} supprimé(s).")
        self.sync_boot()
        self.pause()

    @staticmethod
    def parse_selections(choice: str, count: int) -> list[int]:
        """Parse a selection string into 0-based indices (dedup, in order).

        Accepts "3", "1,3,5", "1 3 5", "1;3;5", "all"/"tous"/"*" (every
        item).  Out-of-range numbers are skipped.  Returns [] if nothing
        valid.
        """
        c = choice.strip()
        if c.lower() in ("all", "tous", "tout", "*"):
            return list(range(count))
        toks = re.split(r"[,\s;]+", c)
        seen: list[int] = []
        for tk in toks:
            if not tk:
                continue
            if not tk.isdigit():
                continue
            i = int(tk) - 1
            if 0 <= i < count and i not in seen:
                seen.append(i)
        return seen

    def do_boot_menu(self):
        """Afficher/masquer le menu systemd-boot au démarrage.

        `clr-boot-manager set-timeout N` : N secondes d'attente sur le menu
        au démarrage.  0 = le menu ne s'affiche pas (boot direct sur la
        première entrée).  Sur Solus (systemd-boot + clr-boot-manager).
        """
        print("Menu systemd-boot au démarrage")
        print("  clr-boot-manager set-timeout <secondes>")
        print("    N > 0 : le menu est affiché pendant N secondes")
        print("    0     : le menu est masqué (boot direct)")
        secs = self.ask("Nombre de secondes d'affichage (0 = masquer le menu)", "5")
        try:
            secs = int(secs)
        except ValueError:
            print("Nombre invalide"); self.pause(); return
        if secs < 0:
            print("Nombre invalide (0 ou plus)"); self.pause(); return
        self.confirm(f"Appliquer: clr-boot-manager set-timeout {secs}")
        rc, out = run(["clr-boot-manager", "set-timeout", str(secs)], check=False)
        if rc == 0:
            print(f"  ✓ Done (timeout={secs}s).")
        else:
            print(f"  ✗ Échec: {out.strip()}")
        self.pause()

    def do_retention(self):
        keep = self.ask("Conserver les N plus récents snapshots (0=tous)", "7")
        try:
            keep = max(0, int(keep))
        except ValueError:
            print("Nombre invalide"); self.pause(); return
        snaps = list_snapshots(self.root, self.snapdir)
        to_del = snaps[:-keep] if len(snaps) > keep else []
        if not to_del:
            print("Aucun snapshot à supprimer (déjà ≤", keep, ").")
            self.pause()
            return
        self.render(f" Rétention — conserver {keep}, supprimer {len(to_del)}",
                    [s['path'] for s in to_del],
                    "Les snapshots listés seront SUPPRIMÉS.")
        self.confirm("Confirmer la suppression ?")
        prune_snapshots(self.root, self.snapdir, keep)
        self.pause()

    def do_info(self):
        bl = detect_bootloader()
        dv = default_subvol(self.root)
        timers = list_timers()
        snaps = list_snapshots(self.root, self.snapdir)
        items = [
            f"Chargeur de boot      : {bl}",
            f"Subvolume par défaut  : {dv['full'] or '?'}",
            f"Snapshots existants   : {len(snaps)}",
            f"Timers {APP} actifs  : {len(timers)}",
        ]
        for t in timers:
            items.append(f"  timer: {t['timer']}  next={t['next']}")
        fs_rc, fs_out = run(["df", "-h", self.root], check=False)
        items.append("Espace disque:")
        items.extend("  " + ln for ln in fs_out.splitlines())
        self.render(" Infos système", items)
        self.pause()

    def run(self):
        if not is_root():
            print("ERREUR: exécuter en root:  sudo btrfsmgr")
            sys.exit(1)
        if not btrfs_available():
            print("ERREUR: 'btrfs' introuvable. Installez btrfs-progs.")
            sys.exit(1)
        # Montage on-demand du sous-volume snapshots + migrations
        # (RO→RW, retrait de l'ancienne ligne fstab).  Le dossier n'est
        # monté qu'ici, à l'ouverture de l'outil — jamais au boot.
        try:
            ensure_snapdir(self.root, self.snapdir)
        except Exception as e:
            print(f"Warning: ensure_snapdir ignorée: {e}")
        while True:
            try:
                ch = self.main_menu()
            except (KeyboardInterrupt, EOFError):
                print("\nAu revoir.")
                break
            if ch in ("0", "10", "q", "Q"):
                print("Au revoir.")
                break
            if ch in ("1", "s"):
                self.do_create()
            elif ch in ("2", "l", "L"):
                self.do_list()
            elif ch in ("3", "r", "R"):
                self.do_restore()
            elif ch in ("4", "a", "A", "d", "D"):
                self.do_delete()
            elif ch in ("5", "p", "P"):
                self.do_schedule()
            elif ch in ("6", "x", "X"):
                self.do_remove_timer()
            elif ch in ("7",):
                self.do_retention()
            elif ch in ("8", "i", "I"):
                self.do_info()
            elif ch in ("9", "m", "M"):
                self.do_boot_menu()
            else:
                print("Choix invalide.")
                self.pause()


# ---------------------------------------------------------------------------
# CLI (headless, pour systemd + usage script)
# ---------------------------------------------------------------------------


def cli_auto(args) -> int:
    # Called from the systemd service.
    return auto_snapshot(args.root, args.snapdir, args.name, args.keep)


def cli(args) -> int:
    if args.cmd == "auto":
        return cli_auto(args)
    if args.cmd == "update":
        return cli_update(args)
    if args.cmd == "uninstall":
        return cli_uninstall(args)
    if args.cmd == "ensure":
        if os.geteuid() != 0:
            print("ERREUR: exécuter en root (sudo)")
            return 1
        ensure_snapdir(args.root, args.snapdir)
        print(f"OK: {os.path.join(args.root, args.snapdir)} "
              f"(snapshots au niveau racine du FS, montés)")
        return 0
    if args.cmd == "sync":
        # Synchronisation seule des entrées systemd-boot avec les
        # snapshots présents (ajoute les manquantes, retire les
        # orphelines).  No-op silencieux hors systemd-boot.
        sync_bootloader_entries(args.root, args.snapdir, verbose=True)
        return 0
    if args.cmd == "delete-snap":
        if os.geteuid() != 0:
            print("ERREUR: exécuter en root (sudo)")
            return 1
        names = [a for a in (args.snaps or []) if a]
        snaps = {s["path"]: s for s in list_snapshots(args.root,
                                                      args.snapdir)}
        ok = True
        for n in names:
            n = _snaprel(n)
            s = snaps.get(n)
            if s is None:
                print(f"Snapshot introuvable: {n}")
                ok = False
                continue
            rc, out = delete_subvol(s["full"])
            if rc == 0:
                print(f"Supprimé: {s['full']}")
            else:
                print(f"Échec: {s['full']} — {out.strip()}")
                ok = False
        sync_bootloader_entries(args.root, args.snapdir, verbose=False)
        return 0 if ok else 1
    # TUI (défaut); synchronise les entrées systemd-boot au démarrage.
    try:
        sync_bootloader_entries(args.root, args.snapdir, verbose=True)
    except Exception as e:
        print(f"Warning: synchronisation boot ignorée: {e}")
    tui = TUI(args.root, args.snapdir)
    tui.run()
    return 0


def cli_update(args) -> int:
    """Relancer install.sh avec le répertoire source (si fourni)."""
    src = args.src or os.path.dirname(os.path.abspath(sys.argv[0]))
    script = os.path.join(src, "install.sh")
    if not os.path.exists(script):
        print(f"ERREUR: {script} introuvable.")
        print(f"  Utiliser: btrfsmgr update /chemin/vers/le/depot")
        return 1
    print("Mise à jour — relance de install.sh:")
    r = subprocess.run(["bash", script])
    return r.returncode


def cli_uninstall(args) -> int:
    """Retirer btrfsmgr: units systemd, entrées de boot, fstab, binaire.

    Le sous-volume `snapshots` (et ses snapshots) est CONSERVÉ — à
    supprimer manuellement si on ne s'en sert plus:
      btrfsmgr est retiré mais les données restent accessibles via
      un montage temporaire:  mount -o subvol=/snapshots <dev> <dir>
    """
    if os.geteuid() != 0:
        print("ERREUR: lancer en root (sudo)")
        return 1
    units = sorted(
        u for u in os.listdir("/etc/systemd/system")
        if u.startswith("btrfsmgr-") and u.endswith((".service", ".timer"))
    )
    for u in units:
        path = f"/etc/systemd/system/{u}"
        subprocess.run(["systemctl", "stop", u], capture_output=True)
        try:
            os.remove(path)
        except OSError as e:
            print(f"  ! {path}: {e}")
        print(f"  - unit retirée: {u}")
    subprocess.run(["systemctl", "daemon-reload"], capture_output=True)

    for path in ("/etc/grub.d/40_btrfsmgr_snapshots",
                 "/boot/loader/entries/btrfsmgr-snapshots.conf"):
        if os.path.exists(path):
            os.remove(path)
            print(f"  - entrée GRUB retirée: {path}")
    edir = "/boot/loader/entries"
    for f in (os.listdir(edir) if os.path.isdir(edir) else []):
        if f.startswith("btrfsmgr-") and f.endswith(".conf"):
            os.remove(os.path.join(edir, f))
            print(f"  - entry systemd-boot retirée: {f}")

    # Démonter et retirer la ligne fstab du dossier des snapshots
    snapdir_full = os.path.join(args.root, args.snapdir)
    rc, _ = run(["findmnt", "-n", snapdir_full], check=False)
    if rc == 0:
        subprocess.run(["umount", snapdir_full], capture_output=True)
        print(f"  - démonté: {snapdir_full}")
    fstab = "/etc/fstab"
    if os.path.exists(fstab):
        with open(fstab) as f:
            lines = f.readlines()
        kept = [ln for ln in lines if snapdir_full not in ln]
        if len(kept) != len(lines):
            with open(fstab, "w") as f:
                f.writelines(kept)
            print(f"  - fstab: ligne {snapdir_full} retirée")

    for path in ("/bin/btrfsmgr", "/usr/local/bin/btrfsmgr", SNAP_SCRIPT):
        if os.path.lexists(path):
            os.remove(path)
            print(f"  - binaire retiré: {path}")

    # Icône menu (.desktop)
    if os.path.lexists(DESKTOP_FILE):
        os.remove(DESKTOP_FILE)
        print(f"  - icône menu retirée: {DESKTOP_FILE}")

    print(f"\nDésinstallation terminée. Le sous-volume snapshots "
          f"({os.path.join(args.root, args.snapdir)}) a été CONSERVÉ "
          f"avec son contenu.")
    print(f"Pour le supprimer définitivement (NE PAS supprimer @ !):")
    print(f"  DEV=$(findmnt -n -o SOURCE {args.root} | cut -d'[' -f1)")
    print(f"  M=$(mktemp -d); mount -o subvol=/,rw $DEV $M")
    print(f"  btrfs subvolume delete $M/{args.snapdir}")
    print(f"  umount $M && rmdir $M")
    return 0


def main():
    ap = argparse.ArgumentParser(prog=APP,
                                 description="Gestionnaire TUI BTRFS "
                                             "(snapshots, timers, boot)")
    ap.add_argument("args_", nargs="*",
                    help="[tui|auto|update|uninstall|ensure|sync|delete-snap] "
                         "[CHEMIN du sous-volume BTRFS (défaut: /)] "
                         "[SNAP1 SNAP2 ...  pour delete-snap]")
    ap.add_argument("--snapdir", default=DEFAULT_SNAPDIR,
                    help="Dossier des snapshots, au même niveau que @ et @home "
                         f"(défaut: {DEFAULT_SNAPDIR})")
    ap.add_argument("--name", default=None,
                    help="Nom du snapshot (auto seulement)")
    ap.add_argument("--keep", type=int, default=7,
                    help="Nombre de snapshots à conserver (auto seulement)")
    ap.add_argument("--src", default=None,
                    help="Dépôt source (update: dossier contenant install.sh)")
    args = ap.parse_args()

    # Interprétation tolérante: "btrfsmgr", "btrfsmgr /", "btrfsmgr auto /",
    # "btrfsmgr sync /", "btrfsmgr delete-snap / snap-a snap-b"
    _cmds = ("tui", "auto", "update", "uninstall", "ensure", "sync",
             "delete-snap")
    cmd, root, snaps = "tui", "/", []
    _seen_root = False
    for a in args.args_:
        if a in _cmds:
            cmd = a
        elif cmd == "delete-snap" and not _seen_root:
            root = a
            _seen_root = True
        elif cmd == "delete-snap" and _seen_root:
            snaps.append(a)
        else:
            root = a
    if len(args.args_) > 10:
        ap.error("trop d'arguments")
    args.root = os.path.abspath(root)
    args.cmd = cmd
    args.snaps = snaps
    sys.exit(cli(args))


if __name__ == "__main__":
    main()
