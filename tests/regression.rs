//! Tests de non-régression du portage Rust.
//!
//! Ces tests sont **autonomes** : plus aucune dépendance à l'implémentation
//! Python d'origine (supprimée du dépôt). Les valeurs attendues ont été
//! figées à partir du comportement validé pendant la migration, et couvrent :
//!   * les fonctions pures (normalisation, versions, sélections, tailles…)
//!   * la génération exacte des unités systemd
//!   * l'intégrité du script d'instantané embarqué
//!   * le comportement face à un environnement BTRFS simulé (stubs)

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use btrfsmgr::core::*;
use btrfsmgr::update::version_key;

fn stub(dir: &Path, name: &str, body: &str) {
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    let mut perm = fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&p, perm).unwrap();
}

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "bm-regress-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn pure_functions_are_stable() {
    // normalisation des noms de snapshot
    for (input, expected) in [
        ("@snapshots/foo", "foo"),
        ("snapshots/foo", "foo"),
        ("foo", "foo"),
        ("/snapshots/foo/", "foo"),
        ("@snapshots", "@snapshots"),
        ("", ""),
    ] {
        assert_eq!(snaprel(input), expected, "snaprel({input:?})");
    }

    for ((dir, name), expected) in [
        (("snapshots", "foo"), "snapshots/foo"),
        (("snapshots", "snapshots/foo"), "snapshots/foo"),
        (("@snapshots", "@snapshots/foo"), "@snapshots/foo"),
        (("/snapshots/", "/foo/"), "snapshots/foo"),
    ] {
        assert_eq!(
            snap_subvol(dir, name),
            expected,
            "snap_subvol({dir:?},{name:?})"
        );
    }

    // versions de noyau : tolérant aux suffixes de distribution
    for ((file, kver), expected) in [
        (("vmlinuz-6.12.113", "6.12.113"), true),
        (("kernel-6.12.113-1", "6.12.113-1"), true),
        (("vmlinuz-6.12.113+", "6.12.113"), true),
        (("vmlinuz-6.11.0", "6.12.113"), false),
        (("vmlinuz-6.12.113", "6.12.113-42"), false),
        (("vmlinuz", ""), false),
    ] {
        assert_eq!(
            fver_match(file, kver),
            expected,
            "fver_match({file:?},{kver:?})"
        );
    }

    for (input, expected) in [
        ("1.0.15", (1, 0, 15)),
        ("v1.2", (1, 2, 0)),
        ("2", (2, 0, 0)),
        ("nope", (0, 0, 0)),
    ] {
        assert_eq!(version_key(input), expected, "version_key({input:?})");
    }
    assert!(version_key("1.0.9") < version_key("1.0.10"));
    assert!(version_key("0.9.9") < version_key("1.0.0"));

    for ((choice, count), expected) in [
        (("3", 5), vec![2]),
        (("1,3,5", 5), vec![0, 2, 4]),
        (("1 3 5", 6), vec![0, 2, 4]),
        (("tous", 3), vec![0, 1, 2]),
        (("9,1", 3), vec![0]),
        (("x,0", 3), vec![]),
        (("all", 4), vec![0, 1, 2, 3]),
    ] {
        assert_eq!(
            parse_selections(choice, count),
            expected,
            "parse_selections({choice:?})"
        );
    }

    for (size, expected) in [
        (0.0, "0.0o"),
        (1023.0, "1023.0o"),
        (1024.0, "1.0K"),
        (1536.0, "1.5K"),
        (1048576.0, "1.0M"),
    ] {
        assert_eq!(human_size(size), expected, "human_size({size})");
    }

    assert_eq!(
        snap_unit_name("/", "snapshots", "daily plan"),
        "btrfsmgr-snap-root-snapshots-daily-plan"
    );
    assert_eq!(
        snap_unit_name("/mnt/a", "snap", "x/y"),
        "btrfsmgr-snap-mnt-a-snap-x-y"
    );
    assert_eq!(
        timer_tag("btrfsmgr-snap-root-snapshots-daily.timer", "/", "snapshots"),
        "daily"
    );

    // lecture d'une entrée systemd-boot
    let entry = "title Snapshot foo (snapshots)\nlinux /vmlinuz\ninitrd /initrd\noptions root=UUID=1 rootflags=subvol=@snapshots/foo rw\n";
    assert_eq!(entry_title(entry), "Snapshot foo (snapshots)");
    assert_eq!(entry_subvol_text(entry), "@snapshots/foo");
}

#[test]
fn systemd_units_match_golden() {
    let (svc, tmr, plan) = timer_unit_contents(
        "/",
        "snapshots",
        "daily",
        "*-*-* 03:00:00",
        7,
        false,
        5,
        false,
        "/usr/local/bin/btrfsmgr-snap.sh",
    );
    assert_eq!(
        svc,
        "[Unit]\nDescription=btrfsmgr — snapshot btrfs de / (daily)\nDefaultDependencies=no\n\n\
[Service]\nType=oneshot\n\
ExecStart=/usr/local/bin/btrfsmgr-snap.sh / snapshots daily-%%Y%%m%%d-%%H%%M%%S 7 0\n\
RemainAfterExit=no\nUser=root\n"
    );
    assert_eq!(
        tmr,
        "[Unit]\nDescription=btrfsmgr — timer snapshots de / (daily)\n\n\
[Timer]\nOnCalendar=*-*-* 03:00:00\nPersistent=true\n\
Unit=btrfsmgr-snap-root-snapshots-daily.service\n\n[Install]\nWantedBy=timers.target\n"
    );
    assert_eq!(plan, "*-*-* 03:00:00");

    let (svc, tmr, plan) = timer_unit_contents(
        "/",
        "snapshots",
        "daily",
        "",
        7,
        true,
        7,
        true,
        "/usr/local/bin/btrfsmgr-snap.sh",
    );
    assert!(svc.contains("ExecStart=/usr/local/bin/btrfsmgr-snap.sh / snapshots daily-%%Y%%m%%d-%%H%%M%%S 7 1"));
    assert!(tmr.contains("OnBootSec=7min"));
    assert_eq!(
        plan,
        "Au démarrage (7 min après le boot) — un seul instantané par jour"
    );
}

