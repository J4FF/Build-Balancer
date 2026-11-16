// pages -- what the window shows: one function per page, the questions it asks, and the
// made-up data of --demo

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use bb_core::proto::{self, WorkerStatus};
use bb_core::util::{ago_text, dur_text, now, size_text};
use bb_main::conf::{JobRec, MainConfig, PairedWorker, Project, Task};
use bb_main::fleet::{self, Needs, WorkerView};
use bb_main::ops::{self, Pairing};
use bb_worker::cleanup;
use bb_worker::config::Config;
use ui::*;

use crate::id;
use crate::state::{self, Gui, Page};
use crate::tr::{t, tf};

fn card_begin(ui: &mut Ui, title: &str) {
    ui.box_begin(0, BOX_V | BOX_CARD | PAD_16, 10);
    if !title.is_empty() {
        ui.heading(2, TS_BODY_STRONG, title);
    }
}

fn card_end(ui: &mut Ui) {
    ui.box_end();
    ui.spacer(12);
}

fn busy_row(ui: &mut Ui, id: u32, text: &str) {
    ui.box_begin(0, BOX_H | BOX_XCENTER, 8);
    ui.spinner(id, 16);
    ui.text(TS_BODY | TX_SECONDARY, text);
    ui.box_end();
}

/// "vor 3 Tagen" for a time in the past
fn ago(time: i64) -> String {
    if time <= 0 {
        return String::from(t("noch nie"));
    }
    let d = (now() - time).max(0);
    let n = |v: i64| format!("{}", v);
    match d {
        0..=119 => String::from(t("gerade eben")),
        120..=7199 => tf("vor {} Minuten", &[&n(d / 60)]),
        7200..=172_799 => tf("vor {} Stunden", &[&n(d / 3600)]),
        _ => tf("vor {} Tagen", &[&n(d / 86_400)]),
    }
}

fn state_badge(state: &str) -> (u32, &'static str) {
    match state {
        proto::ST_DONE => (BADGE_OK, t("fertig")),
        proto::ST_FAILED => (BADGE_CRIT, t("fehlgeschlagen")),
        proto::ST_ERROR => (BADGE_CRIT, t("Fehler")),
        proto::ST_KILLED => (BADGE_WARN, t("beendet (Limit)")),
        proto::ST_CANCELLED => (BADGE_NEUTRAL, t("abgebrochen")),
        proto::ST_RUNNING => (BADGE_ACCENT, t("läuft")),
        proto::ST_SYNCING => (BADGE_INFO, t("Dateien kommen an")),
        proto::ST_COLLECTING => (BADGE_INFO, t("Ergebnisse gehen zurück")),
        proto::ST_QUEUED => (BADGE_NEUTRAL, t("wartet")),
        _ => (BADGE_NEUTRAL, t("abgeschickt")),
    }
}

// ------------------------------------------------------------------------- connect

pub fn connect(g: &mut Gui, ui: &mut Ui) {
    ui.page_header(t("Verbinden"), t("Dieses Gerät mit dem Relay und den anderen Geräten zusammenbringen."));
    ui.spacer(12);
    ui.next_size(520, 0);
    ui.segmented(id::START, &mut g.role, &[t("Dieses Gerät verleiht Leistung"), t("Das ist mein Hauptgerät")]);
    ui.spacer(12);
    let worker = g.role == 0;
    card_begin(ui, if worker { t("Mit einem Hauptgerät koppeln") } else { t("Hauptgerät einrichten") });
    ui.text(
        TS_BODY | TX_SECONDARY | TX_WRAP,
        if worker {
            t("Auf dem Hauptgerät unter „Geräte“ auf „Gerät hinzufügen“ klicken (oder `bb pair` ausführen). Dort stehen die Adresse des Relays und ein Code, der einmal gilt.")
        } else {
            t("Das Relay (bb-relay) zeigt beim Start die Zeile `bb setup ADRESSE SCHLÜSSEL`. Adresse und Schlüssel hier eintragen – oder die ganze Zeile in das erste Feld einfügen.")
        },
    );
    ui.gridbox_begin(id::START + 10, &[170, -1], 0, 8);
    ui.text(TS_BODY, t("Adresse des Relays"));
    ui.textbox(id::START + 11, Some("relay.example.org:7700"), &mut g.f_addr, 400);
    ui.text(TS_BODY, if worker { t("Code") } else { t("Einrichtungsschlüssel") });
    ui.textbox(id::START + 12, Some(if worker { "K7QF-2M9X-PD4T" } else { "" }), &mut g.f_secret, 400);
    ui.text(TS_BODY, t("Name dieses Geräts"));
    ui.textbox(id::START + 13, None, &mut g.f_name, 60);
    ui.gridbox_end();
    ui.box_begin(0, BOX_H | BOX_XCENTER, 12);
    let busy = g.connect_task.is_some();
    if busy {
        ui.next_flags(ST_DISABLED);
    }
    if ui.accent_button(id::START + 14, if worker { t("Koppeln") } else { t("Einrichten") }) & EV_ACT != 0 {
        g.connect(ui);
    }
    if busy {
        busy_row(ui, id::START + 15, if worker { t("Wartet auf das Hauptgerät …") } else { t("Verbindet …") });
    }
    ui.box_end();
    card_end(ui);

    card_begin(ui, t("Dieses Gerät ist"));
    match &g.wcfg {
        Some(c) => ui.key_value(t("Leihgerät"), &tf("„{}“, gekoppelt mit {}", &[&c.name, &c.main_name]), 130),
        None => ui.key_value(t("Leihgerät"), t("nein"), 130),
    }
    match &g.main {
        Some(m) => ui.key_value(t("Hauptgerät"), &tf("„{}“ am Relay {}", &[&m.name, &m.relay]), 130),
        None => ui.key_value(t("Hauptgerät"), t("nein"), 130),
    }
    card_end(ui);
}

// ------------------------------------------------------------------------- this machine

