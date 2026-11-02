//! btrfsmgr — point d'entrée CLI / TUI.

use btrfsmgr::core::*;
use btrfsmgr::tui::Tui;
use btrfsmgr::update;

const USAGE: &str = "usage: btrfsmgr [-h] [--snapdir SNAPDIR] [--version] [--name NAME]\n                 [--keep KEEP] [args_ ...]\n\nGestionnaire TUI BTRFS (snapshots, timers, boot)\n";

fn abspath(p: &str) -> String {
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    std::env::current_dir()
        .map(|d| d.join(path))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string())
}

fn parse_keep(raw: &str) -> i64 {
    match raw.parse::<i64>() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("argument --keep: valeur entière invalide: '{raw}'");
            std::process::exit(2);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut positional: Vec<String> = Vec::new();
    let mut snapdir = DEFAULT_SNAPDIR.to_string();
    let mut name: Option<String> = None;
    let mut keep: i64 = 7;

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let (key, inline) = match a.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k, Some(v.to_string())),
            _ => (a, None),
        };
        match key {
            "--version" | "-V" => {
                println!("{APP} {VERSION}");
                return;
            }
            "--help" | "-h" => {
                print!("{USAGE}");
                return;
            }
            "--snapdir" => {
                snapdir = match inline.or_else(|| {
                    i += 1;
                    args.get(i).cloned()
                }) {
                    Some(v) => v,
                    None => {
                        eprintln!("argument --snapdir: une valeur est requise");
                        std::process::exit(2);
                    }
                };
            }
            "--name" => {
                name = inline.or_else(|| {
                    i += 1;
                    args.get(i).cloned()
                });
            }
            "--keep" => {
                let v = inline.or_else(|| {
                    i += 1;
                    args.get(i).cloned()
                });
                keep = parse_keep(&v.unwrap_or_default());
            }
            _ => positional.push(a.to_string()),
        }
        i += 1;
    }

    // Interprétation tolérante des positionnels.
    let cmds = [
        "tui",
        "auto",
        "update",
        "uninstall",
        "ensure",
        "sync",
        "delete-snap",
    ];
    let mut cmd = "tui".to_string();
    let mut root = "/".to_string();
    let mut snaps: Vec<String> = Vec::new();
    let mut seen_root = false;
    for a in &positional {
        if cmds.contains(&a.as_str()) {
            cmd = a.clone();
        } else if cmd == "delete-snap" && !seen_root {
            root = a.clone();
            seen_root = true;
        } else if cmd == "delete-snap" && seen_root {
            snaps.push(a.clone());
        } else {
            root = a.clone();
        }
    }
    if positional.len() > 10 {
        eprintln!("btrfsmgr: error: trop d'arguments");
        std::process::exit(2);
    }
    let root = abspath(&root);

    let code = match cmd.as_str() {
        "auto" => auto_snapshot(&root, &snapdir, name.as_deref(), keep),
        "update" => update::cli_update(),
        "uninstall" => update::cli_uninstall(&root, &snapdir),
        "ensure" => {
            if !is_root() {
                println!("ERREUR: exécuter en root (sudo)");
                1
            } else {
                match ensure_snapdir(&root, &snapdir) {
                    Ok(_) => {
                        println!(
                            "OK: {}/{} (snapshots au niveau racine du FS, montés)",
                            root.trim_end_matches('/'),
                            snapdir
                        );
                        0
                    }
                    Err(e) => {
                        println!("{e}");
                        1
                    }
                }
            }
        }
        "sync" => {
            sync_bootloader_entries(&root, &snapdir, true);
            0
        }
        "delete-snap" => {
            if !is_root() {
                println!("ERREUR: exécuter en root (sudo)");
                1
            } else {
                let listed = list_snapshots(&root, &snapdir);
                let mut ok = true;
                for n in &snaps {
                    let n = snaprel(n);
                    let Some(s) = listed.iter().find(|s| s.path == n) else {
                        println!("Snapshot introuvable: {n}");
                        ok = false;
                        continue;
                    };
                    let (rc, out) = delete_subvol(&s.full);
                    if rc == 0 {
                        println!("Supprimé: {}", s.full);
                    } else {
                        println!("Échec: {} — {}", s.full, out.trim());
                        ok = false;
                    }
                }
                sync_bootloader_entries(&root, &snapdir, false);
                if ok { 0 } else { 1 }
            }
        }
        _ => {
            // Crochet de test : rendu/menus sans privilèges (les opérations
            // BTRFS échoueront, mais l'interface est utilisable).
            if !is_root() && std::env::var("BTRFSMGR_ALLOW_NONROOT").is_err() {
                println!("ERREUR: exécuter en root:  sudo btrfsmgr");
                std::process::exit(1);
            }
            if let Err(e) = std::panic::catch_unwind(|| sync_bootloader_entries(&root, &snapdir, true)) {
                let _ = e;
                println!("Warning: synchronisation boot ignorée");
            }
            let mut tui = Tui::new(&root, &snapdir);
            tui.run()
        }
    };
    std::process::exit(code);
}
