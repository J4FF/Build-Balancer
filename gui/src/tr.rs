// tr -- the app's languages: German source texts, English from locales/ui.tsv
//
//   install()      the catalogue for the whole process, once, before the window
//   t("…")         a text in the user's language (the system's languages decide; headless
//                  runs stay German so screenshots match everywhere, --lang en picks English)
//   tf("… {} …", &[..])   the same with "{}" filled in order
//
// Messages that come from the shared libraries (errors of the relay, of a pairing, of a job)
// are English as the command-line programs print them.

use alloc::string::String;

use ui::i18n::{self, Catalog};

static UI_TSV: &str = include_str!("../locales/ui.tsv");

pub fn install() {
    i18n::set_catalog(Catalog::from_tsv(UI_TSV));
}

pub fn t(s: &'static str) -> &'static str {
    i18n::t(s)
}

pub fn tf(s: &'static str, args: &[&str]) -> String {
    i18n::tf(s, args)
}