pub fn worker(g: &mut Gui, ui: &mut Ui) {
    let Some(cfg) = g.wcfg.clone() else {
        ui.empty_state(IC_POWER, t("Dieses Gerät verleiht noch nichts"), t("Mit einem Hauptgerät koppeln, dann kann es Builds hierher schicken. Wie viel dieser Rechner hergibt, bestimmst du."));
        ui.box_begin(0, BOX_H | BOX_CENTER, 0);
        if ui.accent_button(id::WORKER, t("Koppeln …")) & EV_ACT != 0 {
            g.role = 0;
            g.go(Page::Connect);
        }
        ui.box_end();
        return;
    };
    let status = g.wstatus.clone();
    let online = g.demo || g.engine.as_ref().is_some_and(|e| e.online());

    ui.box_begin(0, BOX_H | BOX_XCENTER, 12);
    ui.next_size(-1, 0);
    ui.box_begin(0, BOX_V, 0);
    ui.page_header(&cfg.name, &tf("verleiht Leistung an „{}“", &[&cfg.main_name]));
    ui.box_end();
    let (kind, label) = if g.lend == 0 {
        (BADGE_NEUTRAL, t("angehalten"))
    } else if online {
        (BADGE_OK, t("verbunden"))
    } else {
        (BADGE_WARN, t("Relay nicht erreichbar"))
    };
    ui.badge(kind, label);
    let mut lend = g.lend;
    if ui.switch(id::WORKER + 1, t("Leistung verleihen"), &mut lend) & EV_ACT != 0 {
        g.set_lending(lend != 0);
    }
    ui.box_end();
    ui.spacer(12);
    if !g.engine_error.is_empty() && ui.infobar(id::WORKER + 2, IB_ERROR, t("Der Worker läuft nicht"), &g.engine_error.clone()) & EV_ACT != 0 {
        g.engine_error.clear();
    }

    // what is lent
    card_begin(ui, t("Was dieses Gerät verleiht"));
    let cpus = plat::cpu_count().max(1) as i32;
    let ram_total = plat::mem_total().max(1 << 30);
    let half_gib = 1u64 << 29;
    let mut c = cfg.clone();
    let (mut cpu, mut ram, mut disk) = (c.cpu as i32, (c.ram / half_gib) as i32, (c.disk >> 30) as i32);
    // The sliders count in steps (half a GiB, a GiB).  Only a value its widget changed is
    // taken over: one set elsewhere to something in between (bb-worker config --disk 50%) must
    // not be rounded, and marked as changed here, by merely being shown.
    let (cpu0, ram0, disk0) = (cpu, ram, disk);
    let free_gib = plat::fs::free_space(&g.worker_dir).map_or(500, |f| (f.0 >> 30) as i32);
    let disk_max = (free_gib + (g.projects.iter().map(|p| p.bytes).sum::<u64>() >> 30) as i32).max(disk).max(2);
    let mut changed = 0u32;
    ui.gridbox_begin(id::WORKER + 10, &[150, -1, 190], 0, 10);
    ui.text(TS_BODY, t("Prozessoren"));
    changed |= ui.slider(id::WORKER + 11, &mut cpu, &Range::new(1, cpus, 1, RG_TIP));
    ui.text(TS_BODY | TX_SECONDARY, &tf("{} von {}", &[&format!("{}", cpu), &format!("{}", cpus)]));
    ui.text(TS_BODY, t("Arbeitsspeicher"));
    changed |= ui.slider(id::WORKER + 12, &mut ram, &Range::new(1, (ram_total / half_gib) as i32, 1, 0));
    ui.text(TS_BODY | TX_SECONDARY, &tf("{} von {}", &[&size_text(if ram == ram0 { c.ram } else { ram as u64 * half_gib }), &size_text(ram_total)]));
    ui.text(TS_BODY, t("Festplatte"));
    changed |= ui.slider(id::WORKER + 13, &mut disk, &Range::new(1, disk_max, 1, 0));
    ui.text(TS_BODY | TX_SECONDARY, &tf("{} für Projekte", &[&size_text(if disk == disk0 { c.disk } else { (disk as u64) << 30 })]));
    ui.gridbox_end();
    let low0 = (c.priority != "normal") as i32;
    let mut low = low0;
    changed |= ui.switch(id::WORKER + 14, t("Rücksicht nehmen: Aufträge laufen mit niedriger Priorität"), &mut low) << 1;
    ui.box_begin(0, BOX_H | BOX_XCENTER, 8);
    let (mut jobs, mut keep) = (c.jobs.max(1) as i32, c.keep_days as i32);
    let (jobs0, keep0) = (jobs, keep);
    ui.text(TS_BODY, t("Aufträge gleichzeitig"));
    ui.next_size(110, 0);
    changed |= ui.number_box(id::WORKER + 15, &mut jobs, &Range::new(1, 16, 1, 0));
    ui.spacer(16);
    ui.text(TS_BODY, t("Unbenutzte Projekte entfernen nach Tagen (0 = nie)"));
    ui.next_size(110, 0);
    changed |= ui.number_box(id::WORKER + 16, &mut keep, &Range::new(0, 365, 1, 0));
    ui.box_end();
    if cpu != cpu0 {
        c.cpu = cpu as u32;
    }
    if ram != ram0 {
        c.ram = ram as u64 * half_gib;
    }
    if disk != disk0 {
        c.disk = (disk as u64) << 30;
    }
    if low != low0 {
        c.priority = String::from(if low != 0 { "low" } else { "normal" });
    }
    if jobs != jobs0 {
        c.jobs = jobs as u32;
    }
    if keep != keep0 {
        c.keep_days = keep as u32;
    }
    if c != cfg {
        g.wcfg = Some(c);
        g.offer_dirty = true;
    }
    // written when a slider is let go (or a switch flipped), not on every step of a drag
    if g.offer_dirty && changed & (EV_COMMIT | EV_ACT << 1) != 0 {
        g.offer_dirty = false;
        g.save_offer(ui);
    }
    card_end(ui);

    // what runs
    card_begin(ui, t("Gerade"));
    match status.as_ref().filter(|s| !s.running.is_empty() || !s.queued.is_empty()) {
        Some(s) => {
            for (i, b) in s.running.iter().chain(s.queued.iter()).enumerate() {
                ui.box_begin(0, BOX_H | BOX_XCENTER, 10);
                let (kind, label) = state_badge(&b.state);
                ui.badge(kind, label);
                ui.next_size(-1, 0);
                ui.text(TS_BODY | TX_ELLIPSIS, &format!("{} · {}", b.project, b.command));
                ui.text(TS_CAPTION | TX_SECONDARY, &tf("{} Prozessoren", &[&format!("{}", b.cpu)]));
                ui.box_end();
                let _ = i;
            }
        }
        None => ui.text(TS_BODY | TX_SECONDARY, if g.lend != 0 { t("Nichts läuft. Aufträge des Hauptgeräts starten von selbst.") } else { t("Angehalten: es kommen keine Aufträge an.") }),
    }
    card_end(ui);

    // the projects kept here: one click and one is gone
    ui.box_begin(0, BOX_V | BOX_CARD | PAD_16, 10);
    ui.box_begin(0, BOX_H | BOX_XCENTER, 8);
    ui.next_size(-1, 0);
    ui.heading(2, TS_BODY_STRONG, t("Projekte auf diesem Gerät"));
    let total: u64 = g.projects.iter().map(|p| p.bytes).sum();
    ui.text(TS_CAPTION | TX_SECONDARY, &tf("{} von {} belegt", &[&size_text(total), &size_text(cfg.disk)]));
    let cleaning = g.clean_task.is_some();
    if g.projects.is_empty() || cleaning {
        ui.next_flags(ST_DISABLED);
    }
    ui.next_icon(IC_DELETE);
    if ui.button(id::WORKER + 20, t("Alles entfernen")) & EV_ACT != 0 {
        g.remove_project(ui, "");
    }
    ui.box_end();
    if g.projects.is_empty() {
        ui.text(TS_BODY | TX_SECONDARY | TX_WRAP, t("Hier liegt nichts. Ein Projekt kommt mit seinem ersten Auftrag und bleibt für die nächsten – entfernen lässt es sich jederzeit mit einem Klick."));
    }
    let mut remove: Option<String> = None;
    for (i, p) in g.projects.iter().enumerate() {
        let busy = status.as_ref().is_some_and(|s| s.running.iter().any(|b| b.project == p.name));
        ui.box_begin(0, BOX_H | BOX_XCENTER, 10);
        ui.icon(IC_FOLDER, 16);
        ui.next_size(-1, 0);
        ui.text(TS_BODY | TX_ELLIPSIS, &p.name);
        ui.text(TS_CAPTION | TX_SECONDARY, &tf("{} · zuletzt benutzt {}", &[&size_text(p.bytes), &ago(p.used)]));
        if busy || cleaning {
            ui.next_flags(ST_DISABLED);
        }
        if ui.button(id::PROJECT_ROW + i as u32 * 4, t("Entfernen")) & EV_ACT != 0 {
            remove = Some(p.id.clone());
        }
        if busy {
            ui.tooltip(t("Hier läuft gerade ein Auftrag."));
        }
        ui.box_end();
    }
    if let Some(id) = remove {
        g.remove_project(ui, &id);
    }
    if cleaning {
        busy_row(ui, id::WORKER + 21, t("Wird entfernt …"));
    }
    ui.box_end();
    ui.spacer(12);

    // the tools jobs can use
    card_begin(ui, t("Werkzeuge für Aufträge"));
    match status.as_ref().filter(|s| !s.tools.is_empty()) {
        Some(s) => {
            ui.box_begin(0, BOX_H | BOX_XCENTER | BOX_WRAP, 6);
            for tool in &s.tools {
                ui.badge(BADGE_NEUTRAL, &tool.name);
            }
            ui.box_end();
        }
        None => ui.text(TS_BODY | TX_SECONDARY, if g.lend != 0 { t("Wird gesucht …") } else { t("Werden gesucht, sobald das Gerät Leistung verleiht.") }),
    }
    card_end(ui);

    ui.box_begin(0, BOX_H | BOX_END, 0);
    ui.next_flags(ST_SUBTLE);
    if ui.button(id::WORKER + 30, t("Kopplung aufheben und alles entfernen …")) & EV_ACT != 0 {
        g.ask_leave.set(1);
    }
    ui.box_end();
}

