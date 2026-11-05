//! TUI interactive (ANSI/termios) — équivalent du TUI curses Python.
//!
//! Navigation : flèches ↑/↓ (ou j/k), Entrée, Échap, chiffres, raccourcis
//! mnémotechniques ; mode multi avec Espace. Hors terminal : repli numéroté.

use std::collections::HashMap;
use std::io::{BufRead, Write};

use crate::core::*;
use crate::i18n::{t, tf};

// ---------------------------------------------------------------------------
// Terminal brut
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Key {
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Enter,
    Esc,
    Backspace,
    Space,
    Char(char),
    /// Clic souris (SGR 1006 ou X10). Coordonnées 1-based.
    Mouse {
        button: u8,
        x: u16,
        y: u16,
        pressed: bool,
    },
    Eof,
    Unknown,
}

struct Term {
    orig: libc::termios,
}

impl Term {
    fn new() -> Option<Term> {
        if unsafe { libc::isatty(0) } != 1 {
            return None;
        }
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let orig = t;
            libc::cfmakeraw(&mut t);
            // cfmakeraw coupe OPOST : `\n` ne reviendrait plus en colonne 0
            // et tout l'écran s'afficherait en escalier. On garde le mode
            // brut pour l'ENTRÉE, mais on rétablit la traduction NL→CRNL.
            t.c_oflag |= libc::OPOST | libc::ONLCR;
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return None;
            }
            // 1000 = événements bouton, 1006 = format SGR (coordonnées
            // fiables au-delà de la colonne 223) ; les deux sont désactivés
            // au Drop.
            print!("\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h");
            let _ = std::io::stdout().flush();
            Some(Term { orig })
        }
    }

    fn input_ready(&self, ms: i32) -> bool {
        unsafe {
            let mut p: libc::pollfd = std::mem::zeroed();
            p.fd = 0;
            p.events = libc::POLLIN;
            libc::poll(&mut p, 1, ms) > 0
        }
    }

    fn read_byte(&mut self) -> Option<u8> {
        let mut b = [0u8; 1];
        let n = unsafe { libc::read(0, b.as_mut_ptr() as *mut libc::c_void, 1) };
        if n == 1 {
            Some(b[0])
        } else {
            None
        }
    }

    fn read_byte_timeout(&mut self, ms: i32) -> Option<u8> {
        if self.input_ready(ms) {
            self.read_byte()
        } else {
            None
        }
    }

    fn read_key(&mut self) -> Key {
        let Some(b) = self.read_byte() else {
            return Key::Eof;
        };
        match b {
            0x1b => {
                let Some(c) = self.read_byte_timeout(50) else {
                    return Key::Esc;
                };
                match c {
                    b'[' => self.read_csi(),
                    b'O' => match self.read_byte_timeout(50) {
                        Some(b'A') => Key::Up,
                        Some(b'B') => Key::Down,
                        Some(b'H') => Key::Home,
                        Some(b'F') => Key::End,
                        _ => Key::Unknown,
                    },
                    _ => Key::Esc,
                }
            }
            0x0a | 0x0d => Key::Enter,
            0x7f | 0x08 => Key::Backspace,
            b' ' => Key::Space,
            0x09 => Key::Unknown,
            c if (0x20..0x7f).contains(&c) => Key::Char(c as char),
            _ => Key::Unknown,
        }
    }

    /// Lire une séquence CSI (`ESC [ …`). Gère flèches, PagUp/Down et souris
    /// (SGR 1006 `<b;x;yM` et X10 `M` + 3 octets).
    fn read_csi(&mut self) -> Key {
        let mut params: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.read_byte_timeout(50) else {
                return Key::Unknown;
            };
            match c {
                b'A' => return Key::Up,
                b'B' => return Key::Down,
                b'H' => return Key::Home,
                b'F' => return Key::End,
                b'~' => {
                    return match params.first() {
                        Some(b'5') => Key::PageUp,
                        Some(b'6') => Key::PageDown,
                        _ => Key::Unknown,
                    }
                }
                b'M' => {
                    if params.first() == Some(&b'<') {
                        return parse_sgr_mouse(&params[1..], true);
                    }
                    let mut raw = [0u8; 3];
                    for slot in raw.iter_mut() {
                        match self.read_byte_timeout(50) {
                            Some(v) => *slot = v,
                            None => return Key::Unknown,
                        }
                    }
                    return parse_x10_mouse(raw[0], raw[1], raw[2]);
                }
                b'm' => {
                    if params.first() == Some(&b'<') {
                        return parse_sgr_mouse(&params[1..], false);
                    }
                    return Key::Unknown;
                }
                _ => params.push(c),
            }
            if params.len() > 24 {
                return Key::Unknown;
            }
        }
    }
}