#[test]
fn snap_script_asset_is_self_contained() {
    let script = include_str!("../assets/btrfsmgr-snap.sh");
    assert!(script.starts_with("#!/usr/bin/env bash"));
    assert!(script.contains("__BM_DESKTOP_ENTRY__"));
    assert!(script.contains("string:desktop-entry:"));
    assert!(script.contains("ONCE_PER_DAY"));
    assert!(script.contains("prune_snapshots"));
    let installed = script.replace("__BM_DESKTOP_ENTRY__", "btrfsmgr.desktop");
    assert!(!installed.contains("__BM_DESKTOP_ENTRY__"));
}

/// Un seul test touche PATH : les variables d'environnement sont globales au
/// processus et cargo exécute les tests en parallèle.
#[test]
fn stubbed_btrfs_environment() {
    let base = tmpdir("env");
    let bin = base.join("bin");
    let fixture = base.join("fixture");
    let root = base.join("root");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&fixture).unwrap();
    fs::create_dir_all(root.join("snapshots")).unwrap();
    // prune_snapshots ne retient que les entrées qui sont des dossiers
    for name in ["a", "b", "@_old-20260101-000000"] {
        fs::create_dir_all(root.join("snapshots").join(name)).unwrap();
    }

    fs::write(
        fixture.join("subvols.txt"),
        "ID 257 gen 10 top level 5 path @\n\
         ID 258 gen 11 top level 5 path @home\n\
         ID 259 gen 20 top level 5 path snapshots\n\
         ID 260 gen 30 top level 5 path snapshots/a\n\
         ID 261 gen 40 top level 5 path snapshots/b\n\
         ID 262 gen 50 top level 5 path @snapshots\n\
         ID 263 gen 60 top level 5 path snapshots/@_old-20260101-000000\n",
    )
    .unwrap();
    fs::write(
        fixture.join("timers.txt"),
        "Fri 2026-11-01 03:00:00 CET 1h ago btrfsmgr-snap-root-snapshots-daily.timer btrfsmgr-snap-root-snapshots-daily.service\n",
    )
    .unwrap();

    stub(
        &bin,
        "btrfs",
        "#!/bin/sh\ncase \"$1 $2\" in\n  \"subvolume list\") cat \"$BM_FIXTURE/subvols.txt\";;\n  \"subvolume get-default\") echo 'ID 256 gen 100 top level 5 path @';;\n  \"subvolume show\") echo 'Name: @';;\n  \"property get\") echo 'ro=true';;\n  *) exit 0;;\nesac\n",
    );
    stub(
        &bin,
        "findmnt",
        "#!/bin/sh\ncase \"$*\" in\n  *SOURCE,OPTIONS*) echo '/dev/vda2[/@] rw,relatime';;\n  *) echo '/dev/vda2[/@]';;\nesac\n",
    );
    stub(
        &bin,
        "systemctl",
        "#!/bin/sh\nif [ \"$1\" = list-timers ]; then cat \"$BM_FIXTURE/timers.txt\"; fi\nexit 0\n",
    );
    stub(
        &bin,
        "df",
        "#!/bin/sh\necho 'Filesystem Size Used Avail Use% Mounted on'\n",
    );
    stub(&bin, "uname", "#!/bin/sh\necho 6.12.113\n");
    for n in ["mount", "umount", "sync", "mv"] {
        stub(&bin, n, "#!/bin/sh\nexit 0\n");
    }

    let orig_path = std::env::var("PATH").unwrap_or_default();
    let stub_path = format!("{}:{}", bin.display(), orig_path);
    std::env::set_var("PATH", &stub_path);
    std::env::set_var("BM_FIXTURE", fixture.to_string_lossy().into_owned());
    let root_s = root.to_string_lossy().into_owned();

    let snaps = list_snapshots(&root_s, "snapshots");
    let timers = list_timers();
    let (_, prune_out) = capture(|| prune_snapshots(&root_s, "snapshots", 2));

    std::env::set_var("PATH", &orig_path);

    let paths: Vec<&str> = snaps.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(paths, vec!["a", "b", "@_old-20260101-000000"]);
    assert_eq!(snaps[0].full, format!("{root_s}/snapshots/a"));

    assert_eq!(timers.len(), 1);
    assert_eq!(timers[0].next, "Fri");
    assert_eq!(timers[0].last, "2026-11-01");
    assert_eq!(timers[0].activated, "03:00:00");
    assert_eq!(timers[0].timer, "btrfsmgr-snap-root-snapshots-daily.timer");

    // keep=2 sur 3 entrées → seule la plus ancienne est supprimée
    assert_eq!(prune_out, format!("Suppression: {root_s}/snapshots/a\n"));

    let _ = fs::remove_dir_all(&base);
}