// ------------------------------------------------------------------------- machines

fn need_main(g: &mut Gui, ui: &mut Ui, base: u32) -> bool {
    if g.is_main() {
        return false;
    }
    ui.empty_state(IC_GRID, t("Das ist kein Hauptgerät"), t("Ein Hauptgerät verteilt Builds auf die Geräte, die Leistung verleihen. Dazu braucht es ein Relay (bb-relay) und dessen Einrichtungsschlüssel."));
    ui.box_begin(0, BOX_H | BOX_CENTER, 0);
    if ui.accent_button(base, t("Als Hauptgerät einrichten …")) & EV_ACT != 0 {
        g.role = 1;
        g.go(Page::Connect);
    }
    ui.box_end();
    true
}

fn meter_cell(ui: &mut Ui, id: u32, caption: &str, value: &str, used: u64, max: u64) {
    ui.box_begin(0, BOX_V, 4);
    ui.text(TS_CAPTION | TX_SECONDARY, caption);
    ui.text(TS_BODY, value);
    let kind = if max > 0 && used * 10 >= max * 9 { BADGE_WARN } else { BADGE_ACCENT };
    ui.next_size(-1, 6);
    ui.meter(id, used.min(max) as i64, max.max(1) as i64, kind);
    ui.box_end();
}

pub fn fleet(g: &mut Gui, ui: &mut Ui) {
    if need_main(g, ui, id::FLEET) {
        return;
    }
    let relay = g.main.as_ref().map(|m| m.relay.clone()).unwrap_or_default();
    ui.box_begin(0, BOX_H | BOX_XCENTER, 8);
    ui.next_size(-1, 0);
    ui.box_begin(0, BOX_V, 0);
    ui.page_header(t("Geräte"), &tf("Relay {}", &[&relay]));
    ui.box_end();
    ui.next_icon(IC_REFRESH);
    if ui.button(id::FLEET + 1, t("Aktualisieren")) & EV_ACT != 0 {
        g.refresh_fleet(ui);
    }
    if g.pairing.is_some() || g.pair_begin.is_some() {
        ui.next_flags(ST_DISABLED);
    }
    ui.next_icon(IC_ADD);
    if ui.accent_button(id::FLEET + 2, t("Gerät hinzufügen")) & EV_ACT != 0 {
        g.pair(ui);
    }
    ui.box_end();
    ui.spacer(12);

    if g.pair_begin.is_some() {
        busy_row(ui, id::FLEET + 3, t("Der Code wird vorbereitet …"));
        ui.spacer(12);
    }
    if let Some(p) = g.pairing.clone() {
        ui.box_begin(0, BOX_V | BOX_CARD | PAD_16, 10);
        ui.heading(2, TS_BODY_STRONG, t("Neues Gerät koppeln"));
        ui.text(TS_BODY | TX_WRAP, t("Auf dem anderen Gerät Build Balancer starten, „Dieses Gerät verleiht Leistung“ wählen und eintragen:"));
        ui.gridbox_begin(id::FLEET + 10, &[170, -1], 0, 6);
        ui.text(TS_BODY | TX_SECONDARY, t("Adresse des Relays"));
        ui.text(TS_SUBTITLE, &p.relay);
        ui.text(TS_BODY | TX_SECONDARY, t("Code"));
        ui.text(TS_TITLE, &p.code);
        ui.gridbox_end();
        ui.text(TS_CAPTION | TX_SECONDARY | TX_WRAP, &tf("Ohne Fenster geht es mit einer Zeile im Terminal: {}", &[&p.command]));
        ui.box_begin(0, BOX_H | BOX_XCENTER, 10);
        ui.next_icon(IC_COPY);
        if ui.button(id::FLEET + 11, t("Zeile kopieren")) & EV_ACT != 0 {
            ui.clip_set(CLIP_TEXT, p.command.as_bytes());
            ui.toast(IB_INFO, t("Kopiert"), &p.command, 3000);
        }
        if ui.button(id::FLEET + 12, t("Abbrechen")) & EV_ACT != 0 {
            g.pair_cancel();
        }
        ui.spinner(id::FLEET + 13, 16);
        let left = p.deadline.saturating_sub(bb_core::util::tick_ms()) / 60_000 + 1;
        ui.text(TS_BODY | TX_SECONDARY, &tf("Wartet auf das Gerät … der Code gilt noch {} Minuten, einmal.", &[&format!("{}", left.min(p.minutes as u64))]));
        ui.box_end();
        // a machine that has no Build Balancer yet: one line fetches it, pairs and starts lending
        ui.text(TS_CAPTION | TX_SECONDARY | TX_WRAP, t("Noch kein Build Balancer auf dem Gerät? Eine Zeile lädt es herunter und koppelt gleich:"));
        ui.box_begin(0, BOX_H | BOX_XCENTER, 10);
        for (k, label, line) in [(14, t("Für Linux und macOS kopieren"), p.install_line_sh()), (15, t("Für Windows kopieren"), p.install_line_powershell())] {
            if ui.button(id::FLEET + k, label) & EV_ACT != 0 {
                ui.clip_set(CLIP_TEXT, line.as_bytes());
                ui.toast(IB_INFO, t("Kopiert"), &line, 4000);
            }
        }
        ui.box_end();
        ui.box_end();
        ui.spacer(12);
    }
    if !g.fleet_error.is_empty() {
        ui.infobar(id::FLEET + 4, IB_WARNING, t("Das Relay antwortet nicht"), &g.fleet_error.clone());
        ui.spacer(12);
    }
    if g.fleet.is_empty() {
        if g.fleet_loaded || g.demo {
            ui.empty_state(IC_ADD_CIRCLE, t("Noch kein Gerät"), t("„Gerät hinzufügen“ zeigt einen Code. Auf dem anderen Rechner Build Balancer starten und ihn eintragen – mehr ist nicht nötig."));
        } else {
            busy_row(ui, id::FLEET + 5, t("Geräte werden geladen …"));
        }
        return;
    }
    let views: Vec<WorkerView> = core::mem::take(&mut g.fleet);
    for (i, v) in views.iter().enumerate() {
        worker_card(g, ui, id::FLEET_ROW + i as u32 * 64, v);
    }
    g.fleet = views;
}

