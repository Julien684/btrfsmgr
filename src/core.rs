//! Cœur fonctionnel de btrfsmgr (portage fidèle du cœur Python).
//!
//! Toutes les opérations passent par `btrfs` / `findmnt` / `systemctl` via le
//! helper `runv` ; les messages utilisateur sont émis par `say` (capturable
//! par la GUI et par les tests).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::i18n::t;

// ---------------------------------------------------------------------------
// Constantes
// ---------------------------------------------------------------------------

pub const SYSTEMD_SNAPDIR: &str = "/etc/systemd/system";
pub const SNAPSHOTS_DIR: &str = "/var/lib/btrfsmgr";
pub const SNAPSHOTS_STATE: &str = "/var/lib/btrfsmgr/snapshots.state";
pub const GRUB_SNAPDIR_FILE: &str = "/etc/grub.d/40_btrfsmgr_snapshots";
pub const BOOTXMD_SNAPDIR: &str = "/boot/loader/entries/btrfsmgr-snapshots.conf";
pub const SNAP_SCRIPT: &str = "/usr/local/bin/btrfsmgr-snap.sh";
pub const DESKTOP_FILE: &str = "/usr/share/applications/btrfsmgr.desktop";
pub const AUTO_NOTIF_DESKTOP: &str = "btrfsmgr.desktop";

pub const APP: &str = "btrfsmgr";
pub const VERSION: &str = "1.0.15";
pub const PROJECT_REPO: &str = "Linuxon/btrfsmgr";
pub const PROJECT_REPO_URL: &str = "https://depoa.julien68.fr/Linuxon/btrfsmgr.git";
pub const PROJECT_API: &str = "https://depoa.julien68.fr/api/v1/repos/Linuxon/btrfsmgr";

pub const DEFAULT_SNAPDIR: &str = "snapshots";

/// Vrai si c'est CE processus qui a monté /boot (et doit donc le démonter en
/// sortie). Si /boot était déjà monté au lancement, on n'y touche pas.
static BOOT_MOUNTED_BY_US: AtomicBool = AtomicBool::new(false);

/// Avertissement noyau (même texte que le Python, clé de traduction incluse).
pub const KERNEL_MISMATCH_WARNING: &str = "⚠ Différence de noyau détectée !\n  Noyau en cours        : {cur}\n  Noyau de l'instantané : {snaps}\n\nLe noyau de démarrage vit dans la partition EFI (/boot), pas dans l'instantané (qui ne conserve que les modules usr/lib/modules). Après restauration, la racine restaurée n'aura pas les modules du noyau {cur} qui démarrera : le démarrage peut échouer ou être dégradé.";

// ---------------------------------------------------------------------------
// Sortie capturable
// ---------------------------------------------------------------------------

thread_local! {
    static CAPTURE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// Écrire une chaîne (stdout, ou tampon si `capture` est actif).
/// Joindre `root` et un sous-chemin SANS double slash (racine `/`).
pub fn join_path(root: &str, sub: &str) -> String {
    let r = root.trim_end_matches('/');
    let s = sub.trim_start_matches('/');
    if r.is_empty() {
        format!("/{s}")
    } else {
        format!("{r}/{s}")
    }
}

pub fn say(s: &str) {
    CAPTURE.with(|c| {
        let mut b = c.borrow_mut();
        if let Some(v) = b.as_mut() {
            v.push(s.to_string());
        } else {
            print!("{s}");
            let _ = std::io::stdout().flush();
        }
    });
}

/// Exécuter `f` en capturant tout ce que `say` écrirait.
pub fn capture<F: FnOnce() -> T, T>(f: F) -> (T, String) {
    CAPTURE.with(|c| *c.borrow_mut() = Some(Vec::new()));
    let r = f();
    let out = CAPTURE
        .with(|c| c.borrow_mut().take())
        .unwrap_or_default()
        .join("");
    (r, out)
}

/// `say` avec retour à la ligne.
#[macro_export]
macro_rules! sayln {
    ($($arg:tt)*) => { $crate::core::say(&format!("{}\n", format!($($arg)*))) };
}

pub fn sargs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

// ---------------------------------------------------------------------------
// Helpers système
// ---------------------------------------------------------------------------

pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Exécuter une commande ; renvoie (code, stdout+stderr).
pub fn runv(cmd: &[String], check: bool) -> (i32, String) {
    if cmd.is_empty() {
        return (1, String::new());
    }
    let out = Command::new(&cmd[0]).args(&cmd[1..]).output();
    match out {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            sayln!("ERREUR: commande introuvable: {}", cmd[0]);
            (127, String::new())
        }
        Err(e) => {
            sayln!("ERREUR: {e}");
            (1, String::new())
        }
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            let rc = o.status.code().unwrap_or(-1);
            if check && rc != 0 {
                sayln!("ERREUR (rc={rc}): {}", cmd.join(" "));
                sayln!("{}", s.trim());
            }
            (rc, s)
        }
    }
}

pub fn btrfs_available() -> bool {
    which("btrfs").is_some()
}