/// `ESC [ < b ; x ; y M|m` (SGR 1006).
fn parse_sgr_mouse(params: &[u8], pressed: bool) -> Key {
    let txt = String::from_utf8_lossy(params);
    let nums: Vec<u16> = txt.split(';').filter_map(|v| v.parse().ok()).collect();
    if nums.len() < 3 {
        return Key::Unknown;
    }
    Key::Mouse {
        button: (nums[0] & 0xff) as u8,
        x: nums[1],
        y: nums[2],
        pressed,
    }
}

/// `ESC [ M` suivi de (bouton, x, y) décalés de 32 (X10).
fn parse_x10_mouse(b: u8, x: u8, y: u8) -> Key {
    let btn = b.wrapping_sub(32);
    Key::Mouse {
        button: btn & 3,
        x: x.saturating_sub(32) as u16,
        y: y.saturating_sub(32) as u16,
        pressed: (btn & 3) != 3,
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.orig);
        }
        print!("\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
    }
}

// ---------------------------------------------------------------------------
// TUI
// ---------------------------------------------------------------------------

pub struct Tui {
    pub root: String,
    pub snapdir: String,
    term: Option<Term>,
}

impl Tui {
    pub fn new(root: &str, snapdir: &str) -> Tui {
        Tui {
            root: validate_root(root),
            snapdir: snapdir.to_string(),
            term: Term::new(),
        }
    }

    fn interactive(&self) -> bool {
        self.term.is_some()
    }

    fn clear(&self) {
        print!("\x1b[2J\x1b[H");
        let _ = std::io::stdout().flush();
    }

    fn draw_menu(
        &self,
        title: &str,
        items: &[String],
        footer: &str,
        sel: usize,
        numbered: bool,
        multi: bool,
        chosen: &[usize],
    ) {
        self.clear();
        println!("\x1b[1m{title}\x1b[0m");
        println!("\x1b[2m{}\x1b[0m", "─".repeat(59));
        if !footer.is_empty() {
            println!("\x1b[1;33m{footer}\x1b[0m");
            println!();
        }
        for (i, it) in items.iter().enumerate() {
            let tag = if numbered {
                format!(" {:>2} ", i + 1)
            } else {
                "    ".to_string()
            };
            let chk = if multi {
                if chosen.contains(&i) {
                    "[x] "
                } else {
                    "[ ] "
                }
            } else {
                ""
            };
            let line = format!("{tag}{chk}{it}");
            if i == sel {
                println!("\x1b[7m{line}\x1b[0m");
            } else {
                println!("{line}");
            }
        }
        let _ = std::io::stdout().flush();
    }