fn worker_card(g: &mut Gui, ui: &mut Ui, base: u32, v: &WorkerView) {
    ui.box_begin(0, BOX_V | BOX_CARD | PAD_16, 10);
    ui.box_begin(0, BOX_H | BOX_XCENTER, 8);
    let (kind, label): (u32, String) = if !v.admitted {
        (BADGE_CRIT, String::from(t("am Relay entfernt")))
    } else if v.online {
        (BADGE_OK, String::from(t("online")))
    } else if v.idle_secs < 0 {
        (BADGE_NEUTRAL, String::from(t("noch nie gesehen")))
    } else {
        (BADGE_NEUTRAL, tf("offline seit {}", &[&dur_text(v.idle_secs as u64 * 1000)]))
    };
    ui.status_dot(kind);
    ui.text(TS_SUBTITLE, &v.paired.name);
    ui.badge(kind, &label);
    ui.spacer(-1);
    let model = v.status.as_ref().map(|s| s.cpu_model.as_str()).filter(|m| !m.is_empty()).unwrap_or("");
    ui.text(TS_CAPTION | TX_SECONDARY | TX_ELLIPSIS, &format!("{} · {}{}{}", v.paired.os, v.paired.arch, if model.is_empty() { "" } else { " · " }, model));
    ui.box_end();

    // its version, and one click to bring an older one up to this machine's
    if let Some(s) = v.status.as_ref().filter(|s| !s.version.is_empty()) {
        ui.box_begin(0, BOX_H | BOX_XCENTER, 10);
        ui.text(TS_CAPTION | TX_SECONDARY, &tf("Version {}", &[&s.version]));
        if bb_update::newer(bb_core::VERSION, &s.version) {
            ui.next_flags(ST_COMPACT);
            ui.next_icon(IC_DOWNLOAD);
            if ui.button(base + 41, t("Update einspielen")) & EV_ACT != 0 {
                let worker = v.paired.clone();
                g.act(ui, move |ctx| {
                    ops::ask_update(ctx, &worker)?;
                    Ok(tf("{} holt die neue Version, sobald dort kein Auftrag läuft, und meldet sich danach wieder.", &[&worker.name]))
                });
            }
        }
        ui.box_end();
    }

    if let Some(s) = &v.status {
        let n = |x: u32| format!("{}", x);
        ui.gridbox_begin(base + 1, &[-1, -1, -1], 0, 16);
        meter_cell(ui, base + 2, t("Prozessoren"), &tf("{} von {} frei · {} im Gerät", &[&n(s.cpu_idle()), &n(s.offer.cpu), &n(s.cpu_total)]), s.cpu_reserved as u64, s.offer.cpu as u64);
        meter_cell(ui, base + 3, t("Arbeitsspeicher"), &tf("{} von {} frei", &[&size_text(s.ram_idle()), &size_text(s.offer.ram)]), s.ram_reserved, s.offer.ram);
        meter_cell(ui, base + 4, t("Festplatte für Projekte"), &tf("{} von {} belegt", &[&size_text(s.disk_used), &size_text(s.offer.disk)]), s.disk_used, s.offer.disk);
        ui.gridbox_end();
        if !s.tools.is_empty() {
            ui.box_begin(0, BOX_H | BOX_XCENTER | BOX_WRAP, 6);
            ui.text(TS_CAPTION | TX_SECONDARY, t("Werkzeuge"));
            for tool in &s.tools {
                ui.badge(BADGE_NEUTRAL, &tool.name);
            }
            for tag in &s.tags {
                ui.badge(BADGE_INFO, tag);
            }
            ui.box_end();
        }
        for b in s.running.iter().chain(s.queued.iter()) {
            ui.box_begin(0, BOX_H | BOX_XCENTER, 10);
            let (kind, label) = state_badge(&b.state);
            ui.badge(kind, label);
            ui.next_size(-1, 0);
            ui.text(TS_BODY | TX_ELLIPSIS, &format!("{} · {}", b.project, b.command));
            ui.box_end();
        }
        // what it keeps of which project, each with its one click
        for (k, p) in cleanup::group(&s.workspaces).iter().enumerate().take(12) {
            ui.box_begin(0, BOX_H | BOX_XCENTER, 10);
            ui.icon(IC_FOLDER, 16);
            ui.next_size(-1, 0);
            ui.text(TS_BODY | TX_ELLIPSIS, &p.name);
            ui.text(TS_CAPTION | TX_SECONDARY, &tf("{} · zuletzt benutzt {}", &[&size_text(p.bytes), &ago(p.used)]));
            ui.next_flags(ST_COMPACT);
            if ui.button(base + 10 + k as u32, t("Entfernen")) & EV_ACT != 0 {
                let (worker, project, name) = (v.paired.clone(), p.id.clone(), p.name.clone());
                g.act(ui, move |ctx| {
                    ops::clean(ctx, &worker, &project, "")?;
                    Ok(tf("{} räumt „{}“ weg.", &[&worker.name, &name]))
                });
            }
            ui.box_end();
        }
    } else if v.admitted {
        ui.text(TS_BODY | TX_SECONDARY, t("Hat sich noch nicht gemeldet: läuft Build Balancer dort, und ist „Leistung verleihen“ eingeschaltet?"));
    }
    ui.box_begin(0, BOX_H | BOX_END, 0);
    ui.next_flags(ST_SUBTLE | ST_COMPACT);
    if ui.button(base + 40, t("Entkoppeln …")) & EV_ACT != 0 {
        g.unpair_name = v.paired.name.clone();
        g.ask_unpair.set(1);
    }
    ui.box_end();
    ui.box_end();
    ui.spacer(12);
}

