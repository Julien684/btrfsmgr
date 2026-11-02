//! Vérification de version Gitea, mise à jour et désinstallation.

use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::core::*;


/// Convertit une version en tuple comparable (major, minor, patch).
pub fn version_key(v: &str) -> (i64, i64, i64) {
    let s = v.trim();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let mut nums = [0i64; 3];
    let mut idx = 0;
    for part in s.split(|c: char| !c.is_ascii_digit()) {
        if part.is_empty() {
            continue;
        }
        if idx >= 3 {
            break;
        }
        nums[idx] = part.parse().unwrap_or(0);
        idx += 1;
    }
    (nums[0], nums[1], nums[2])
}

/// URL git du dépôt (remote `gitea` si présent, sinon Gitea public).
pub fn repo_url(src: &str) -> String {
    if Path::new(&format!("{src}/.git")).is_dir() {
        let (rc, out) = runv(&sargs(&["git", "-C", src, "remote", "get-url", "gitea"]), false);
        if rc == 0 && !out.trim().is_empty() {
            return out.trim().to_string();
        }
    }
    PROJECT_REPO_URL.to_string()
}

/// Liste des tags (API Gitea puis fallback `git ls-remote`).
pub fn gitea_tags(repo: &str) -> Vec<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    if let Ok(resp) = agent
        .get(&format!("{PROJECT_API}/tags"))
        .set("User-Agent", "btrfsmgr")
        .set("Accept", "application/json")
        .call()
    {
        if let Ok(v) = resp.into_json::<serde_json::Value>() {
            let mut names = Vec::new();
            if let Some(arr) = v.as_array() {
                for d in arr {
                    if let Some(n) = d.get("name").and_then(|n| n.as_str()) {
                        if !n.is_empty() {
                            names.push(n.to_string());
                        }
                    }
                }
            }
            if !names.is_empty() {
                return names;
            }
        }
    }
    let (rc, out) = runv(&sargs(&["git", "ls-remote", "--tags", repo]), false);
    if rc == 0 {
        let mut names = Vec::new();
        for ln in out.lines() {
            if let Some(reference) = ln.split_whitespace().last() {
                if reference.ends_with("^{}") {
                    continue;
                }
                if let Some(n) = reference.rsplit('/').next() {
                    names.push(n.to_string());
                }
            }
        }
        return names;
    }
    Vec::new()
}

/// Dernière version (tag) disponible, None si indisponible.
pub fn latest_remote_version(repo: &str) -> Option<String> {
    let mut best: Option<String> = None;
    for tag in gitea_tags(repo) {
        let v = tag.trim().trim_start_matches(['v', 'V']).to_string();
        if v.is_empty() || !v.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
            continue;
        }
        match &best {
            None => best = Some(v),
            Some(b) if version_key(&v) > version_key(b) => best = Some(v),
            _ => {}
        }
    }
    best
}

/// Vérifier la dernière version publiée sur Gitea.
///
/// Depuis la migration Rust, l'installation passe par l'eopkg : `update`
/// n'installe plus rien, il compare la version locale à la dernière version
/// publiée et indique la commande de mise à jour.
pub fn cli_update() -> i32 {
    let local = VERSION.to_string();
    let src = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
        .unwrap_or_else(|| ".".to_string());
    let repo = repo_url(&src);
    println!("{APP} {local}");

    let Some(remote) = latest_remote_version(&repo) else {
        println!("  Version Gitea indisponible (hors ligne ou dépôt sans tag).");
        return 1;
    };

    println!("  Dernière version Gitea : {remote}");
    if version_key(&local) < version_key(&remote) {
        println!("  Une mise à jour est disponible : {local} → {remote}");
        println!("  Mettre à jour avec : sudo eopkg upgrade {APP}");
        return 0;
    }
    println!("  Vous êtes déjà à jour.");
    0
}

pub fn cli_uninstall(root: &str, snapdir: &str) -> i32 {
    if !is_root() {
        println!("ERREUR: lancer en root (sudo)");
        return 1;
    }
    let mut units: Vec<String> = match fs::read_dir(SYSTEMD_SNAPDIR) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|u| {
                u.starts_with("btrfsmgr-") && (u.ends_with(".service") || u.ends_with(".timer"))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    units.sort();
    for u in &units {
        let path = format!("{SYSTEMD_SNAPDIR}/{u}");
        runv(&sargs(&["systemctl", "stop", u]), false);
        if let Err(e) = fs::remove_file(&path) {
            println!("  ! {path}: {e}");
        }
        println!("  - unit retirée: {u}");
    }
    runv(&sargs(&["systemctl", "daemon-reload"]), false);

    for path in [GRUB_SNAPDIR_FILE, BOOTXMD_SNAPDIR] {
        if Path::new(path).exists() {
            let _ = fs::remove_file(path);
            println!("  - entrée GRUB retirée: {path}");
        }
    }
    let edir = "/boot/loader/entries";
    if let Ok(rd) = fs::read_dir(edir) {
        for e in rd.flatten() {
            let f = e.file_name().to_string_lossy().into_owned();
            if f.starts_with("btrfsmgr-") && f.ends_with(".conf") {
                let _ = fs::remove_file(e.path());
                println!("  - entry systemd-boot retirée: {f}");
            }
        }
    }

    let snapdir_full = format!("{}/{}", root.trim_end_matches('/'), snapdir);
    if runv(&sargs(&["findmnt", "-n", &snapdir_full]), false).0 == 0 {
        runv(&sargs(&["umount", &snapdir_full]), false);
        println!("  - démonté: {snapdir_full}");
    }
    if let Ok(content) = fs::read_to_string("/etc/fstab") {
        let before = content.lines().count();
        let kept: String = content
            .lines()
            .filter(|ln| !ln.contains(&snapdir_full))
            .map(|ln| format!("{ln}\n"))
            .collect();
        let after = kept.lines().count();
        if after != before {
            let _ = fs::write("/etc/fstab", kept);
            println!("  - fstab: ligne {snapdir_full} retirée");
        }
    }

    for path in ["/bin/btrfsmgr", "/usr/local/bin/btrfsmgr", SNAP_SCRIPT] {
        if Path::new(path).exists() {
            let _ = fs::remove_file(path);
            println!("  - binaire retiré: {path}");
        }
    }
    if Path::new(DESKTOP_FILE).exists() {
        let _ = fs::remove_file(DESKTOP_FILE);
        println!("  - icône menu retirée: {DESKTOP_FILE}");
    }

    println!(
        "\nDésinstallation terminée. Le sous-volume snapshots ({snapdir_full}) a été CONSERVÉ avec son contenu."
    );
    println!("Pour le supprimer définitivement (NE PAS supprimer @ !):");
    println!("  DEV=$(findmnt -n -o SOURCE {root} | cut -d'[' -f1)");
    println!("  M=$(mktemp -d); mount -o subvol=/,rw $DEV $M");
    println!("  btrfs subvolume delete $M/{snapdir}");
    println!("  umount $M && rmdir $M");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_key_orders_versions() {
        assert_eq!(version_key("1.0.15"), (1, 0, 15));
        assert_eq!(version_key("v1.2"), (1, 2, 0));
        assert_eq!(version_key("2"), (2, 0, 0));
        assert!(version_key("1.0.15") < version_key("1.0.16"));
        assert!(version_key("0.9.9") < version_key("1.0.0"));
        assert_eq!(version_key("nope"), (0, 0, 0));
    }

    #[test]
    fn version_update_detection() {
        assert!(version_key("1.0.9") < version_key("v1.0.10"));
        assert!(!(version_key("1.0.15") < version_key("1.0.15")));
    }
}