    /// Menu simple ; None = annulé (Échap).
    fn choose(
        &mut self,
        title: &str,
        items: &[String],
        footer: &str,
        default: usize,
        numbered: bool,
        hotkeys: &HashMap<char, usize>,
    ) -> Option<usize> {
        if items.is_empty() {
            return None;
        }
        if !self.interactive() {
            return self.choose_fallback(title, items, footer, hotkeys);
        }
        let mut sel = if default < items.len() { default } else { 0 };
        loop {
            self.draw_menu(title, items, footer, sel, numbered, false, &[]);
            let key = self.term.as_mut().map(|t| t.read_key()).unwrap_or(Key::Eof);
            match key {
                Key::Up => sel = (sel + items.len() - 1) % items.len(),
                Key::Down => sel = (sel + 1) % items.len(),
                Key::Home => sel = 0,
                Key::End => sel = items.len() - 1,
                Key::PageUp => sel = sel.saturating_sub(10),
                Key::PageDown => sel = (sel + 10).min(items.len() - 1),
                Key::Enter | Key::Space => return Some(sel),
                Key::Esc | Key::Eof => return None,
                Key::Mouse {
                    button,
                    y,
                    pressed,
                    ..
                } if pressed && (button & 3) == 0 => {
                    // Ligne 1 = titre, 2 = séparateur, 3 = aide, 4 = blanc,
                    // puis les entrées à partir de la ligne 5.
                    let first = if footer.is_empty() { 3 } else { 5 };
                    if (y as usize) >= first {
                        let idx = y as usize - first;
                        if idx < items.len() {
                            return Some(idx);
                        }
                    }
                }
                Key::Char(c) => {
                    if let Some(&idx) = hotkeys.get(&c.to_ascii_lowercase()) {
                        return Some(idx);
                    }
                    if numbered {
                        if let Some(n) = c.to_digit(10) {
                            if n >= 1 && (n as usize) <= items.len() {
                                sel = n as usize - 1;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn choose_fallback(
        &mut self,
        title: &str,
        items: &[String],
        footer: &str,
        hotkeys: &HashMap<char, usize>,
    ) -> Option<usize> {
        println!("{title}");
        println!("{}", "─".repeat(60));
        for (i, it) in items.iter().enumerate() {
            println!("  {}  {}", i + 1, it);
        }
        if !footer.is_empty() {
            println!("{}", "─".repeat(60));
            println!("{footer}");
        }
        print!("{}: ", t("Choix"));
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return None;
        }
        let ans = line.trim().to_lowercase();
        if let Some(c) = ans.chars().next() {
            if let Some(&i) = hotkeys.get(&c) {
                return Some(i);
            }
        }
        if let Ok(n) = ans.parse::<usize>() {
            if n >= 1 && n <= items.len() {
                return Some(n - 1);
            }
        }
        println!("{}", t("Choix invalide."));
        None
    }

    /// Menu multi-sélection ; None = annulé (Échap).
    fn choose_multi(
        &mut self,
        title: &str,
        items: &[String],
        footer: &str,
    ) -> Option<Vec<usize>> {
        if items.is_empty() {
            return None;
        }
        if !self.interactive() {
            println!("{title}");
            println!("{}", "─".repeat(60));
            for (i, it) in items.iter().enumerate() {
                println!("  {}  {}", i + 1, it);
            }
            println!("{}", "─".repeat(60));
            println!("{footer}");
            print!("\x1b[0m{}: ", t("Choix"));
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if std::io::stdin().lock().read_line(&mut line).is_err() {
                return None;
            }
            return Some(parse_selections(line.trim(), items.len()));
        }
        let mut sel = 0usize;
        let mut chosen: Vec<usize> = Vec::new();
        loop {
            self.draw_menu(title, items, footer, sel, true, true, &chosen);
            let key = self.term.as_mut().map(|t| t.read_key()).unwrap_or(Key::Eof);
            match key {
                Key::Up => sel = (sel + items.len() - 1) % items.len(),
                Key::Down => sel = (sel + 1) % items.len(),
                Key::Space => {
                    if let Some(pos) = chosen.iter().position(|&x| x == sel) {
                        chosen.remove(pos);
                    } else {
                        chosen.push(sel);
                    }
                    sel = (sel + 1).min(items.len() - 1);
                }
                Key::Enter => {
                    chosen.sort_unstable();
                    return Some(chosen);
                }
                Key::Esc | Key::Eof => return None,
                Key::Mouse {
                    button,
                    y,
                    pressed,
                    ..
                } if pressed && (button & 3) == 0 => {
                    let first = if footer.is_empty() { 3 } else { 5 };
                    if (y as usize) >= first {
                        let idx = y as usize - first;
                        if idx < items.len() {
                            if let Some(pos) = chosen.iter().position(|&v| v == idx) {
                                chosen.remove(pos);
                            } else {
                                chosen.push(idx);
                            }
                            sel = idx;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn ask(&mut self, prompt: &str, default: &str) -> String {
        let label = if default.is_empty() {
            format!("{prompt}: ")
        } else {
            format!("{prompt}[{default}]: ")
        };
        if !self.interactive() {
            print!("{label}");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if std::io::stdin().lock().read_line(&mut line).is_err() {
                return default.to_string();
            }
            let v = line.trim().to_string();
            return if v.is_empty() { default.to_string() } else { v };
        }
        self.clear();
        print!("{label}");
        let _ = std::io::stdout().flush();
        let mut buf = String::new();
        loop {
            let key = self.term.as_mut().map(|t| t.read_key()).unwrap_or(Key::Eof);
            match key {
                Key::Enter => break,
                Key::Esc | Key::Eof => break,
                Key::Backspace => {
                    if buf.pop().is_some() {
                        print!("\x08 \x08");
                        let _ = std::io::stdout().flush();
                    }
                }
                Key::Char(c) => {
                    buf.push(c);
                    print!("{c}");
                    let _ = std::io::stdout().flush();
                }
                _ => {}
            }
        }
        println!();
        let v = buf.trim().to_string();
        if v.is_empty() {
            default.to_string()
        } else {
            v
        }
    }

    fn ask_yn(&mut self, prompt: &str) -> bool {
        let ans = self.ask(&format!("{prompt} [o/n]"), "").to_lowercase();
        matches!(ans.as_str(), "o" | "oui" | "y" | "yes")
    }

    fn pause(&mut self) {
        if !self.interactive() {
            print!("\n[Entrée pour continuer…]");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            return;
        }
        println!("\n\x1b[2m[Entrée pour continuer…]\x1b[0m");
        let _ = std::io::stdout().flush();
        if let Some(t) = self.term.as_mut() {
            // Le RELÂCHEMENT du clic qui vient d'ouvrir l'écran ne doit pas
            // le refermer : on l'ignore. Un nouveau clic (appui) ou une
            // touche poursuit.
            loop {
                match t.read_key() {
                    // Le relâchement du clic qui a ouvert l'écran (et un
                    // éventuel événement terminal parasite) ne doit pas le
                    // refermer : seule une vraie touche poursuit.
                    Key::Mouse { .. } | Key::Unknown => continue,
                    _ => break,
                }
            }
        }
    }

    fn render(&mut self, title: &str, items: &[String], footer: &str) {
        if self.interactive() {
            self.clear();
        }
        println!("\x1b[1;36m{title}\x1b[0m");
        println!("\x1b[2;33m{}\x1b[0m", "─".repeat(60));
        for it in items {
            println!("  {it}");
        }
        if !footer.is_empty() {
            println!("\x1b[2;33m{}\x1b[0m", "─".repeat(60));
            println!("\x1b[2m{footer}\x1b[0m");
        }
        let _ = std::io::stdout().flush();
    }

    fn sync_boot(&self) {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sync_bootloader_entries(&self.root, &self.snapdir, true);
        })) {
            Ok(()) => {}
            Err(_) => sayln!("Warning: sync boot ignorée"),
        }
    }

    fn ask_boot_action(&mut self, action: &str, body: &str) -> Option<&'static str> {
        let title = t("Adapter les entrées de démarrage systemd-boot ?");
        let mut info = vec![tf("Opération : {action}", &[("action", action)])];
        if !body.is_empty() {
            info.extend(body.trim_end().lines().map(|l| l.to_string()));
        }
        info.push(String::new());
        info.push(t("• Adapter systemd-boot : mettre le menu de démarrage à jour."));
        info.push(t("• Ne pas adapter : laisser le menu de démarrage inchangé."));
        info.push(t("• Annuler : ne rien faire."));
        let n_info = info.len();
        let mut items = info;
        items.push(t("Adapter systemd-boot"));
        items.push(t("Ne pas adapter"));
        items.push(t("Annuler"));
        let idx = self.choose(
            &title,
            &items,
            &t("↑/↓ naviguer · Entrée ou clic souris choisir · Échap annuler"),
            n_info,
            false,
            &HashMap::new(),
        )?;
        if idx < n_info {
            return self.ask_boot_action(action, body);
        }
        [Some("adapt"), Some("skip"), None][idx - n_info]
    }

    // ----------------------------------------------------------------- écrans

    fn main_menu(&mut self) -> String {
        let title = tf(
            " Unofficial Solus BTRFS Manager v{ver} — {root}",
            &[("ver", VERSION), ("root", &self.root)],
        );
        let items: Vec<String> = vec![
            t("Créer un instantané"),
            t("Lister les instantanés"),
            t("Restaurer un instantané"),
            t("Supprimer un instantané"),
            t("Programmer des instantanés automatiques (systemd)"),
            t("Supprimer une automatisation (timer systemd)"),
            t("Rétention : conserver les N plus récents"),
            t("Infos système (chargeur de boot, timers…)"),
            t("Menu systemd-boot : afficher/masquer au démarrage"),
            t("Quitter"),
        ];
        let keys = ["s", "l", "r", "d", "p", "x", "7", "i", "m", "q"];
        let mut hot = HashMap::new();
        for (k, v) in [('s', 0), ('l', 1), ('r', 2), ('d', 3), ('a', 3), ('p', 4), ('x', 5), ('i', 7), ('m', 8), ('q', 9)] {
            hot.insert(k, v);
        }
        match self.choose(
            &title,
            &items,
            &t("↑/↓ naviguer · Entrée ou clic souris choisir · Échap quitter"),
            0,
            true,
            &hot,
        ) {
            Some(i) => keys[i].to_string(),
            None => "q".to_string(),
        }
    }

    fn do_create(&mut self) {
        let name = self.ask(&t("Nom du snapshot"), &now_stamp());
        let action = tf("Créer l'instantané « {name} »", &[("name", &name)]);
        let Some(boot) = self.ask_boot_action(&action, "") else {
            println!("{}", t("Annulé."));
            self.pause();
            return;
        };
        create_snapshot(&self.root, &self.snapdir, Some(&name));
        if boot == "adapt" {
            self.sync_boot();
        }
        self.pause();
    }

    fn do_list(&mut self) {
        let vis = join_path(&self.root, &self.snapdir);
        let snaps = list_snapshots(&self.root, &self.snapdir);
        if snaps.is_empty() {
            let title = tf(
                " Snapshots — {vis}",
                &[("vis", &vis)],
            );
            self.render(&title, &[t("(aucun snapshot)")], "");
            self.pause();
            return;
        }
        let mut items: Vec<String> = snaps
            .iter()
            .rev()
            .map(|s| format!("{}   (gen {})", s.path, s.generation))
            .collect();
        let others: Vec<Snapshot> = all_subvols(&self.root)
            .into_iter()
            .filter(|s| {
                s.path != self.snapdir && !s.path.starts_with(&format!("{}/", self.snapdir))
            })
            .collect();
        if !others.is_empty() {
            items.push(String::new());
            items.push(t("Autres sous-volumes:"));
            for s in others {
                items.push(format!("      {}   (gen {})", s.path, s.generation));
            }
        }
        let title = tf(
            " Snapshots — {vis}  ({n} snapshot(s))",
            &[("vis", &vis), ("n", &snaps.len().to_string())],
        );
        self.render(
            &title,
            &items,
            &t("N = plus récent → moins récent"),
        );
        self.pause();
    }

    fn ask_hour(&mut self) -> i64 {
        loop {
            let h = self.ask(&t("Heure (0-23)"), "3");
            if let Ok(h) = h.trim().parse::<i64>() {
                if (0..=23).contains(&h) {
                    return h;
                }
            }
            println!("{}", t("Heure invalide (0-23)"));
        }
    }

    fn do_schedule(&mut self) {
        let freqs = vec![
            t("Chaque jour"),
            t("Chaque semaine"),
            t("Chaque mois"),
        ];
        let Some(fi) = self.choose(
            &t(" Programmer des instantanés automatiques — fréquence"),
            &freqs,
            &t("↑/↓ naviguer · Entrée ou clic souris choisir · Échap annuler"),
            0,
            true,
            &HashMap::new(),
        ) else {
            println!("{}", t("Annulé."));
            self.pause();
            return;
        };
        let freq = ["daily", "weekly", "monthly"][fi];

        let mut boot = false;
        let mut boot_delay = 5i64;
        let mut once_per_day = false;
        let schedule: String;

        if freq == "daily" {
            let when = vec![t("À une heure précise"), t("Au démarrage de la machine")];
            let Some(wi) = self.choose(
                &t(" Chaque jour"),
                &when,
                &t("↑/↓ naviguer · Entrée ou clic souris choisir · Échap annuler"),
                0,
                true,
                &HashMap::new(),
            ) else {
                println!("{}", t("Annulé."));
                self.pause();
                return;
            };
            if wi == 1 {
                boot = true;
                loop {
                    let d = self.ask(
                        &t("Créer l'instantané combien de minutes après le démarrage ?"),
                        "5",
                    );
                    match d.trim().parse::<i64>() {
                        Ok(v) => {
                            boot_delay = v.max(0);
                            break;
                        }
                        Err(_) => println!("{}", t("Nombre invalide")),
                    }
                }
                once_per_day = self.ask_yn(&t(
                    "N'effectuer qu'un seul instantané par jour (ignoré si un instantané du jour existe déjà) ?",
                ));
                schedule = String::new();
            } else {
                let hour = self.ask_hour();
                schedule = format!("*-*-* {hour:02}:00:00");
            }
        } else if freq == "weekly" {
            let days = vec![
                t("lundi"),
                t("mardi"),
                t("mercredi"),
                t("jeudi"),
                t("vendredi"),
                t("samedi"),
                t("dimanche"),
            ];
            let Some(di) = self.choose(
                &t(" Chaque semaine — jour"),
                &days,
                &t("↑/↓ naviguer · Entrée ou clic souris choisir · Échap annuler"),
                0,
                true,
                &HashMap::new(),
            ) else {
                println!("{}", t("Annulé."));
                self.pause();
                return;
            };
            let abbr = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][di];
            let hour = self.ask_hour();
            schedule = format!("{abbr} *-*-* {hour:02}:00:00");
        } else {
            let j = self.ask(&t("Numéro du jour dans le mois (1-31)"), "1");
            let Ok(daynum) = j.trim().parse::<i64>() else {
                println!("{}", t("Choix invalide (1-31)"));
                self.pause();
                return;
            };
            if !(1..=31).contains(&daynum) {
                println!("{}", t("Choix invalide (1-31)"));
                self.pause();
                return;
            }
            let hour = self.ask_hour();
            schedule = format!("*-*-{daynum} {hour:02}:00:00");
        }

        let keep = {
            let k = self.ask(&t("Nombre d'instantanés à conserver"), "7");
            match k.trim().parse::<i64>() {
                Ok(v) => v.max(0),
                Err(_) => {
                    println!("{}", t("Nombre invalide"));
                    self.pause();
                    return;
                }
            }
        };
        let tag = self.ask(&t("Nom du plan (ex. daily, weekly)"), freq);

        let plan_desc = if boot {
            if once_per_day {
                tf(
                    "Au démarrage de la machine ({delay} min après le boot, un seul par jour)",
                    &[("delay", &boot_delay.to_string())],
                )
            } else {
                tf(
                    "Au démarrage de la machine ({delay} min après le boot)",
                    &[("delay", &boot_delay.to_string())],
                )
            }
        } else {
            format!("OnCalendar={schedule}")
        };
        let vis = join_path(&self.root, &self.snapdir);
        let prompt = tf(
            "Créer un timer systemd '{tag}'\n  Planning : {plan}\n  Rétention: {keep} instantané(s)\n  Cible    : {vis}",
            &[
                ("tag", &tag),
                ("plan", &plan_desc),
                ("keep", &keep.to_string()),
                ("vis", &vis),
            ],
        );
        if !self.ask_yn(&prompt) {
            println!("{}", t("Annulé."));
            self.pause();
            return;
        }
        write_timer_files(
            &self.root,
            &self.snapdir,
            &tag,
            &schedule,
            keep,
            boot,
            boot_delay,
            once_per_day,
        );
        // Créer une automatisation doit aussi mettre le menu de démarrage à
        // jour pour les instantanés DÉJÀ présents (comme au démarrage du TUI).
        self.sync_boot();
        self.pause();
    }

    fn do_remove_timer(&mut self) {
        let timers = list_timers();
        if timers.is_empty() {
            println!("{}", t("Aucun timer btrfsmgr actif."));
            self.pause();
            return;
        }
        let items: Vec<String> = timers
            .iter()
            .map(|t| format!("{}  (next={})", t.timer, t.next))
            .collect();
        let title = tf(" Supprimer une automatisation — {root}", &[("root", &self.root)]);
        let Some(idx) = self.choose(
            &title,
            &items,
            &t("↑/↓ naviguer · Entrée ou clic souris choisir · Échap annuler"),
            0,
            true,
            &HashMap::new(),
        ) else {
            println!("{}", t("Annulé."));
            self.pause();
            return;
        };
        let timer = timers[idx].timer.clone();
        let tag = timer_tag(&timer, &self.root, &self.snapdir);
        if !self.ask_yn(&tf("Supprimer l'automatisation '{tag}' ?", &[("tag", &tag)])) {
            println!("{}", t("Annulé."));
            self.pause();
            return;
        }
        delete_timer_files(&self.root, &self.snapdir, &tag);
        self.pause();
    }

    fn do_restore(&mut self) {
        let snaps = list_snapshots(&self.root, &self.snapdir);
        if snaps.is_empty() {
            println!("{}", t("Aucun snapshot à restaurer."));
            self.pause();
            return;
        }
        let snaps_r: Vec<Snapshot> = snaps.into_iter().rev().collect();
        let items: Vec<String> = snaps_r
            .iter()
            .map(|s| format!("{}  (gen {})", s.path, s.generation))
            .collect();
        let vis = join_path(&self.root, &self.snapdir);
        let title = tf(" Restaurer — {vis}", &[("vis", &vis)]);
        let Some(idx) = self.choose(
            &title,
            &items,
            &t("↑/↓ naviguer · Entrée ou clic souris choisir · Échap annuler"),
            0,
            true,
            &HashMap::new(),
        ) else {
            println!("{}", t("Annulé."));
            self.pause();
            return;
        };
        let snap = &snaps_r[idx];
        let at = boot_subvol(&self.root).unwrap_or_else(|| t("(racine du FS)"));
        let mut body = tf(
            "Restaurer {snap} en premier plan par deux mv (CoW) ?\n\n  1. mv  {at} → {snapdir}/{at}_old-<date-heure>  (conserve l'actuel)\n  2. mv  {snapdir}/{snap} → {at}  (le snapshot devient le volume de boot)\n\nLe sous-volume courant n'est pas perdu: il reste dans {snapdir}/.\nRedémarrez ensuite pour booter sur la version restaurée.",
            &[("snap", &snap.path), ("at", &at), ("snapdir", &self.snapdir)],
        );
        let kinfo = snapshot_kernel_info(&self.root, &self.snapdir, &snap.path);
        if kinfo.mismatch {
            let ks = if kinfo.snapshot_kernels.is_empty() {
                "?".to_string()
            } else {
                kinfo.snapshot_kernels.join(", ")
            };
            body = format!(
                "{}\n\n{body}",
                tf(
                    KERNEL_MISMATCH_WARNING,
                    &[("cur", &kinfo.current), ("snaps", &ks)]
                )
            );
        }
        let action = tf("Restaurer « {name} »", &[("name", &snap.path)]);
        let Some(boot) = self.ask_boot_action(&action, &body) else {
            println!("{}", t("Annulé."));
            self.pause();
            return;
        };
        restore_snapshot(&self.root, &self.snapdir, &snap.path);
        if boot == "adapt" {
            self.sync_boot();
        }
        if self.ask_yn(&t("\nRedémarrer maintenant pour booter sur la version restaurée ?")) {
            runv(&sargs(&["systemctl", "reboot"]), false);
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        self.pause();
    }

    fn do_delete(&mut self) {
        let snaps = list_snapshots(&self.root, &self.snapdir);
        if snaps.is_empty() {
            println!("{}", t("Aucun snapshot à supprimer."));
            self.pause();
            return;
        }
        let snaps_r: Vec<Snapshot> = snaps.into_iter().rev().collect();
        let items: Vec<String> = snaps_r
            .iter()
            .map(|s| format!("{}  (gen {})", s.path, s.generation))
            .collect();
        let vis = join_path(&self.root, &self.snapdir);
        let title = tf(" Supprimer — {vis}", &[("vis", &vis)]);
        let Some(sel) = self.choose_multi(
            &title,
            &items,
            &t("Espace cocher · ↑/↓ naviguer · Entrée supprimer · Échap retour"),
        ) else {
            return;
        };
        if sel.is_empty() {
            println!("{}", t("Aucune sélection valide."));
            self.pause();
            return;
        }
        let targets: Vec<&Snapshot> = sel.iter().map(|&i| &snaps_r[i]).collect();
        let recap: Vec<String> = targets
            .iter()
            .map(|t| format!("{}   (gen {})", t.path, t.generation))
            .collect();
        let body = format!(
            "{}\n\n{}",
            t("Ces instantanés seront SUPPRIMÉS définitivement."),
            recap.join("\n")
        );
        let action = tf(
            "Supprimer {n} snapshot(s)",
            &[("n", &targets.len().to_string())],
        );
        let Some(boot) = self.ask_boot_action(&action, &body) else {
            println!("{}", t("Annulé."));
            self.pause();
            return;
        };
        let mut ok = 0;
        for s in &targets {
            let (rc, out) = delete_subvol(&s.full);
            if rc == 0 {
                ok += 1;
                sayln!("  Supprimé: {}", s.full);
            } else {
                sayln!("  Échec: {} — {}", s.full, out.trim());
            }
        }
        sayln!("{ok}/{} supprimé(s).", targets.len());
        if boot == "adapt" {
            self.sync_boot();
        }
        self.pause();
    }

    fn do_boot_menu(&mut self) {
        println!("{}", t("Menu systemd-boot au démarrage"));
        println!("{}", t("  clr-boot-manager set-timeout <secondes>"));
        println!("{}", t("    N > 0 : le menu est affiché pendant N secondes"));
        println!("{}", t("    0     : le menu est masqué (boot direct)"));
        let raw = self.ask(
            &t("Nombre de secondes d'affichage (0 = masquer le menu)"),
            "5",
        );
        let Ok(secs) = raw.trim().parse::<i64>() else {
            println!("{}", t("Nombre invalide"));
            self.pause();
            return;
        };
        if secs < 0 {
            println!("{}", t("Nombre invalide (0 ou plus)"));
            self.pause();
            return;
        }
        if !self.ask_yn(&tf(
            "Appliquer: clr-boot-manager set-timeout {secs}",
            &[("secs", &secs.to_string())],
        )) {
            println!("{}", t("Annulé."));
            self.pause();
            return;
        }
        let (rc, out) = runv(&sargs(&["clr-boot-manager", "set-timeout", &secs.to_string()]), false);
        if rc == 0 {
            sayln!("  ✓ Done (timeout={secs}s).");
        } else {
            sayln!("  ✗ Échec: {}", out.trim());
        }
        self.pause();
    }

    fn do_retention(&mut self) {
        let keep = {
            let k = self.ask(
                &t("Conserver les N plus récents snapshots (0=tous)"),
                "7",
            );
            match k.trim().parse::<i64>() {
                Ok(v) => v.max(0),
                Err(_) => {
                    println!("{}", t("Nombre invalide"));
                    self.pause();
                    return;
                }
            }
        };
        let snaps = list_snapshots(&self.root, &self.snapdir);
        let to_del: Vec<&Snapshot> = if keep <= 0 || snaps.len() as i64 <= keep {
            Vec::new()
        } else {
            snaps[..(snaps.len() - keep as usize)].iter().collect()
        };
        if to_del.is_empty() {
            println!("{}", t("Aucun snapshot à supprimer (déjà ≤ N)."));
            self.pause();
            return;
        }
        let body = format!(
            "{}\n\n{}",
            t("Les snapshots listés seront SUPPRIMÉS."),
            to_del.iter().map(|s| s.path.as_str()).collect::<Vec<_>>().join("\n")
        );
        let action = tf("Rétention : garder {keep}", &[("keep", &keep.to_string())]);
        let Some(boot) = self.ask_boot_action(&action, &body) else {
            println!("{}", t("Annulé."));
            self.pause();
            return;
        };
        prune_snapshots(&self.root, &self.snapdir, keep);
        if boot == "adapt" {
            self.sync_boot();
        }
        self.pause();
    }

    fn do_info(&mut self) {
        let bl = detect_bootloader();
        let dv = default_subvol(&self.root);
        let timers = list_timers();
        let snaps = list_snapshots(&self.root, &self.snapdir);
        // Alignement CALCULÉ (et non par espaces codés en dur dans les
        // chaînes) : les deux-points restent sur la même colonne en FR comme
        // en EN, quel que soit le libellé.
        let rows: [(&str, String); 5] = [
            ("Version", format!("{APP} {VERSION}")),
            ("Chargeur de boot", bootloader_display(bl)),
            (
                "Subvolume par défaut",
                if dv.full.is_empty() {
                    "?".to_string()
                } else {
                    dv.full.clone()
                },
            ),
            ("Snapshots existants", snaps.len().to_string()),
            ("Timers actifs", timers.len().to_string()),
        ];
        let mut items: Vec<String> = rows
            .iter()
            .map(|(label, value)| format!("{:<22}: {}", t(label), value))
            .collect();
        for t in &timers {
            items.push(format!("  timer: {}  next={}", t.timer, t.next));
        }
        let (_rc, fs_out) = runv(&sargs(&["df", "-h", &self.root]), false);
        items.push(t("Espace disque:"));
        for ln in fs_out.lines() {
            items.push(format!("  {ln}"));
        }
        self.render(&t(" Infos système"), &items, "");
        self.pause();
    }

    pub fn run(&mut self) -> i32 {
        if !is_root() && std::env::var("BTRFSMGR_ALLOW_NONROOT").is_err() {
            println!("{}", t("ERREUR: exécuter en root:  sudo btrfsmgr"));
            return 1;
        }
        // Réécrire le script d'instantané au démarrage (root) : après une
        // mise à jour du paquet, le script déjà installé (utilisé par les
        // timers) bénéficie des correctifs sans recréer les automatisations.
        if is_root() {
            install_snap_script();
        }
        if !btrfs_available() {
            println!(
                "{}",
                t("ERREUR: 'btrfs' introuvable. Installez btrfs-progs.")
            );
            return 1;
        }
        if let Err(e) = ensure_snapdir(&self.root, &self.snapdir) {
            sayln!(
                "{}",
                tf("Warning: ensure_snapdir ignorée: {err}", &[("err", &e)])
            );
        }
        loop {
            let ch = self.main_menu();
            match ch.as_str() {
                "q" | "0" | "10" => {
                    println!("{}", t("Au revoir."));
                    break;
                }
                "s" | "1" => self.do_create(),
                "l" | "2" => self.do_list(),
                "r" | "3" => self.do_restore(),
                "d" | "a" | "4" => self.do_delete(),
                "p" | "5" => self.do_schedule(),
                "x" | "6" => self.do_remove_timer(),
                "7" => self.do_retention(),
                "i" | "8" => self.do_info(),
                "m" | "9" => self.do_boot_menu(),
                _ => {
                    println!("{}", t("Choix invalide."));
                    self.pause();
                }
            }
        }
        0
    }
}