// ------------------------------------------------------------------------- jobs

pub fn jobs(g: &mut Gui, ui: &mut Ui) {
    if need_main(g, ui, id::JOBS) {
        return;
    }
    ui.page_header(t("Aufträge"), t("Was dieses Hauptgerät an die Geräte geschickt hat."));
    ui.spacer(12);
    if g.jobs.is_empty() {
        ui.empty_state(IC_HISTORY, t("Noch kein Auftrag"), t("Unter „Projekt“ eine Aufgabe starten – oder im Terminal `bb run -- BEFEHL`. Auch was ein KI-Agent über `bb` startet, erscheint hier."));
        return;
    }
    let mut pick: Option<String> = None;
    ui.box_begin(0, BOX_V | BOX_CARD | PAD_8, 2);
    for (i, r) in g.jobs.iter().enumerate().take(40) {
        let (kind, label) = state_badge(&r.state);
        let what = if r.label.is_empty() { r.command.clone() } else { format!("{} · {}", r.label, r.command) };
        let aside = if proto::is_final(&r.state) { dur_text(r.duration_ms) } else { r.detail.chars().take(30).collect() };
        let d = CardDesc { title: &what, text: &tf("{} · {} · {}", &[&r.worker, &r.project, &ago(r.created)]), one_line: true, badge: Some((kind, label)), aside: &aside, ..Default::default() };
        ui.next_flags(ST_SUBTLE | if r.id == g.job_sel { ST_CHECKED } else { 0 });
        ui.next_size(-1, 0);
        if ui.action_card(id::JOB_ROW + i as u32, &r.id, &d) & EV_ACT != 0 {
            pick = Some(r.id.clone());
        }
    }
    ui.box_end();
    ui.spacer(12);
    if let Some(id) = pick {
        g.select_job(ui, &id);
    }
    let Some(r) = g.jobs.iter().find(|r| r.id == g.job_sel).cloned() else { return };
    job_detail(g, ui, &r);
}

fn job_detail(g: &mut Gui, ui: &mut Ui, r: &JobRec) {
    card_begin(ui, &format!("{} · {}", r.id, r.command));
    let fin = proto::is_final(&r.state);
    if !r.error.is_empty() {
        ui.infobar(id::JOBS + 1, if r.state == proto::ST_KILLED { IB_WARNING } else { IB_ERROR }, state_badge(&r.state).1, &r.error);
    }
    ui.key_value(t("Gerät"), &r.worker, 130);
    ui.key_value(t("Projekt"), &format!("{} ({})", r.project, r.slot), 130);
    if fin {
        ui.key_value(t("Ergebnis"), &tf("Exit-Code {} nach {}", &[&format!("{}", r.exit), &dur_text(r.duration_ms)]), 130);
        if r.peak_ram > 0 {
            ui.key_value(t("Speicher"), &tf("höchstens {}", &[&size_text(r.peak_ram)]), 130);
        }
        if !r.outputs.is_empty() {
            let out = if r.artifacts.files == 0 {
                String::from(t("keine Datei passte zu den Mustern"))
            } else if r.fetched_to.is_empty() {
                tf("{} Dateien, {} liegen am Relay", &[&format!("{}", r.artifacts.files), &size_text(r.artifacts.bytes)])
            } else {
                tf("{} Dateien, {} in {}", &[&format!("{}", r.artifacts.files), &size_text(r.artifacts.bytes), &r.fetched_to])
            };
            ui.key_value(t("Ergebnisdateien"), &out, 130);
        }
    } else if !r.detail.is_empty() {
        ui.key_value(t("Stand"), &r.detail, 130);
    }
    ui.box_begin(0, BOX_H | BOX_XCENTER, 8);
    let id_s = r.id.clone();
    if !fin {
        ui.next_icon(IC_STOP);
        if ui.button(id::JOBS + 2, t("Abbrechen")) & EV_ACT != 0 {
            g.act(ui, move |ctx| {
                let rec = ctx.find_job(&id_s)?;
                bb_main::jobs::cancel(ctx, &rec)?;
                Ok(tf("{} wird abgebrochen.", &[&rec.id]))
            });
        }
    } else {
        if !r.artifacts.is_empty() && r.fetched_to.is_empty() {
            let id2 = r.id.clone();
            ui.next_icon(IC_DOWNLOAD);
            if ui.button(id::JOBS + 3, t("Ergebnisse holen")) & EV_ACT != 0 {
                g.act(ui, move |ctx| {
                    let mut rec = ctx.find_job(&id2)?;
                    let files = bb_main::jobs::fetch(ctx, &mut rec, None)?;
                    Ok(tf("{} Dateien in {}", &[&format!("{}", files.len()), &rec.fetched_to]))
                });
            }
        }
        ui.next_flags(ST_SUBTLE);
        if ui.button(id::JOBS + 4, t("Aus der Liste nehmen")) & EV_ACT != 0 {
            g.job_sel.clear();
            g.act(ui, move |ctx| {
                for ext in ["json", "log", "lock"] {
                    plat::delete(&ctx.job_path(&id_s, ext));
                }
                Ok(String::from(t("Der Auftrag ist aus der Liste.")))
            });
        }
    }
    ui.box_end();
    ui.text(TS_CAPTION | TX_SECONDARY, t("Ausgabe (das Ende)"));
    ui.next_size(-1, 280);
    let mut log = if g.job_log.is_empty() { String::from(t("(noch keine Ausgabe)")) } else { g.job_log.clone() };
    ui.textbox(id::JOBS + 5, None, &mut log, (1 << 20) | TB_MULTI | TB_READONLY);
    card_end(ui);
}