pub fn which(bin: &str) -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    for d in path.split(':').filter(|d| !d.is_empty()) {
        let p = Path::new(d).join(bin);
        if p.is_file() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

pub fn systemd_available() -> bool {
    runv(&sargs(&["systemctl", "is-system-running"]), false).0 == 0
}

pub fn read_state() -> serde_json::Value {
    match fs::read_to_string(SNAPSHOTS_STATE) {
        Ok(s) => serde_json::from_str(&s).unwrap_or(serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    }
}

pub fn write_state(data: &serde_json::Value) {
    let _ = fs::create_dir_all(SNAPSHOTS_DIR);
    if let Ok(s) = serde_json::to_string_pretty(data) {
        let _ = fs::write(SNAPSHOTS_STATE, s);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(SNAPSHOTS_STATE, fs::Permissions::from_mode(0o644));
        }
    }
}

pub fn now_stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

pub fn human_size(n: f64) -> String {
    let mut v = n;
    for unit in ["o", "K", "M", "G", "T", "P"] {
        if v.abs() < 1024.0 {
            return format!("{v:.1}{unit}");
        }
        v /= 1024.0;
    }
    format!("{v:.1}E")
}

fn temp_dir(prefix: &str) -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!("{prefix}{pid}.{nanos}"));
    let _ = fs::create_dir_all(&p);
    p.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// Structures
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub generation: i64,
    pub path: String,
    pub full: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Timer {
    pub next: String,
    pub last: String,
    pub activated: String,
    pub timer: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BootEntry {
    pub file: String,
    pub title: String,
    pub subvol: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct KernelInfo {
    pub current: String,
    pub snapshot_kernels: Vec<String>,
    pub mismatch: bool,
}

// ---------------------------------------------------------------------------
// Requêtes BTRFS
// ---------------------------------------------------------------------------

fn parse_subvol_list(out: &str, root: &str) -> Vec<Snapshot> {
    let mut subs = Vec::new();
    for line in out.lines() {
        let Some((_, rest)) = line.split_once(" path ") else {
            continue;
        };
        let parts: Vec<&str> = line.split_whitespace().collect();
        let Some(gen) = parts.get(1).and_then(|s| s.parse::<i64>().ok()) else {
            continue;
        };
        let path = rest.trim().to_string();
        subs.push(Snapshot {
            generation: gen,
            full: format!("{}/{}", root.trim_end_matches('/'), path),
            path,
        });
    }
    subs.sort_by_key(|s| s.generation);
    subs
}

pub fn btrfs_subvols(root: &str) -> Vec<Snapshot> {
    let (rc, out) = runv(
        &sargs(&["btrfs", "subvolume", "list", "-s", root]),
        false,
    );
    if rc != 0 {
        return Vec::new();
    }
    parse_subvol_list(&out, root)
}

pub fn all_subvols(root: &str) -> Vec<Snapshot> {
    let (rc, out) = runv(&sargs(&["btrfs", "subvolume", "list", root]), false);
    if rc != 0 {
        return Vec::new();
    }
    parse_subvol_list(&out, root)
}

pub fn move_subvol(src: &str, dst: &str) -> (i32, String) {
    let (rc, out) = runv(
        &sargs(&["btrfs", "subvolume", "snapshot", src, dst]),
        false,
    );
    if rc != 0 {
        return (rc, out);
    }
    runv(&sargs(&["btrfs", "subvolume", "delete", src]), false)
}

pub fn is_ro_subvolume(path: &str) -> bool {
    if !is_btrfs_subvolume(path) {
        return false;
    }
    let (rc, out) = runv(&sargs(&["btrfs", "property", "get", path, "ro"]), false);
    if rc == 0 && out.contains("ro=") {
        return out.contains("ro=true");
    }
    let (rc, out) = runv(&sargs(&["btrfs", "subvolume", "show", path]), false);
    if rc == 0 {
        for line in out.lines() {
            let s = line.trim();
            if let Some(v) = s.strip_prefix("Snapshot:") {
                return v.contains("yes");
            }
            if s.starts_with("Flags:") && s.split_whitespace().any(|w| w == "ro") {
                return true;
            }
        }
    }
    false
}

pub fn subvols_by_id(root: &str) -> BTreeMap<String, String> {
    let (rc, out) = runv(&sargs(&["btrfs", "subvolume", "list", root]), false);
    let mut m = BTreeMap::new();
    if rc == 0 {
        for line in out.lines() {
            let Some((_, rest)) = line.split_once(" path ") else {
                continue;
            };
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            let p = rest.trim().trim_start_matches('/');
            if p.is_empty() {
                continue;
            }
            m.insert(parts[1].to_string(), p.to_string());
        }
    }
    m
}

/// Sous-volume BTRFS monté sur `mount` (nom sans slash), None sinon.
pub fn subvol_of_mount(mount: &str, root: &str) -> Option<String> {
    let (rc, out) = runv(
        &sargs(&["findmnt", "-n", "-o", "SOURCE,OPTIONS", mount]),
        false,
    );
    if rc != 0 {
        return None;
    }
    let first = out.lines().next().unwrap_or("").trim().to_string();
    // SOURCE `dev[/@]` → @
    if let (Some(a), Some(b)) = (first.find('['), first.rfind(']')) {
        if b > a + 1 {
            let name = first[a + 1..b].trim_matches('/').to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    let mut subvolid: Option<String> = None;
    for tok in first.split_whitespace() {
        if let Some(v) = tok.strip_prefix("subvol=") {
            return Some(v.trim_matches('/').to_string());
        }
        if let Some(v) = tok.strip_prefix("subvolid=") {
            subvolid = Some(v.to_string());
        }
    }
    if let Some(id) = subvolid {
        let table = subvols_by_id(root);
        if let Some(v) = table.get(&id) {
            return Some(v.clone());
        }
        if let Ok(rm) = RootMount::new(root) {
            let table = subvols_by_id(&rm.path);
            return table.get(&id).cloned();
        }
    }
    None
}

pub fn is_subvolume_rw(path: &str) -> bool {
    if !Path::new(path).exists() {
        return false;
    }
    let (rc, out) = runv(&sargs(&["findmnt", "-n", "-o", "OPTIONS", path]), false);
    if rc == 0 && !out.trim().is_empty() {
        let first = out.lines().next().unwrap_or("");
        let is_ro = first
            .split(',')
            .any(|o| o.trim() == "ro")
            || first.starts_with("ro,")
            || first == "ro";
        if is_ro {
            return false;
        }
    }
    let (rc, out) = runv(&sargs(&["btrfs", "subvolume", "show", "-s", path]), false);
    if rc == 0 {
        for line in out.lines() {
            let s = line.trim();
            if let Some(v) = s.strip_prefix("Snapshot:") {
                return !v.contains("yes");
            }
            if s.starts_with("Flags:") && s.split_whitespace().any(|w| w == "ro") {
                return false;
            }
        }
    }
    true
}

pub fn subvol_name(path: &str) -> Option<String> {
    let (rc, out) = runv(&sargs(&["btrfs", "subvolume", "show", path]), false);
    if rc != 0 {
        return None;
    }
    for line in out.lines() {
        let s = line.trim();
        if let Some(v) = s.strip_prefix("Name:") {
            return Some(v.trim().to_string());
        }
    }
    None
}

pub fn list_subvols(mount: &str) -> Vec<String> {
    let (_rc, out) = runv(&sargs(&["btrfs", "subvolume", "list", mount]), false);
    out.lines()
        .filter_map(|l| l.split_once(" path ").map(|(_, r)| r.trim().trim_start_matches('/').to_string()))
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn is_btrfs_subvolume(path: &str) -> bool {
    if !Path::new(path).exists() {
        return false;
    }
    runv(&sargs(&["btrfs", "subvolume", "show", path]), false).0 == 0
}

pub fn btrfs_dev_of(path: &str) -> String {
    let (_rc, out) = runv(&sargs(&["findmnt", "-n", "-o", "SOURCE", path]), false);
    let first = out.lines().next().unwrap_or("").trim().to_string();
    first.split('[').next().unwrap_or("").to_string()
}

/// Montage temporaire de la RACINE du FS (subvol=/), libéré au Drop.
pub struct RootMount {
    pub path: String,
    mounted: bool,
}

impl RootMount {
    pub fn new(root: &str) -> Result<RootMount, String> {
        let (_rc, out) = runv(&sargs(&["findmnt", "-n", "-o", "SOURCE", root]), false);
        let src = out.lines().next().unwrap_or("").trim().to_string();
        if !src.contains('[') {
            return Ok(RootMount {
                path: root.to_string(),
                mounted: false,
            });
        }
        let dev = src.split('[').next().unwrap_or("").to_string();
        let mnt = temp_dir("btrfsmgr-root.");
        let (rc, out) = runv(&sargs(&["mount", "-o", "subvol=/,rw", &dev, &mnt]), false);
        if rc != 0 {
            let _ = fs::remove_dir_all(&mnt);
            return Err(format!(
                "ERREUR: montage temporaire de la racine du FS impossible:\n{}",
                out.trim()
            ));
        }
        Ok(RootMount {
            path: mnt,
            mounted: true,
        })
    }
}

impl Drop for RootMount {
    fn drop(&mut self) {
        if self.mounted {
            let p = self.path.clone();
            runv(&sargs(&["umount", &p]), false);
            let _ = fs::remove_dir_all(&p);
        }
    }
}

/// Sous-volume par défaut du FS (gère les formats v6.x et v7.x).
pub fn default_subvol(root: &str) -> Snapshot {
    let (rc, out) = runv(&sargs(&["btrfs", "subvolume", "get-default", root]), false);
    if rc != 0 {
        return Snapshot {
            generation: 0,
            path: String::new(),
            full: root.to_string(),
        };
    }
    // ID 259 (path @) gen 150   /   ID 259 gen 150 top level 5 path @
    let id = regex_find(&out, "ID");
    let path = regex_find(&out, "path");
    let gen = regex_find(&out, "gen").and_then(|g| g.parse::<i64>().ok()).unwrap_or(0);
    if id.is_some() && path.is_some() {
        let vuelta = path.unwrap();
        return Snapshot {
            generation: gen,
            full: format!("{}/{}", root.trim_end_matches('/'), vuelta),
            path: vuelta,
        };
    }
    Snapshot {
        generation: gen,
        path: String::new(),
        full: root.to_string(),
    }
}

fn regex_find(text: &str, key: &str) -> Option<String> {
    // Équivalent simple de re.search(r"<key>\s+(\S+)")
    let mut rest = text;
    while let Some(pos) = rest.find(key) {
        let after = &rest[pos + key.len()..];
        if after.starts_with(char::is_whitespace) {
            return after.split_whitespace().next().map(|s| s.to_string());
        }
        rest = &rest[pos + key.len()..];
    }
    None
}

pub fn boot_subvol(root: &str) -> Option<String> {
    if let Some(mnt) = subvol_of_mount(root, root) {
        if !mnt.contains('/') {
            return Some(mnt.trim_end_matches('/').to_string());
        }
    }
    let std = standard_entry_subvol();
    if !std.is_empty() && !std.contains('/') {
        return Some(std.trim_end_matches('/').to_string());
    }
    let dv = default_subvol(root);
    let p = dv.path.trim_matches('/').to_string();
    if !p.is_empty() {
        return Some(p);
    }
    None
}

pub fn standard_entry_subvol() -> String {
    let mut dirs: Vec<String> = Vec::new();
    for d in [
        "/boot/loader/entries",
        "/efi/loader/entries",
        "/boot/efi/loader/entries",
    ] {
        if Path::new(d).is_dir() && !dirs.iter().any(|x| x == d) {
            dirs.push(d.to_string());
        }
    }
    for d in dirs {
        let mut names: Vec<String> = match fs::read_dir(&d) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => continue,
        };
        names.sort();
        for fname in names {
            if !(fname.ends_with(".conf") || fname.ends_with(".cfg")) {
                continue;
            }
            if fname.starts_with("btrfsmgr-") || fname.starts_with("00-") {
                continue;
            }
            let sv = entry_subvol(&format!("{d}/{fname}"));
            if !sv.is_empty() {
                return sv;
            }
        }
    }
    String::new()
}

pub fn make_snapshot(source: &str, target: &str) -> (i32, String) {
    let parent = Path::new(target)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !parent.is_empty() && !is_subvolume_rw(&parent) {
        return (
            1,
            format!(
                "le dossier des snapshots {parent} n'est pas un sous-volume BTRFS RW (ensure_snapdir)"
            ),
        );
    }
    runv(&sargs(&["btrfs", "subvolume", "snapshot", source, target]), true)
}

pub fn delete_subvol(path: &str) -> (i32, String) {
    runv(&sargs(&["btrfs", "subvolume", "delete", path]), true)
}

pub fn subvol_gen_by_path(root: &str, path: &str) -> Option<i64> {
    let (rc, out) = runv(&sargs(&["btrfs", "subvolume", "list", root]), false);
    if rc != 0 {
        return None;
    }
    for line in out.lines() {
        let Some((_, rest)) = line.split_once(" path ") else {
            continue;
        };
        if rest.trim() == path {
            return line.split_whitespace().nth(1).and_then(|s| s.parse().ok());
        }
    }
    None
}

/// Assurer que le dossier des snapshots existe et est opérationnel.
pub fn ensure_snapdir(root: &str, snapdir: &str) -> Result<String, String> {
    let vis = format!("{}/{}", root.trim_end_matches('/'), snapdir);
    let dev = btrfs_dev_of(root);
    if dev.is_empty() {
        return Err(format!("ERREUR: device BTRFS introuvable pour {root}"));
    }

    let mounted_ok = {
        let (rc, _) = runv(&sargs(&["findmnt", "-n", &vis]), false);
        rc == 0 && subvol_of_mount(&vis, root).as_deref() == Some(snapdir)
    };

    if !mounted_ok {
        let (rc, _) = runv(&sargs(&["findmnt", "-n", &vis]), false);
        if rc == 0 {
            runv(&sargs(&["umount", &vis]), false);
        }
        let rm = RootMount::new(root)?;
        let r = rm.path.clone();
        let p = format!("{}/{}", r.trim_end_matches('/'), snapdir);

        // résidus de migrations interrompues
        if let Ok(rd) = fs::read_dir(&r) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(".btrfsmgr-mig.") {
                    let full = format!("{}/{}", r.trim_end_matches('/'), name);
                    runv(&sargs(&["btrfs", "subvolume", "delete", &full]), false);
                }
            }
        }

        if is_ro_subvolume(&p) {
            let children: Vec<String> = list_subvols(&r)
                .into_iter()
                .filter_map(|c| c.strip_prefix(&format!("{snapdir}/")).map(|x| x.to_string()))
                .collect();
            let tmp = format!("{}/.btrfsmgr-mig.{}", r.trim_end_matches('/'), std::process::id());
            let (rc, out) = runv(&sargs(&["btrfs", "subvolume", "create", &tmp]), false);
            if rc != 0 {
                return Err(format!("ERREUR: sous-volume temporaire:\n{}", out.trim()));
            }
            for c in &children {
                let (rc, out) = move_subvol(&format!("{p}/{c}"), &format!("{tmp}/{c}"));
                if rc != 0 {
                    sayln!("  ! migration {}: {}", c, out.trim());
                }
            }
            runv(&sargs(&["btrfs", "subvolume", "delete", &p]), false);
            let (rc, out) = runv(&sargs(&["btrfs", "subvolume", "create", &p]), false);
            if rc != 0 {
                return Err(format!("ERREUR: recréation de {p}:\n{}", out.trim()));
            }
            for c in &children {
                move_subvol(&format!("{tmp}/{c}"), &format!("{p}/{c}"));
            }
            runv(&sargs(&["btrfs", "subvolume", "delete", &tmp]), false);
            sayln!(
                "  + {snapdir} (racine du FS) remplacé: snapshot RO → sous-volume RW ({} snapshots préservés)",
                children.len()
            );
        } else if is_btrfs_subvolume(&p) {
            // déjà au bon endroit
        } else {
            let _ = fs::remove_dir_all(&p);
            let (rc, out) = runv(&sargs(&["btrfs", "subvolume", "create", &p]), false);
            if rc != 0 {
                return Err(format!("ERREUR: création de {p}:\n{}", out.trim()));
            }
            sayln!("  + {snapdir} créé (racine du FS)");
        }

        // 2a) migration @snapshots (frère de @)
        let old = format!("{}/@snapshots", r.trim_end_matches('/'));
        if is_btrfs_subvolume(&old) {
            let children: Vec<String> = list_subvols(&r)
                .into_iter()
                .filter_map(|c| c.strip_prefix("@snapshots/").map(|x| x.to_string()))
                .collect();
            for c in &children {
                let (rc, out) = move_subvol(&format!("{old}/{c}"), &format!("{p}/{c}"));
                if rc == 0 {
                    sayln!("  + déplacé @snapshots/{c} → {snapdir}/{c}");
                } else {
                    sayln!("  ! @snapshots/{c}: {}", out.trim());
                }
            }
            runv(&sargs(&["btrfs", "subvolume", "delete", &old]), false);
        }

        // 2b) migration du résidu in-@
        let in_at = format!("{}/@/{}", r.trim_end_matches('/'), snapdir);
        if is_btrfs_subvolume(&in_at) {
            let children: Vec<String> = list_subvols(&r)
                .into_iter()
                .filter_map(|c| c.strip_prefix(&format!("@/{snapdir}/")).map(|x| x.to_string()))
                .collect();
            for c in &children {
                let (rc, out) = move_subvol(&format!("{in_at}/{c}"), &format!("{p}/{c}"));
                if rc == 0 {
                    sayln!("  + déplacé @{snapdir}/{c} (dans @) → {snapdir}/{c}");
                } else {
                    sayln!("  ! @{snapdir}/{c}: {}", out.trim());
                }
            }
            runv(&sargs(&["btrfs", "subvolume", "delete", &in_at]), false);
        }
    }

    // 3) point de montage propre + montage (uniquement si non monté)
    let (rc, _) = runv(&sargs(&["findmnt", "-n", &vis]), false);
    if rc != 0 {
        if Path::new(&vis).is_dir() {
            if let Ok(rd) = fs::read_dir(&vis) {
                for e in rd.flatten() {
                    let ep = e.path();
                    if ep.is_dir() && !ep.is_symlink() {
                        let _ = fs::remove_dir_all(&ep);
                    } else {
                        let _ = fs::remove_file(&ep);
                    }
                }
            }
        }
        let _ = fs::create_dir_all(&vis);
        let (rc, out) = runv(
            &sargs(&["mount", "-o", &format!("subvol=/{snapdir},rw,noatime"), &dev, &vis]),
            false,
        );
        if rc != 0 {
            return Err(format!("ERREUR: montage de {vis} impossible:\n{}", out.trim()));
        }
    }

    // 4) migration des anciens snapshots RO → RW
    let mut conv = 0;
    for s in list_snapshots(root, snapdir) {
        if s.full == vis {
            continue;
        }
        if Path::new(&s.full).is_dir() && is_ro_subvolume(&s.full) {
            let (rc, _) = runv(&sargs(&["btrfs", "property", "set", &s.full, "ro", "false"]), false);
            if rc == 0 {
                conv += 1;
            }
        }
    }
    if conv > 0 {
        sayln!("  + {conv} ancien(s) snapshot(s) RO → RW (bootable)");
    }

    // 5) fstab : montage on-demand, plus au boot
    remove_fstab_line_for(&vis, snapdir);
    Ok(vis)
}

pub fn remove_fstab_line_for(snapdir_full: &str, snapdir: &str) {
    let fstab = "/etc/fstab";
    let Ok(content) = fs::read_to_string(fstab) else {
        return;
    };
    let mut kept = String::new();
    let mut removed = 0;
    for ln in content.lines() {
        let s = ln.trim();
        if s.is_empty() || s.starts_with('#') {
            kept.push_str(ln);
            kept.push('\n');
            continue;
        }
        let parts: Vec<&str> = s.split_whitespace().collect();
        let hit = (parts.len() >= 2 && parts[1] == snapdir_full)
            || parts.iter().any(|p| p.contains(&format!("subvol=/{snapdir}")));
        if hit {
            removed += 1;
            continue;
        }
        kept.push_str(ln);
        kept.push('\n');
    }
    if removed > 0 && fs::write(fstab, kept).is_ok() {
        sayln!(
            "  - fstab: {removed} ligne(s) retirée(s) ({snapdir} monté on-demand, plus au boot)"
        );
    }
}

pub fn set_default_subvol(root: &str, subvol: &str) -> (i32, String) {
    let mut gen = subvol_gen_by_path(root, subvol);
    if gen.is_none() {
        if let Ok(rm) = RootMount::new(root) {
            gen = subvol_gen_by_path(&rm.path, subvol);
        }
    }
    let Some(gen) = gen else {
        return (1, format!("subvolume introuvable: {subvol}"));
    };
    runv(&sargs(&["btrfs", "subvolume", "set-default", &gen.to_string(), root]), false)
}

pub fn btrfs_filesystem_of(path: &str) -> Option<String> {
    let (rc, out) = runv(&sargs(&["btrfs", "filesystem", "show", path]), false);
    if rc != 0 {
        return None;
    }
    for line in out.lines() {
        let s = line.trim();
        if s.starts_with('/') {
            return Some(s.trim_start_matches(['*', ' ']).trim().to_string());
        }
    }
    None
}

/// Ligne fstab du sous-volume `<racine>/<snapdir>`.
pub fn snapdir_fstab_line(root: &str, snapdir: &str) -> String {
    let dev = {
        let d = btrfs_dev_of(root);
        if d.is_empty() { "<device>".to_string() } else { d }
    };
    let mnt = format!("{}/{}", root.trim_end_matches('/'), snapdir);
    format!("{dev}\t{mnt}\tbtrfs\tnoatime,subvol=/{snapdir} 0 0\n")
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

pub fn list_snapshots(root: &str, snapdir: &str) -> Vec<Snapshot> {
    let snap = snapdir.trim_matches('/');
    let prefix = format!("{snap}/");
    let base = format!("{}/{}", root.trim_end_matches('/'), snapdir);
    let out;
    if Path::new(&base).is_dir() {
        let (_rc, o) = runv(&sargs(&["btrfs", "subvolume", "list", root]), false);
        out = o;
    } else {
        let Ok(rm) = RootMount::new(root) else {
            return Vec::new();
        };
        let r = rm.path.clone();
        if !Path::new(&format!("{}/{}", r.trim_end_matches('/'), snapdir)).is_dir() {
            return Vec::new();
        }
        let (_rc, o) = runv(&sargs(&["btrfs", "subvolume", "list", &r]), false);
        out = o;
    }
    let mut subs = Vec::new();
    for line in out.lines() {
        let Some((_, rest)) = line.split_once(" path ") else {
            continue;
        };
        let Ok(gen) = line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .parse::<i64>()
        else {
            continue;
        };
        let p = rest.trim().trim_start_matches('/');
        let Some(rel) = p.strip_prefix(&prefix) else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        subs.push(Snapshot {
            generation: gen,
            path: rel.to_string(),
            full: format!("{}/{}/{}", root.trim_end_matches('/'), snapdir, rel),
        });
    }
    subs.sort_by_key(|s| s.generation);
    subs
}

pub fn create_snapshot(root: &str, snapdir: &str, name: Option<&str>) -> String {
    let name = name
        .map(|s| s.to_string())
        .unwrap_or_else(now_stamp)
        .replace(' ', "_");
    if let Err(e) = ensure_snapdir(root, snapdir) {
        sayln!("{e}");
        return String::new();
    }
    let target = format!("{}/{}/{}", root.trim_end_matches('/'), snapdir, name);
    let (rc, out) = make_snapshot(root, &target);
    if rc != 0 {
        sayln!("{}", out.trim());
        return String::new();
    }
    sayln!("Snapshot créé: {target}");
    target
}

pub fn prune_snapshots(root: &str, snapdir: &str, keep: i64) {
    let snapdir_full = format!("{}/{}", root.trim_end_matches('/'), snapdir);
    let snaps: Vec<Snapshot> = list_snapshots(root, snapdir)
        .into_iter()
        .filter(|s| s.full != snapdir_full && Path::new(&s.full).is_dir())
        .collect();
    // keep <= 0 → tout conserver (cohérent avec le script shell et le libellé UI)
    let to_delete: Vec<Snapshot> = if keep <= 0 || snaps.len() as i64 <= keep {
        Vec::new()
    } else {
        snaps[..(snaps.len() - keep as usize)].to_vec()
    };
    for s in to_delete {
        sayln!("Suppression: {}", s.full);
        let (_rc, out) = delete_subvol(&s.full);
        if !out.trim().is_empty() {
            sayln!("{}", out.trim());
        }
    }
}

// ---------------------------------------------------------------------------
// Timers systemd
// ---------------------------------------------------------------------------

fn safe_unit(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim_matches('/').chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

pub fn snap_unit_name(root: &str, snapdir: &str, tag: &str) -> String {
    let (r, s) = (safe_unit(root), safe_unit(snapdir));
    let r = if r.is_empty() { "root".to_string() } else { r };
    let s = if s.is_empty() { "snap".to_string() } else { s };
    format!("{APP}-snap-{r}-{s}-{}", safe_unit(tag))
}

/// Écrire le script shell auto-suffisant (chmod +x) et renvoyer son chemin.
pub fn install_snap_script() -> String {
    let body = include_str!("../assets/btrfsmgr-snap.sh")
        .replace("__BM_DESKTOP_ENTRY__", AUTO_NOTIF_DESKTOP);
    if let Some(parent) = Path::new(SNAP_SCRIPT).parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(SNAP_SCRIPT, body).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(SNAP_SCRIPT, fs::Permissions::from_mode(0o755));
        }
    }

    // Icône menu (.desktop)
    let candidates: Vec<String> = {
        let mut v = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(d) = exe.parent() {
                v.push(d.join("btrfsmgr.desktop").to_string_lossy().into_owned());
                v.push(
                    d.join("../share/btrfsmgr/btrfsmgr.desktop")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        v.push(format!("{}/btrfsmgr.desktop", env!("CARGO_MANIFEST_DIR")));
        v
    };
    let src = candidates.into_iter().find(|p| Path::new(p).is_file());
    if let Some(parent) = Path::new(DESKTOP_FILE).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let content = match src {
        Some(p) => fs::read_to_string(p).unwrap_or_default(),
        None => "[Desktop Entry]\nType=Application\nName=BTRFS Manager\nComment=Gérer les sous-volumes BTRFS — snapshots, restauration, automation\nExec=/usr/bin/sudo /usr/bin/btrfsmgr\nIcon=chronometer\nTerminal=true\nCategories=System;Filesystem;\n".to_string(),
    };
    if Path::new(DESKTOP_FILE).exists() {
        // Déjà fourni par le paquet : ne jamais l'écraser avec le fallback.
        return SNAP_SCRIPT.to_string();
    }
    match fs::write(DESKTOP_FILE, content) {
        Ok(_) => sayln!("  - icône menu installée: {DESKTOP_FILE}"),
        Err(e) => sayln!("  ! icône menu non installée: {e}"),
    }
    SNAP_SCRIPT.to_string()
}

/// Contenu (service, timer, description du planning) des unités systemd.
/// Fonction pure : testable et comparée au Python en non-régression.
#[allow(clippy::too_many_arguments)]
pub fn timer_unit_contents(
    root: &str,
    snapdir: &str,
    tag: &str,
    schedule: &str,
    keep: i64,
    boot: bool,
    boot_delay: i64,
    once_per_day: bool,
    script_path: &str,
) -> (String, String, String) {
    let unit = snap_unit_name(root, snapdir, tag);
    let svc = format!("{unit}.service");
    let tmr = format!("{unit}.timer");
    let once_flag = if boot && once_per_day { "1" } else { "0" };
    let svc_content = format!(
        "[Unit]\nDescription={APP} — snapshot btrfs de {root} ({tag})\nDefaultDependencies=no\n\n[Service]\nType=oneshot\nExecStart={script_path} {root} {snapdir} {tag}-%%Y%%m%%d-%%H%%M%%S {keep} {once_flag}\nRemainAfterExit=no\nUser=root\n"
    );
    let (trigger, plan) = if boot {
        let mut plan = format!("Au démarrage ({boot_delay} min après le boot)");
        if once_per_day {
            plan.push_str(" — un seul instantané par jour");
        }
        (format!("OnBootSec={boot_delay}min"), plan)
    } else {
        (format!("OnCalendar={schedule}"), schedule.to_string())
    };
    let tmr_content = format!(
        "[Unit]\nDescription={APP} — timer snapshots de {root} ({tag})\n\n[Timer]\n{trigger}\nPersistent=true\nUnit={svc}\n\n[Install]\nWantedBy=timers.target\n"
    );
    let _ = tmr;
    (svc_content, tmr_content, plan)
}

/// Créer service + timer pour les instantanés automatiques avec rétention.
#[allow(clippy::too_many_arguments)]
pub fn write_timer_files(
    root: &str,
    snapdir: &str,
    tag: &str,
    schedule: &str,
    keep: i64,
    boot: bool,
    boot_delay: i64,
    once_per_day: bool,
) -> String {
    let unit = snap_unit_name(root, snapdir, tag);
    let svc = format!("{unit}.service");
    let tmr = format!("{unit}.timer");
    let _ = fs::create_dir_all(SYSTEMD_SNAPDIR);

    let script_path = install_snap_script();
    let (svc_content, tmr_content, plan) = timer_unit_contents(
        root,
        snapdir,
        tag,
        schedule,
        keep,
        boot,
        boot_delay,
        once_per_day,
        &script_path,
    );
    let _ = fs::write(format!("{SYSTEMD_SNAPDIR}/{svc}"), svc_content);
    let _ = fs::write(format!("{SYSTEMD_SNAPDIR}/{tmr}"), tmr_content);

    let (rc, out) = runv(&sargs(&["systemctl", "daemon-reload"]), false);
    if rc != 0 {
        sayln!("WARN: daemon-reload en échec: {}", out.trim());
    }
    let (rc, out) = runv(&sargs(&["systemctl", "enable", "--now", &tmr]), false);
    if rc == 0 {
        sayln!("Timer activé: {tmr}");
        sayln!("  Planning:  {plan}");
        sayln!("  Rétention: {keep} snapshots");
    } else {
        sayln!("Échec de l'activation du timer: {}", out.trim());
    }
    tmr
}

pub fn delete_timer_files(root: &str, snapdir: &str, tag: &str) {
    let unit = snap_unit_name(root, snapdir, tag);
    let tmr = format!("{unit}.timer");
    let svc = format!("{unit}.service");
    runv(&sargs(&["systemctl", "disable", "--now", &tmr]), false);
    let _ = fs::remove_file(format!("{SYSTEMD_SNAPDIR}/{svc}"));
    let _ = fs::remove_file(format!("{SYSTEMD_SNAPDIR}/{tmr}"));
    runv(&sargs(&["systemctl", "daemon-reload"]), false);
    sayln!("Timer supprimé: {tmr}");
}

pub fn list_timers() -> Vec<Timer> {
    let (rc, out) = runv(
        &sargs(&["systemctl", "list-timers", "--all", "--no-pager", "--plain"]),
        false,
    );
    if rc != 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for line in out.lines() {
        if !line.contains(&format!("{APP}-")) {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let tname = parts
            .iter()
            .find(|p| p.ends_with(".timer"))
            .copied()
            .unwrap_or_else(|| parts[parts.len() - 2]);
        rows.push(Timer {
            next: parts[0].to_string(),
            last: parts[1].to_string(),
            activated: parts[2].to_string(),
            timer: tname.to_string(),
        });
    }
    rows
}

pub fn auto_snapshot(root: &str, snapdir: &str, name: Option<&str>, keep: i64) -> i32 {
    let name = name.map(|s| s.to_string()).unwrap_or_else(now_stamp);
    let script = install_snap_script();
    sayln!(
        "[{APP}] auto snapshot: {root} → {}/{}",
        root.trim_end_matches('/'),
        format!("{snapdir}/{name}")
    );
    let (rc, out) = runv(
        &sargs(&[&script, root, snapdir, &name, &keep.to_string()]),
        false,
    );
    if rc != 0 {
        sayln!("{}", out.trim());
        return rc;
    }
    sayln!(
        "[{APP}] création OK: {}/{}",
        root.trim_end_matches('/'),
        format!("{snapdir}/{name}")
    );
    0
}

// ---------------------------------------------------------------------------
// Restauration
// ---------------------------------------------------------------------------

pub fn snaprel(snap_name: &str) -> String {
    let s = snap_name.trim_matches('/');
    if let Some(v) = s.strip_prefix("@snapshots/") {
        return v.to_string();
    }
    if let Some(v) = s.strip_prefix("snapshots/") {
        return v.to_string();
    }
    s.to_string()
}

pub fn snap_subvol(snapdir: &str, snap_name: &str) -> String {
    let name = snap_name.trim_matches('/');
    let sd = snapdir.trim_matches('/');
    if name == sd || name.starts_with(&format!("{sd}/")) {
        return name.to_string();
    }
    format!("{sd}/{name}")
}

pub fn restore_snapshot(root: &str, snapdir: &str, snap_name: &str) -> i32 {
    let Some(at_name) = boot_subvol(root) else {
        sayln!("ERREUR: aucun sous-volume principal nommé (le default subvolume et l'entrée de boot standard sont sans nom, et / n'est pas sur un sous-volume nommé).  La restauration par mv nécessite un volume de boot nommé (ex. @, @rootfs).");
        return 1;
    };
    let at_name = at_name.trim_end_matches('/').to_string();
    let at_leaf = at_name.clone();
    let rel = snaprel(snap_name);

    let rm = match RootMount::new(root) {
        Ok(r) => r,
        Err(e) => {
            sayln!("{e}");
            return 1;
        }
    };
    let r = rm.path.trim_end_matches('/').to_string();
    let at = format!("{r}/{at_name}");
    let snap = format!("{r}/{}/{rel}", snapdir.trim_matches('/'));
    let stamp = now_stamp();
    let old = format!("{r}/{}/{at_leaf}_old-{stamp}", snapdir.trim_matches('/'));

    if !(is_btrfs_subvolume(&at) || is_ro_subvolume(&at)) {
        sayln!("ERREUR: sous-volume par défaut introuvable: {at}");
        return 1;
    }
    if !Path::new(&snap).is_dir() {
        sayln!("ERREUR: snapshot introuvable sous la racine: {snap}");
        return 1;
    }
    if Path::new(&old).exists() {
        sayln!("ERREUR: {old} existe déjà");
        return 1;
    }

    let old_base = Path::new(&old)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    sayln!("Restauration: snapshots/{rel} → {at_name} (l'ancien {at_name} → {snapdir}/{old_base})");
    let (rc, out) = runv(&sargs(&["mv", &at, &old]), false);
    if rc != 0 {
        sayln!("Échec du déplacement de {at}:\n{}", out.trim());
        return rc;
    }
    runv(&sargs(&["sync"]), false);
    runv(&sargs(&["btrfs", "property", "set", &snap, "ro", "false"]), false);
    let (rc, out) = runv(&sargs(&["mv", &snap, &at]), false);
    if rc != 0 {
        sayln!("Échec du placement du snapshot — rollback…");
        runv(&sargs(&["mv", &old, &at]), false);
        sayln!("{}", out.trim());
        return rc;
    }
    drop(rm);

    let (rc, out) = set_default_subvol(root, &at_name);
    if rc != 0 {
        sayln!("(avert.) défaut non repointé: {}", out.trim());
    }
    sayln!("Restauration terminée: {rel} est désormais {at_name}.");
    sayln!("L'ancien sous-volume est conservé dans {snapdir}/{old_base}.");
    sayln!("Redémarrez pour booter sur la version restaurée.");
    0
}

// ---------------------------------------------------------------------------
// Chargeur de boot / entrées
// ---------------------------------------------------------------------------

pub fn detect_bootloader() -> &'static str {
    for p in [
        "/boot/loader",
        "/boot/efi/loader",
        "/efi/loader",
        "/sys/firmware/efi/efi/systemd",
    ] {
        if Path::new(p).is_dir() {
            return "systemd-boot";
        }
    }
    // Solus : clr-boot-manager n'existe que pour piloter systemd-boot. C'est
    // la signature fiable quand /boot n'est PAS monté (cas par défaut sur
    // Solus) et que /etc/grub.d existe quand même dans le paquet grub.
    if which("clr-boot-manager").is_some() {
        return "systemd-boot";
    }
    if Path::new("/etc/grub.d").is_dir()
        || Path::new("/etc/default/grub").is_file()
        || Path::new("/boot/grub/grub.cfg").is_file()
        || Path::new("/boot/grub2/grub.cfg").is_file()
    {
        return "grub";
    }
    if let Ok(rd) = fs::read_dir("/boot/efi/EFI") {
        for e in rd.flatten() {
            if e.path().join("grub.cfg").is_file() {
                return "grub";
            }
        }
    }
    "unknown"
}

/// True si `fname` porte plausiblement la version de noyau `kver`.
pub fn fver_match(fname: &str, kver: &str) -> bool {
    let Some(mm) = major_minor(kver) else {
        return false;
    };
    if !fname.contains(&mm) {
        return false;
    }
    // suffixe -N de kver
    if let Some(idx) = kver.find('-') {
        let tail: String = kver[idx + 1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !tail.is_empty() && !fname.contains(&tail) {
            return false;
        }
    }
    true
}

fn major_minor(kver: &str) -> Option<String> {
    let bytes: Vec<char> = kver.chars().collect();
    let mut start = None;
    for (i, c) in bytes.iter().enumerate() {
        if c.is_ascii_digit() {
            // forme N.N
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == '.' {
                let mut k = j + 1;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                if k > j + 1 {
                    return Some(bytes[i..k].iter().collect());
                }
            }
            start = Some(i);
            break;
        }
    }
    start.map(|_| String::new())
}

pub fn kernel_release() -> String {
    let (_rc, out) = runv(&sargs(&["uname", "-r"]), false);
    out.trim().to_string()
}

pub fn snapshot_module_dirs(base: &str) -> Vec<String> {
    for sub in ["usr/lib/modules", "lib/modules"] {
        let d = format!("{}/{}", base.trim_end_matches('/'), sub);
        if Path::new(&d).is_dir() {
            if let Ok(rd) = fs::read_dir(&d) {
                let mut v: Vec<String> = rd
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                v.sort();
                return v;
            }
            return Vec::new();
        }
    }
    Vec::new()
}

pub fn snapshot_kernel_info(root: &str, snapdir: &str, snap_name: &str) -> KernelInfo {
    let current = kernel_release();
    let rel = snaprel(snap_name);
    let base = format!("{}/{}/{}", root.trim_end_matches('/'), snapdir, rel);
    let mut kernels = snapshot_module_dirs(&base);
    if kernels.is_empty() && !Path::new(&base).is_dir() {
        if let Ok(rm) = RootMount::new(root) {
            kernels = snapshot_module_dirs(&format!(
                "{}/{}/{}",
                rm.path.trim_end_matches('/'),
                snapdir,
                rel
            ));
        }
    }
    let mismatch = !kernels.is_empty() && !kernels.iter().any(|k| fver_match(k, &current));
    KernelInfo {
        current,
        snapshot_kernels: kernels,
        mismatch,
    }
}

fn e2l(path: &str) -> String {
    let p = path.replace('\\', "/").trim_start_matches('/').to_string();
    if p.starts_with("boot/") {
        return format!("/{p}");
    }
    format!("/boot/{p}")
}

pub fn boot_is_mounted() -> bool {
    runv(&sargs(&["findmnt", "-n", "-o", "SOURCE", "/boot"]), false).0 == 0
}

pub fn ensure_boot_mounted() -> bool {
    if boot_is_mounted() {
        // Déjà monté avant nous : on le laissera monté en sortie.
        return true;
    }
    sayln!("( /boot n'est pas monté — tentative de montage… )");
    let (rc, _out) = runv(&sargs(&["clr-boot-manager", "mount-boot"]), false);
    if rc == 0 {
        sayln!("  ✓ /boot monté via clr-boot-manager.");
    } else {
        let (rc, out) = runv(&sargs(&["mount", "/boot"]), false);
        if rc == 0 {
            sayln!("  ✓ /boot monté.");
        } else {
            sayln!("  ✗ impossible de monter /boot.");
            if !out.trim().is_empty() {
                sayln!("    {}", out.trim());
            }
        }
    }
    let ok = boot_is_mounted();
    if ok {
        BOOT_MOUNTED_BY_US.store(true, Ordering::SeqCst);
    }
    ok
}

/// Démonter /boot uniquement si c'est nous qui l'avons monté.
pub fn release_boot_mount() {
    if BOOT_MOUNTED_BY_US.swap(false, Ordering::SeqCst) {
        runv(&sargs(&["umount", "/boot"]), false);
        sayln!("  - /boot démonté (monté par btrfsmgr).");
    }
}

/// (device racine, image noyau, image initrd) relatifs à /boot.
pub fn get_boot_info() -> (String, String, String) {
    let (rc, out) = runv(&sargs(&["findmnt", "-n", "-o", "SOURCE", "/"]), false);
    let mut src = out.trim().to_string();
    if src.is_empty() || rc != 0 {
        let (_rc, out) = runv(&sargs(&["df", "--output=source", "/"]), false);
        src = out
            .lines()
            .last()
            .unwrap_or("/dev/sda1")
            .trim()
            .to_string();
    }
    if let Some(idx) = src.find('[') {
        src.truncate(idx);
    }

    let kver = runv(&sargs(&["uname", "-r"]), false).1.trim().to_string();
    let mut kernel = String::new();
    let mut initrd = String::new();

    if let Ok(cmdline) = fs::read_to_string("/proc/cmdline") {
        for tok in cmdline.split_whitespace() {
            if let Some(v) = tok.strip_prefix("initrd=") {
                if initrd.is_empty() {
                    initrd = e2l(v);
                }
            }
        }
    }

    for base in ["/boot", "/boot/efi"] {
        if !Path::new(base).is_dir() {
            continue;
        }
        for (path, name) in walk_files(base) {
            if kernel.is_empty()
                && (name.starts_with("vmlinuz")
                    || name.starts_with("linux-")
                    || name.starts_with("kernel-"))
                && fver_match(&name, &kver)
            {
                kernel = path.clone();
            }
            if initrd.is_empty()
                && (name.starts_with("initrd")
                    || name.starts_with("initramfs")
                    || name.starts_with("init"))
                && fver_match(&name, &kver)
            {
                initrd = path;
            }
        }
    }
    if kernel.is_empty() {
        kernel = format!("vmlinuz-{kver}");
    }
    if initrd.is_empty() {
        initrd = format!("initrd-{kver}");
    }
    (src, relimg(&kernel), relimg(&initrd))
}

fn walk_files(base: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![base.to_string()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p.to_string_lossy().into_owned());
                } else {
                    out.push((
                        p.to_string_lossy().into_owned(),
                        e.file_name().to_string_lossy().into_owned(),
                    ));
                }
            }
        }
    }
    out
}

fn relimg(p: &str) -> String {
    if let Some(v) = p.strip_prefix("/boot/") {
        return v.to_string();
    }
    p.trim_start_matches('/').to_string()
}

pub fn current_boot_options() -> String {
    let Ok(cmdline) = fs::read_to_string("/proc/cmdline") else {
        return String::new();
    };
    let opts: Vec<&str> = cmdline
        .trim()
        .split_whitespace()
        .filter(|t| !t.starts_with("BOOT_IMAGE=") && !t.starts_with("initrd"))
        .collect();
    opts.join(" ")
}

pub fn snap_boot_options(snap_subvol: &str) -> String {
    let opts = current_boot_options();
    let parts: Vec<&str> = opts.split_whitespace().collect();
    if parts.is_empty() {
        return format!("subvol={snap_subvol} ro");
    }
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for tok in parts {
        if let Some(val) = tok.strip_prefix("rootflags=") {
            if val.starts_with("subvol=") {
                out.push(format!("rootflags=subvol={snap_subvol}"));
                replaced = true;
            } else {
                out.push(tok.to_string());
            }
        } else if tok.starts_with("subvol=") {
            out.push(format!("subvol={snap_subvol}"));
            replaced = true;
        } else {
            out.push(tok.to_string());
        }
    }
    if !replaced {
        out.push(format!("subvol={snap_subvol}"));
    }
    out.join(" ")
}

pub fn add_bootloader_snapshot_entry(
    _root: &str,
    snapdir: &str,
    snap_name: &str,
    label: &str,
    verbose: bool,
) -> String {
    let (src, kernel, initrd) = get_boot_info();
    let subvol = snap_subvol(snapdir, snap_name);
    let mut options = snap_boot_options(&subvol);
    if options.is_empty() {
        options = format!("root={src} subvol=/{subvol} ro");
    }
    let entry = format!("title {label}\nlinux /{kernel}\ninitrd /{initrd}\noptions {options}\n");
    let _ = fs::create_dir_all("/boot/loader/entries");
    let fname = format!("btrfsmgr-{}.conf", subvol.replace('/', "-"));
    let fpath = format!("/boot/loader/entries/{fname}");
    let _ = fs::write(&fpath, entry);
    if verbose {
        sayln!("Entrée systemd-boot créée: {label}");
        sayln!("  Fichier: {fpath}");
        sayln!("  Options: {options}");
    }
    fpath
}

/// Entrées systemd-boot générées par btrfsmgr (`/boot/loader/entries/btrfsmgr-*.conf`).
pub fn boot_entries() -> Vec<BootEntry> {
    let dir = "/boot/loader/entries";
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut out = Vec::new();
    for file in names {
        if !file.starts_with("btrfsmgr-") || !file.ends_with(".conf") {
            continue;
        }
        let path = format!("{dir}/{file}");
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        out.push(BootEntry {
            file,
            title: entry_title(&text),
            subvol: entry_subvol_text(&text),
        });
    }
    out
}

/// Titre d'une entrée (`title …`), vide si absent.
pub fn entry_title(text: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix("title "))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub fn entry_subvol(fpath: &str) -> String {
    let Ok(text) = fs::read_to_string(fpath) else {
        return String::new();
    };
    entry_subvol_text(&text)
}

pub fn entry_subvol_text(text: &str) -> String {
    for line in text.lines() {
        if !line.trim_start().starts_with("options") {
            continue;
        }
        for tok in line.split_whitespace() {
            if let Some(val) = tok.strip_prefix("rootflags=") {
                for flag in val.split(',') {
                    if let Some(sv) = flag.strip_prefix("subvol=") {
                        return sv.trim_matches('/').to_string();
                    }
                }
            } else if let Some(sv) = tok.strip_prefix("subvol=") {
                return sv.trim_matches('/').to_string();
            }
        }
    }
    String::new()
}

/// Synchroniser les entrées systemd-boot générées avec les snapshots présents.
pub fn sync_bootloader_entries(root: &str, snapdir: &str, verbose: bool) {
    if !ensure_boot_mounted() {
        if verbose {
            sayln!("Synchronisation boot: /boot inaccessible, ignoré.");
        }
        return;
    }
    if detect_bootloader() != "systemd-boot" {
        return;
    }
    let entries_dir = "/boot/loader/entries";
    if !Path::new(entries_dir).is_dir() {
        return;
    }
    let snaps = list_snapshots(root, snapdir);

    let mut snap_by_file: BTreeMap<String, Snapshot> = BTreeMap::new();
    for s in &snaps {
        let subvol = snap_subvol(snapdir, &s.path);
        let fname = format!("btrfsmgr-{}.conf", subvol.replace('/', "-"));
        snap_by_file.insert(fname, s.clone());
    }

    let mut to_add: Vec<(String, Snapshot)> = Vec::new();
    let mut to_remove: Vec<String> = Vec::new();

    if let Ok(rd) = fs::read_dir(entries_dir) {
        let mut names: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for fname in names {
            if !fname.starts_with("btrfsmgr-") || !fname.ends_with(".conf") {
                continue;
            }
            if snap_by_file.contains_key(&fname) {
                continue;
            }
            let subvol = entry_subvol(&format!("{entries_dir}/{fname}"));
            if !subvol.is_empty() && Path::new(&format!("{root}/{subvol}")).is_dir() {
                continue;
            }
            to_remove.push(fname);
        }
    }

    for (fname, s) in &snap_by_file {
        if !Path::new(&format!("{entries_dir}/{fname}")).exists() {
            to_add.push((fname.clone(), s.clone()));
        }
    }

    if verbose {
        if to_add.is_empty() && to_remove.is_empty() {
            sayln!(
                "Synchronisation boot: {} entrée(s) OK, rien à faire.",
                snap_by_file.len()
            );
        }
        for (fname, s) in &to_add {
            sayln!("  + ajout  {fname} ({})", s.path);
        }
        for fname in &to_remove {
            sayln!("  - retrait {fname}");
        }
    }

    let sd = snapdir.trim_matches('/');
    for (fname, s) in &to_add {
        let subvol = snap_subvol(snapdir, &s.path);
        let short = subvol
            .strip_prefix(&format!("{sd}/"))
            .unwrap_or(&subvol)
            .to_string();
        let label = format!("Snapshot {short} ({sd})");
        add_bootloader_snapshot_entry(root, snapdir, &s.path, &label, false);
        let _ = fname;
    }
    for fname in &to_remove {
        if fs::remove_file(format!("{entries_dir}/{fname}")).is_err() && verbose {
            sayln!("  ! échec suppression {fname}");
        }
    }
}

// ---------------------------------------------------------------------------
// Divers
// ---------------------------------------------------------------------------

pub fn parse_selections(choice: &str, count: usize) -> Vec<usize> {
    let c = choice.trim();
    if ["all", "tous", "tout", "*"].contains(&c.to_lowercase().as_str()) {
        return (0..count).collect();
    }
    let mut seen: Vec<usize> = Vec::new();
    for tk in c
        .split([' ', ',', ';', '\t'])
        .filter(|s| !s.is_empty())
    {
        let Ok(n) = tk.parse::<usize>() else {
            continue;
        };
        if n == 0 {
            continue;
        }
        let i = n - 1;
        if i < count && !seen.contains(&i) {
            seen.push(i);
        }
    }
    seen
}

/// Extrait le tag d'un nom d'unité `btrfsmgr-snap-<root>-<snapdir>-<tag>.timer`.
pub fn timer_tag(timer: &str, root: &str, snapdir: &str) -> String {
    let (r, s) = (safe_unit(root), safe_unit(snapdir));
    let r = if r.is_empty() { "root".to_string() } else { r };
    let s = if s.is_empty() { "snap".to_string() } else { s };
    let prefix = format!("{APP}-snap-{r}-{s}-");
    timer
        .strip_prefix(&prefix)
        .unwrap_or(timer)
        .strip_suffix(".timer")
        .unwrap_or(timer)
        .to_string()
}

/// Valider/corriger un root erroné (ex. /snapshots) en remontant à /.
pub fn validate_root(raw_root: &str) -> String {
    let r = raw_root.trim_end_matches('/');
    if r.is_empty() || r == "/" {
        return "/".to_string();
    }
    if r == "/snapshots" || r == "snapshots" {
        sayln!(
            "{}",
            t("NOTE: root /snapshots détecté — restauration impossible sur un sous-dossier.  Utilisation de / (racine système).")
        );
        return "/".to_string();
    }
    "/".to_string()
}

pub fn bootloader_display(token: &str) -> String {
    match token {
        "systemd-boot" => "systemd-boot".to_string(),
        "grub" => "GRUB".to_string(),
        _ => t("Inconnu"),
    }
}

pub fn snapshot_labels(snaps: &[Snapshot]) -> Vec<String> {
    snaps
        .iter()
        .map(|s| format!("{}   (gen {})", s.path, s.generation))
        .collect()
}

pub fn unique_paths(snaps: &[Snapshot]) -> BTreeSet<String> {
    snaps.iter().map(|s| s.full.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fver_match_handles_distro_suffixes() {
        assert!(fver_match("vmlinuz-6.12.113", "6.12.113"));
        assert!(fver_match("kernel-6.12.113-1", "6.12.113-1"));
        assert!(fver_match("vmlinuz-6.12.113+", "6.12.113"));
        assert!(!fver_match("vmlinuz-6.11.0", "6.12.113"));
        assert!(!fver_match("vmlinuz-6.12.113", "6.12.113-42"));
        assert!(!fver_match("vmlinuz", ""));
    }

    #[test]
    fn snaprel_never_doubles_prefix() {
        assert_eq!(snaprel("@snapshots/foo"), "foo");
        assert_eq!(snaprel("snapshots/foo"), "foo");
        assert_eq!(snaprel("foo"), "foo");
        assert_eq!(snaprel("/snapshots/foo/"), "foo");
    }

    #[test]
    fn snap_subvol_prefixes_once() {
        assert_eq!(snap_subvol("snapshots", "foo"), "snapshots/foo");
        assert_eq!(snap_subvol("snapshots", "snapshots/foo"), "snapshots/foo");
        assert_eq!(snap_subvol("@snapshots", "@snapshots/foo"), "@snapshots/foo");
        assert_eq!(snap_subvol("/snapshots/", "/foo/"), "snapshots/foo");
    }

    #[test]
    fn parse_selections_accepts_lists_and_all() {
        assert_eq!(parse_selections("3", 5), vec![2]);
        assert_eq!(parse_selections("1,3,5", 5), vec![0, 2, 4]);
        assert_eq!(parse_selections("1 3 5", 6), vec![0, 2, 4]);
        assert_eq!(parse_selections("tous", 3), vec![0, 1, 2]);
        assert_eq!(parse_selections("9,1", 3), vec![0]);
        assert_eq!(parse_selections("x,0", 3), Vec::<usize>::new());
    }

    #[test]
    fn human_size_matches_python_format() {
        assert_eq!(human_size(0.0), "0.0o");
        assert_eq!(human_size(1023.0), "1023.0o");
        assert_eq!(human_size(1024.0), "1.0K");
        assert_eq!(human_size(1536.0), "1.5K");
    }

    #[test]
    fn snap_unit_name_sanitises() {
        assert_eq!(
            snap_unit_name("/", "snapshots", "daily plan"),
            "btrfsmgr-snap-root-snapshots-daily-plan"
        );
        assert_eq!(
            timer_tag("btrfsmgr-snap-root-snapshots-daily.timer", "/", "snapshots"),
            "daily"
        );
    }

    #[test]
    fn boot_options_rewrite_subvol() {
        // depends on /proc/cmdline; only checks the pure helper's invariants
        let got = snap_boot_options("@snapshots/x");
        if !got.is_empty() {
            assert!(got.contains("subvol=@snapshots/x"));
            assert!(!got.contains("subvol=@ "));
        }
    }

    #[test]
    fn boot_entry_parsing() {
        let t = "title Snapshot foo (snapshots)\nlinux /vmlinuz\ninitrd /initrd\noptions root=UUID=1 rootflags=subvol=@snapshots/foo rw\n";
        assert_eq!(entry_title(t), "Snapshot foo (snapshots)");
        assert_eq!(entry_subvol_text(t), "@snapshots/foo");
        let b = "title X\noptions root=UUID=1 subvol=@ rw\n";
        assert_eq!(entry_subvol_text(b), "@");
    }

    #[test]
    fn entry_subvol_parses_rootflags_and_bare() {
        let dir = temp_dir("bm-test.");
        let f1 = format!("{dir}/a.conf");
        fs::write(&f1, "title x\noptions root=UUID=1 rootflags=subvol=@snapshots/a rw\n").unwrap();
        assert_eq!(entry_subvol(&f1), "@snapshots/a");
        let f2 = format!("{dir}/b.conf");
        fs::write(&f2, "title y\noptions root=UUID=1 subvol=@ rw\n").unwrap();
        assert_eq!(entry_subvol(&f2), "@");
        let _ = fs::remove_dir_all(&dir);
    }
}