// ------------------------------------------------------------------------- project

pub fn project(g: &mut Gui, ui: &mut Ui) {
    if need_main(g, ui, id::PROJECT) {
        return;
    }
    ui.page_header(t("Projekt"), t("Einen Ordner an ein Gerät schicken und dort bauen lassen."));
    ui.spacer(12);
    ui.box_begin(0, BOX_H | BOX_XCENTER, 8);
    ui.next_size(-1, 0);
    let mut shown = if g.project_dir.is_empty() { String::from(t("Noch kein Ordner gewählt")) } else { g.project_dir.clone() };
    ui.textbox(id::PROJECT + 1, None, &mut shown, 1024 | TB_READONLY);
    ui.next_icon(IC_FOLDER_OPEN);
    if ui.button(id::PROJECT + 2, t("Ordner wählen …")) & EV_ACT != 0 {
        g.fd_open.set(1);
    }
    ui.box_end();
    ui.spacer(12);
    let mut path = g.project_dir.clone();
    let open = g.fd_open.clone();
    if ui.filedlg(id::FILEDLG, FD_FOLDER, &open, &mut path, 1024, &[]) & EV_ACT != 0 && !path.is_empty() {
        g.open_project(path);
    }
    if g.project_dir.is_empty() {
        ui.empty_state(IC_FOLDER, t("Welches Projekt?"), t("Den Ordner wählen, der gebaut werden soll. Geschickt wird alles darin außer dem, was .gitignore und .bbignore ausnehmen."));
        return;
    }
    if !g.project_error.is_empty() {
        ui.infobar(id::PROJECT + 3, IB_ERROR, t("buildbalancer.ini ist nicht lesbar"), &g.project_error.clone());
        return;
    }
    let Some(p) = g.project.clone() else {
        card_begin(ui, t("Dieser Ordner ist noch kein Projekt"));
        ui.text(TS_BODY | TX_SECONDARY | TX_WRAP, t("Einrichten schreibt eine Datei hinein, buildbalancer.ini – sonst nichts. Darin stehen der Name des Projekts und, wenn du magst, Aufgaben: Befehle mit dem, was sie brauchen und was zurückkommen soll."));
        if ui.accent_button(id::PROJECT + 4, t("Als Projekt einrichten")) & EV_ACT != 0 {
            match ops::init_project(&g.project_dir, None) {
                Ok((_, _)) => {
                    let dir = g.project_dir.clone();
                    g.open_project(dir);
                }
                Err(e) => {
                    ui.toast(IB_ERROR, t("Nicht eingerichtet"), &e, 10_000);
                }
            }
        }
        card_end(ui);
        return;
    };

    // where it runs
    let mut names: Vec<String> = alloc::vec![String::from(t("Automatisch"))];
    names.extend(g.fleet.iter().map(|v| v.paired.name.clone()));
    let items: Vec<&str> = names.iter().map(String::as_str).collect();
    ui.box_begin(0, BOX_H | BOX_XCENTER, 10);
    ui.text(TS_BODY, t("Ausführen auf"));
    if g.run_on as usize >= items.len() {
        g.run_on = 0;
    }
    ui.segmented(id::PROJECT + 10, &mut g.run_on, &items);
    ui.box_end();
    ui.spacer(12);
    let busy = g.run_task.is_some();

    card_begin(ui, &tf("Aufgaben von „{}“", &[&p.name]));
    if p.tasks.is_empty() {
        ui.text(TS_BODY | TX_SECONDARY | TX_WRAP, t("Noch keine. In buildbalancer.ini einen Abschnitt [task.NAME] mit „run = BEFEHL“ anlegen – oder unten einen Befehl direkt ausführen."));
    }
    let mut start: Option<String> = None;
    for (i, task) in p.tasks.iter().enumerate() {
        task_row(ui, id::TASK_ROW + i as u32 * 2, task, task_warning(task, &g.fleet), busy, &mut start);
    }
    if busy {
        busy_row(ui, id::PROJECT + 11, t("Dateien werden verglichen und geschickt …"));
    }
    card_end(ui);
    if let Some(name) = start {
        g.run(ui, Some(name));
    }

    card_begin(ui, t("Einen Befehl ausführen"));
    ui.gridbox_begin(id::PROJECT + 20, &[170, -1], 0, 8);
    ui.text(TS_BODY, t("Befehl"));
    ui.textbox(id::PROJECT + 21, Some("cargo build --release"), &mut g.f_cmd, 2000);
    ui.text(TS_BODY, t("Zurückholen (Muster)"));
    ui.textbox(id::PROJECT + 22, Some("target/release/app ; build/**/*.exe"), &mut g.f_out, 1000);
    ui.gridbox_end();
    ui.box_begin(0, BOX_H, 0);
    if busy {
        ui.next_flags(ST_DISABLED);
    }
    ui.next_icon(IC_PLAY);
    if ui.accent_button(id::PROJECT + 23, t("Ausführen")) & EV_ACT != 0 {
        g.run(ui, None);
    }
    ui.box_end();
    card_end(ui);
}

/// What stands in a task's way before it is started: no machine can take it right now (None:
/// one can, or the machines are not known yet).  The usual reason gets its own words -- a tool
/// the task needs is on none of the machines.
fn task_warning(task: &Task, fleet: &[WorkerView]) -> Option<String> {
    let needs = Needs::parse(&task.needs).ok()?;
    let named = |v: &&WorkerView| task.worker.is_empty() || task.worker == "auto" || v.paired.id == task.worker || v.paired.name.eq_ignore_ascii_case(&task.worker);
    let asked: Vec<&WorkerView> = fleet.iter().filter(named).collect();
    if asked.is_empty() || asked.iter().any(|v| fleet::why_not(v, &needs, task.cpu, task.ram).is_none()) {
        return None;
    }
    let known: Vec<&WorkerStatus> = asked.iter().filter_map(|v| v.status.as_ref()).collect();
    if let Some(tool) = needs.tools.iter().find(|tool| !known.is_empty() && !known.iter().any(|s| s.has_tool(tool))) {
        return Some(tf("Kein Gerät hat „{}“. Dort installieren – die Geräte finden es danach von selbst.", &[tool]));
    }
    Some(String::from(t("Gerade kann kein Gerät diese Aufgabe übernehmen – unter „Geräte“ steht, was fehlt.")))
}

fn task_row(ui: &mut Ui, base: u32, task: &Task, warning: Option<String>, busy: bool, start: &mut Option<String>) {
    ui.box_begin(0, BOX_H | BOX_XCENTER, 10);
    ui.next_size(-1, 0);
    ui.box_begin(0, BOX_V, 2);
    ui.text(TS_BODY_STRONG, &task.name);
    let cmd = if task.script.is_empty() { task.argv.join(" ") } else { task.script.clone() };
    let mut line = cmd;
    if !task.needs.is_empty() {
        line += &tf("   ·   braucht {}", &[&task.needs]);
    }
    if !task.outputs.is_empty() {
        line += &tf("   ·   bringt {} zurück", &[&task.outputs.join(" ")]);
    }
    ui.text(TS_CAPTION | TX_SECONDARY | TX_ELLIPSIS, &line);
    if let Some(w) = &warning {
        ui.box_begin(0, BOX_H | BOX_XCENTER, 6);
        ui.badge(BADGE_WARN, t("fehlt"));
        ui.text(TS_CAPTION | TX_WRAP, w);
        ui.box_end();
    }
    ui.box_end();
    if busy {
        ui.next_flags(ST_DISABLED);
    }
    ui.next_icon(IC_PLAY);
    if ui.button(base, t("Starten")) & EV_ACT != 0 {
        *start = Some(task.name.clone());
    }
    ui.box_end();
}

// ------------------------------------------------------------------------- settings

pub fn settings(g: &mut Gui, ui: &mut Ui) {
    ui.page_header(t("Einstellungen"), t("Die Version dieses Programms und neuere."));
    ui.spacer(12);
    card_begin(ui, t("Version und Updates"));
    ui.box_begin(0, BOX_H | BOX_XCENTER, 10);
    ui.text(TS_BODY | TX_SECONDARY, t("Installierte Version"));
    ui.text(TS_BODY_STRONG, bb_core::VERSION);
    ui.box_end();

    let newer = g.upd_found.as_ref().filter(|c| c.newer).map(|c| c.latest.clone());
    if g.upd_install.is_some() {
        busy_row(ui, id::SETTINGS + 1, t("Die neue Version wird geholt, geprüft und eingespielt …"));
        ui.text(TS_CAPTION | TX_SECONDARY | TX_WRAP, t("Build Balancer startet danach von selbst neu und verbindet sich wieder."));
    } else if g.upd_waiting {
        busy_row(ui, id::SETTINGS + 1, t("Hier laufen gerade Aufträge. Das Update startet, sobald sie fertig sind."));
    } else if g.upd_check.is_some() {
        busy_row(ui, id::SETTINGS + 1, t("Es wird nach einer neueren Version gesucht …"));
    } else if let Some(v) = &newer {
        ui.box_begin(0, BOX_H | BOX_XCENTER, 10);
        ui.badge(BADGE_ACCENT, t("neu"));
        ui.text(TS_BODY, &tf("Version {} ist verfügbar.", &[v]));
        ui.box_end();
        ui.text(TS_CAPTION | TX_SECONDARY | TX_WRAP, t("Ein Klick holt sie, prüft sie und spielt sie ein. Build Balancer startet danach von selbst neu und verbindet sich wieder."));
    } else if g.upd_found.is_some() {
        ui.box_begin(0, BOX_H | BOX_XCENTER, 10);
        ui.badge(BADGE_OK, t("aktuell"));
        ui.text(TS_BODY, t("Das ist die neueste Version."));
        ui.box_end();
    }
    if !g.upd_error.is_empty() && ui.infobar(id::SETTINGS + 2, IB_WARNING, t("Die Suche hat nicht geklappt"), &g.upd_error.clone()) & EV_ACT != 0 {
        g.upd_error.clear();
    }

    let working = g.upd_install.is_some() || g.upd_check.is_some() || g.upd_waiting;
    ui.box_begin(0, BOX_H | BOX_XCENTER, 10);
    if newer.is_some() {
        if working {
            ui.next_flags(ST_DISABLED);
        }
        ui.next_icon(IC_DOWNLOAD);
        if ui.accent_button(id::SETTINGS + 3, t("Jetzt aktualisieren")) & EV_ACT != 0 {
            g.update_now(ui);
        }
    }
    if working {
        ui.next_flags(ST_DISABLED);
    }
    ui.next_icon(IC_REFRESH);
    if ui.button(id::SETTINGS + 4, t("Nach Updates suchen")) & EV_ACT != 0 {
        g.upd_found = None;
        g.check_updates(ui);
    }
    ui.box_end();

    let mut auto = g.upd_auto;
    if ui.switch(id::SETTINGS + 5, t("Beim Start nach Updates suchen"), &mut auto) & EV_ACT != 0 {
        g.set_update_auto(auto != 0);
    }
    let dir = bb_update::own_dir().unwrap_or_default();
    ui.text(TS_CAPTION | TX_SECONDARY | TX_WRAP, &tf("Neue Versionen kommen von {} und werden vor dem Einspielen gegen ihre Prüfsumme geprüft. Die Programme liegen in {}.", &[&format!("{}/releases", bb_core::REPOSITORY), &dir]));
    card_end(ui);

    if g.is_main() {
        card_begin(ui, t("Die anderen Geräte"));
        ui.text(TS_BODY | TX_SECONDARY | TX_WRAP, t("Unter „Geräte“ steht bei jedem Gerät seine Version. „Update einspielen“ dort lässt es die neue Version selbst holen und neu starten – ohne dass jemand hingehen muss."));
        card_end(ui);
    }
}

// ------------------------------------------------------------------------- questions

pub fn dialogs(g: &mut Gui, ui: &mut Ui) {
    let open = g.ask_leave.clone();
    if ui.dialog_begin(id::DIALOG, t("Kopplung aufheben?"), &open) {
        ui.next_size(420, 0);
        ui.text(TS_BODY | TX_WRAP, t("Dieses Gerät vergisst das Hauptgerät, und alles, was Build Balancer hier abgelegt hat, wird entfernt: Projekte, Protokolle, Einstellungen. Laufende Aufträge werden abgebrochen."));
        if ui.dialog_buttons(Some(t("Alles entfernen")), None, Some(t("Abbrechen"))) == DLG_PRIMARY {
            g.leave(ui);
        }
        ui.dialog_end();
    }
    let open = g.ask_unpair.clone();
    if ui.dialog_begin(id::DIALOG + 1, t("Gerät entkoppeln?"), &open) {
        ui.next_size(420, 0);
        ui.text(TS_BODY | TX_WRAP, &tf("„{}“ bekommt keine Aufträge mehr und wird am Relay entfernt. Was dort noch liegt, räumt das Gerät selbst weg (dort: „Kopplung aufheben“).", &[&g.unpair_name]));
        if ui.dialog_buttons(Some(t("Entkoppeln")), None, Some(t("Abbrechen"))) == DLG_PRIMARY {
            let name = g.unpair_name.clone();
            g.act(ui, move |ctx| ops::unpair(ctx, &name).map(|w| tf("„{}“ ist entkoppelt.", &[&w.name])));
        }
        ui.dialog_end();
    }
    let open = g.ask_close.clone();
    if ui.dialog_begin(id::DIALOG + 2, t("Beenden, obwohl Aufträge laufen?"), &open) {
        ui.next_size(420, 0);
        ui.text(TS_BODY | TX_WRAP, t("Auf diesem Gerät laufen gerade Aufträge. Beim Beenden werden sie abgebrochen; das Hauptgerät erfährt es."));
        if ui.dialog_buttons(Some(t("Beenden")), None, Some(t("Weiterlaufen lassen"))) == DLG_PRIMARY {
            g.close_anyway(ui);
        }
        ui.dialog_end();
    }
}

// ------------------------------------------------------------------------- demo

pub const DEMO_LOG: &str = "   Compiling kritzel v0.4.2\n   Compiling jflib-ui v0.2.1\n    Finished `release` profile [optimized] target(s) in 3m 41s\n";

pub fn demo_pairing() -> Pairing {
    ops::demo_pairing("relay.example.org:7700", "K7QF-2M9X-PD4T")
}

fn demo_status(name: &str, os: &str, cpu: (u32, u32, u32), ram_gib: (u64, u64), disk_gib: (u64, u64), tool_names: &[&str]) -> WorkerStatus {
    let mut s = WorkerStatus {
        time: now(),
        name: String::from(name),
        version: String::from(bb_core::VERSION),
        protocol: bb_core::PROTOCOL,
        os: String::from(os),
        arch: String::from("x86_64"),
        cpu_model: String::from(if os == "linux" { "AMD Ryzen 7 5800U" } else { "Intel Core i5-12400" }),
        cpu_total: cpu.2,
        ram_total: ram_gib.1 << 31,
        offer: state::offer(cpu.1, ram_gib.1, disk_gib.1),
        cpu_reserved: cpu.1 - cpu.0,
        disk_used: disk_gib.0 << 30,
        tools: state::tools(tool_names),
        ..WorkerStatus::default()
    };
    s.ram_reserved = (ram_gib.1 - ram_gib.0) << 30;
    s
}

pub fn demo(g: &mut Gui) {
    g.page = Page::Worker;
    // a newer version, so the settings show what they are for
    g.upd_found = Some(bb_update::Check { current: String::from(bb_core::VERSION), latest: String::from("0.2.0"), newer: true });
    g.lend = 1;
    g.wcfg = Some(Config { name: String::from("laptop"), id: String::from("w-demo000001"), main_name: String::from("werkbank"), cpu: 6, ram: 12 << 30, disk: 40 << 30, jobs: 4, priority: String::from("low"), keep_days: 14, ..Config::default() });
    let mut me = demo_status("laptop", "linux", (2, 6, 8), (8, 12), (11, 40), &["cargo", "rustc", "gcc", "g++", "clang", "cmake", "make", "python3", "git"]);
    me.running = alloc::vec![state::brief("j-k7m2x9pq4d", "kritzel", proto::ST_RUNNING, 4, "cargo build --release")];
    g.wstatus = Some(me.clone());
    let ws = alloc::vec![state::workspace("p-kritzel0001", "kritzel", 68, 0), state::workspace("p-jfword00001", "jfWord", 31, 3), state::workspace("p-altesdemo01", "netzlabor-alt", 12, 12)];
    g.projects = cleanup::group(&ws);
    me.workspaces = ws;

    g.main = Some(MainConfig { id: String::from("m-demo000001"), name: String::from("werkbank"), relay: String::from("https://relay.example.org:7700"), ..MainConfig::default() });
    let paired = |name: &str, os: &str| PairedWorker { id: format!("w-{}", name), name: String::from(name), os: String::from(os), arch: String::from("x86_64"), host: String::from(name), ..PairedWorker::default() };
    let mut tower = demo_status("tower", "windows", (8, 8, 12), (16, 16), (3, 60), &["cargo", "rustc", "cl", "msbuild", "cmake", "python", "node", "npm", "git"]);
    tower.workspaces = alloc::vec![state::workspace("p-kritzel0001", "kritzel", 29, 1)];
    g.fleet = alloc::vec![
        WorkerView { paired: paired("laptop", "linux"), admitted: true, online: true, idle_secs: 2, status: Some(me) },
        WorkerView { paired: paired("tower", "windows"), admitted: true, online: true, idle_secs: 4, status: Some(tower) },
        WorkerView { paired: paired("mini", "linux"), admitted: true, online: false, idle_secs: 5400, status: None },
    ];
    g.fleet_loaded = true;
    let job = |id: &str, worker: &str, state: &str, label: &str, cmd: &str, age: i64, ms: u64, exit: i64| JobRec {
        id: String::from(id),
        worker: String::from(worker),
        project: String::from("kritzel"),
        slot: String::from("main"),
        label: String::from(label),
        command: String::from(cmd),
        state: String::from(state),
        created: now() - age,
        duration_ms: ms,
        exit,
        ..JobRec::default()
    };
    g.jobs = alloc::vec![
        job("j-k7m2x9pq4d", "laptop", proto::ST_RUNNING, "linux-release", "cargo build --release", 95, 0, -1),
        job("j-4tq8hw2mzc", "tower", proto::ST_DONE, "windows-release", "cargo build --release", 900, 221_000, 0),
        job("j-9dfx3k7vbn", "laptop", proto::ST_FAILED, "tests", "cargo test --workspace", 4200, 512_000, 101),
        job("j-2c5m8r6jpa", "tower", proto::ST_KILLED, "", "python tools/big_bake.py", 90_000, 61_000, 1),
    ];
    g.jobs[3].error = String::from("memory limit: the jobs held 17.2 GiB of the 16.0 GiB this worker lends");
    g.job_sel = String::from("j-4tq8hw2mzc");
    g.job_log = String::from(DEMO_LOG);
    g.project_dir = String::from("C:/Projekte/kritzel");
    let task = |name: &str, run: &str, needs: &str, out: &str| Task { name: String::from(name), argv: bb_main::conf::split_args(run), needs: String::from(needs), outputs: bb_main::conf::split_list(out), ..Task::default() };
    g.project = Some(Project {
        dir: g.project_dir.clone(),
        id: String::from("p-kritzel0001"),
        name: String::from("kritzel"),
        tasks: alloc::vec![
            task("linux-release", "cargo build --release", "os=linux tool=cargo", "target/release/kritzel"),
            task("windows-release", "cargo build --release", "os=windows tool=cargo", "target/release/kritzel.exe"),
            task("tests", "cargo test --workspace", "cpu>=4", "")
        ],
        ..Project::default()
    });
    let _ = ago_text;
}
